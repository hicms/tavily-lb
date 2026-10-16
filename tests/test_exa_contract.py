"""Validate real local HTTP responses against a snapshot of Exa's official schema.

Run after cargo build: python tests/test_exa_contract.py
Requires jsonschema; only contacts localhost mock services. Does not read local config/keys.
"""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

from jsonschema import Draft202012Validator, FormatChecker

ROOT = Path(__file__).resolve().parents[1]
MASTER = "contract-test-master-0123456789"


class MockTavily(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert self.headers["Authorization"] == "Bearer tvly-contract-test"
        assert self.headers.get("x-api-key") is None
        status = 200
        if self.path == "/search":
            if body["query"] == "upstream-error":
                status, output = 500, {"detail": "mock failure"}
            else:
                results = [] if body["query"] == "empty" else [{
                    "url": "https://example.com/article", "title": "Article",
                    "raw_content": "Full page 正文😀", "content": "Relevant excerpt",
                    "score": 0.9, "published_date": "2026-01-01",
                }]
                output = {"results": results, "request_id": "mock-search"}
        elif self.path == "/extract":
            results, failed = [], []
            for url in body["urls"]:
                if url.endswith("/fail"):
                    failed.append({"url": url, "error": "mock crawl failure"})
                else:
                    results.append({"url": url, "raw_content": (
                        "Relevant chunk" if "query" in body else "Full page 正文😀"
                    )})
            output = {"results": results, "failed_results": failed}
        else:
            raise AssertionError(f"Unexpected upstream path: {self.path}")
        payload = json.dumps(output).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def validator(spec, name):
    return Draft202012Validator({
        "$ref": f"#/components/schemas/{name}", "components": spec["components"],
    }, format_checker=FormatChecker())


def exchange(base, path, payload, authenticated=True):
    headers = {"Content-Type": "application/json"}
    if authenticated:
        headers["x-api-key"] = MASTER
    request = Request(base + path, data=json.dumps(payload).encode(), headers=headers)
    try:
        response = urlopen(request, timeout=5)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.headers, json.load(response)


def stop_process(process):
    if process.poll() is None:
        process.terminate()
        process.wait(timeout=5)


def run(binary, spec_path, sdk=False):
    spec = json.loads(spec_path.read_text(encoding="utf-8"))
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), MockTavily)
    thread = threading.Thread(target=upstream.serve_forever, daemon=True)
    thread.start()
    process = None
    try:
        with tempfile.TemporaryDirectory(prefix="tavily-exa-contract-") as temporary, ExitStack() as cleanup:
            directory = Path(temporary)
            with socket.socket() as reserve:
                reserve.bind(("127.0.0.1", 0))
                port = reserve.getsockname()[1]
            config = directory / "config.toml"
            config.write_text(
                f'listen = "127.0.0.1:{port}"\n'
                f'upstream_base_url = "http://127.0.0.1:{upstream.server_port}"\n'
                'data_file = "state.json"\nkeys_file = "keys.txt"\n'
                'admin_ui = false\nmcp = false\nmonthly_reset = false\n'
                'request_deadline = "5s"\nupstream_timeout = "2s"\n',
                encoding="utf-8",
            )
            (directory / "keys.txt").write_text("tvly-contract-test\n", encoding="utf-8")
            env = os.environ.copy()
            for key in (
                "CONFIG", "LISTEN_ADDR", "PORT", "TAVILY_BASE_URL", "UPSTREAM_TIMEOUT",
                "REQUEST_DEADLINE", "MAX_ATTEMPTS", "COOLDOWN_ON_429", "MASTER_KEY",
                "DATA_FILE", "KEYS_FILE", "ADMIN_UI", "MCP_ENABLED", "MONTHLY_RESET",
                "USAGE_SYNC_ENABLED", "USAGE_SYNC_INTERVAL",
            ):
                env.pop(key, None)
            env.update(MASTER_KEY=MASTER, USAGE_SYNC_ENABLED="false", RUST_LOG="error")
            with (directory / "server.log").open("w", encoding="utf-8") as log:
                process = subprocess.Popen(
                    [str(binary), "--config", str(config)], cwd=directory, env=env,
                    stdout=log, stderr=log,
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                )
                cleanup.callback(stop_process, process)
                base = f"http://127.0.0.1:{port}"
                for _ in range(100):
                    if process.poll() is not None:
                        raise RuntimeError((directory / "server.log").read_text())
                    try:
                        with urlopen(base + "/healthz", timeout=0.1):
                            break
                    except (URLError, TimeoutError):
                        time.sleep(0.05)
                else:
                    raise RuntimeError("Local test server did not become ready")
                count = validate_cases(base, spec)
                print(f"PASS: {count} real HTTP responses match Exa official schemas")
                if sdk:
                    validate_sdk(base)
                stop_process(process)
                process = None
    finally:
        if process is not None:
            stop_process(process)
        upstream.shutdown()
        upstream.server_close()
        thread.join(timeout=5)


def validate_cases(base, spec):
    searches = [
        {"query": "metadata"}, {"query": "empty"},
        {"query": "text", "contents": {"text": {"maxCharacters": 12}}},
        {"query": "highlights", "type": "fast", "contents": {"highlights": True}},
        {"query": "both", "contents": {"text": True, "highlights": {"query": "focus"}}},
        {"query": "nullable", "contents": None, "numResults": None},
    ]
    contents = [
        {"urls": ["https://example.com/article"]},
        {"ids": ["https://example.com/article", "https://example.com/fail"], "text": True},
        {"urls": ["https://example.com/fail"]},
        {"urls": ["https://example.com/article"], "text": False, "highlights": False},
        {"urls": ["https://example.com/article"], "highlights": {"query": "focus"}},
        {"urls": ["https://example.com/article"], "text": True, "highlights": True},
        {"urls": [f"https://example.com/{n}" for n in range(100)]},
    ]
    count = 0
    for path, requests, request_schema, response_schema in [
        ("/exa/search", searches, "SearchRequest", "SearchResultsResponse"),
        ("/exa/contents", contents, "ContentsRequest", "ContentsResponse"),
    ]:
        for payload in requests:
            validator(spec, request_schema).validate(payload)
            status, headers, response = exchange(base, path, payload)
            assert status == 200, (status, response)
            validator(spec, response_schema).validate(response)
            assert response["requestId"] == headers["x-request-id"]
            count += 1
    for payload, authenticated, expected in [
        ({"query": "q"}, False, 401), ({"query": "q", "type": "deep"}, True, 400),
        ({"query": "q", "numResults": 100}, True, 400), ({"query": "upstream-error"}, True, 502),
    ]:
        status, headers, response = exchange(base, "/exa/search", payload, authenticated)
        assert status == expected, (status, response)
        validator(spec, "ErrorResponse").validate(response)
        assert response["requestId"] == headers["x-request-id"]
        count += 1
    return count


def validate_sdk(base):
    from importlib.metadata import version
    from exa_py import Exa

    client = Exa(api_key=MASTER, base_url=base + "/exa")
    result = client.search("sdk highlights", num_results=3, contents={"highlights": True})
    assert result.results[0].highlights == ["Relevant excerpt"]
    full = client.search("sdk text", contents={"text": {"maxCharacters": 4}})
    assert full.results[0].text == "Full"
    metadata = client.search("sdk metadata", contents=False)
    assert metadata.results[0].url == "https://example.com/article"
    pages = client.get_contents(result.results, text=True)
    assert pages.results[0].text == "Full page 正文😀"
    excerpts = client.get_contents(["https://example.com/article"], highlights={"query": "focus"})
    assert excerpts.results[0].highlights == ["Relevant chunk"]
    combined = client.get_contents(["https://example.com/article"], text=True, highlights=True)
    assert combined.results[0].text == "Full page 正文😀"
    assert combined.results[0].highlights == ["Relevant chunk"]
    print(f"PASS: 6 official exa-py {version('exa-py')} SDK calls and response parsing")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug" / (
        "tavily-lb.exe" if os.name == "nt" else "tavily-lb"
    ))
    parser.add_argument("--spec", type=Path, default=ROOT / "tests/fixtures/exa-contract.json")
    parser.add_argument("--sdk", action="store_true", help="Also exercise an installed official exa-py SDK")
    args = parser.parse_args()
    run(args.binary.resolve(), args.spec.resolve(), args.sdk)
