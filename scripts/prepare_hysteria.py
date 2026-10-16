"""Import Clash Hysteria2 nodes; probe public IPs serially without any Tavily keys.

Requires Python 3, PyYAML, curl and the official Hysteria 2 client.
Generated files contain credentials: keep output inside the ignored data directory.
"""
import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import socket
import subprocess
import time

import yaml


def client_config(node, port):
    host = str(node["server"])
    if ":" in host:
        host = f"[{host}]"
    tls = {"sni": node.get("sni", node["server"]),
           "insecure": bool(node.get("skip-cert-verify", False))}
    if node.get("fingerprint"):
        tls["pinSHA256"] = node["fingerprint"]
    if node.get("alpn") not in (None, ["h3"]):
        raise ValueError("unsupported custom ALPN")
    config = {
        "server": f'{host}:{node["port"]}', "auth": str(node["password"]),
        "tls": tls, "lazy": True,
        "socks5": {"listen": f"127.0.0.1:{port}", "disableUDP": True},
    }
    if node.get("obfs"):
        if node["obfs"] != "salamander":
            raise ValueError("unsupported obfuscation")
        config["obfs"] = {"type": "salamander", "salamander": {"password": node["obfs-password"]}}
    # Keep server-controlled congestion defaults; API requests do not need speed-test bandwidth.
    return config


def probe(binary, config_file, port):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", port))
    child = subprocess.Popen(
        [str(binary), "client", "--disable-update-check", "--log-level", "error", "-c", str(config_file)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
    )
    try:
        ready = False
        for _ in range(100):
            if child.poll() is not None:
                raise RuntimeError("client exited")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                    ready = True
                    break
            except OSError:
                time.sleep(0.05)
        if not ready:
            raise RuntimeError("listener timeout")
        result = subprocess.run([
            "curl.exe" if os.name == "nt" else "curl",
            "--silent", "--show-error", "--fail", "--retry", "0",
            "--connect-timeout", "10", "--max-time", "20", "--noproxy", "",
            "--proxy", f"socks5h://127.0.0.1:{port}", "https://api.ipify.org",
        ], capture_output=True, text=True, timeout=25)
        if result.returncode:
            raise RuntimeError(f"IP probe failed (curl {result.returncode})")
        ip = ipaddress.ip_address(result.stdout.strip())
        if not ip.is_global:
            raise RuntimeError("probe returned a non-public IP")
        return str(ip)
    finally:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


def prepare(source, binary, output, start_port):
    source_data = yaml.safe_load(source.read_text(encoding="utf-8-sig"))
    nodes = [p for p in source_data.get("proxies", []) if p.get("type") == "hysteria2"]
    if not nodes:
        raise ValueError("No Hysteria2 nodes in YAML")
    output.mkdir(parents=True, exist_ok=True)
    config_path = output / "egress.toml"
    if config_path.exists():
        raise ValueError("Output already has egress.toml; use a new directory to preserve deployed routes")
    exits = set()
    selected = []
    report = []
    for index, node in enumerate(nodes):
        # ID depends on the endpoint, not its credentials or the subscription order.
        identity = f'{node["server"]}:{node["port"]}'
        node_id = "hy2-" + hashlib.sha256(identity.encode()).hexdigest()[:12]
        port = start_port + index
        path = output / f"{node_id}.json"
        if path.exists():
            report.append({"id": node_id, "status": "duplicate endpoint"})
            continue
        try:
            config = client_config(node, port)
            with path.open("x", encoding="utf-8") as handle:
                json.dump(config, handle, indent=2)
            ip = probe(binary, path, port)
            if ip in exits:
                status = "duplicate exit IP"
            else:
                exits.add(ip)
                selected.append({"id": node_id, "exit_ip": ip, "proxy_url": f"socks5h://127.0.0.1:{port}",
                                 "hysteria_config": path.resolve().as_posix()})
                status = "ready"
            report.append({"id": node_id, "status": status, "exit_ip": ip})
        except (ValueError, KeyError, OSError, RuntimeError, subprocess.TimeoutExpired) as exc:
            # Exceptions can contain credential-bearing input; print only a safe class name.
            report.append({"id": node_id, "status": "unavailable", "reason": type(exc).__name__})
        print(json.dumps(report[-1]), flush=True)
        time.sleep(2)
    (output / "probe-report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    if not selected:
        raise RuntimeError("No verified independent exits; no routing config was created")
    lines = ["[egress]", "hysteria_binary = " + json.dumps(binary.resolve().as_posix())]
    for node in selected:
        lines.append("\n[[egress.nodes]]")
        lines.extend(f"{k} = {json.dumps(v)}" for k, v in node.items())
    config_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"Prepared {len(selected)} verified independent exits; no Tavily API calls.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("data/hysteria/nodes"))
    parser.add_argument("--start-port", type=int, default=19080)
    args = parser.parse_args()
    prepare(args.source, args.binary, args.output, args.start_port)
