// Real REST + MCP tests against api.tavily.com through the proxy, using real clients.
import {execFile, spawn} from 'node:child_process';
import {existsSync} from 'node:fs';
import {mkdtemp, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {dirname, join} from 'node:path';
import {promisify} from 'node:util';
import {Client} from '@modelcontextprotocol/sdk/client/index.js';
import {StreamableHTTPClientTransport} from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import {expect, test} from '@playwright/test';
import {AUTH, BASE, MASTER, importRealKeysViaApi, listKeys, overview, realKeys, resetPool} from './live';

const run = promisify(execFile);
test.describe.configure({mode: 'serial'});

/** Runs the Claude CLI with stdin closed (otherwise it waits for piped input) and a hard timeout. */
function runClaude(bin: string, args: string[], cwd: string, timeoutMs: number): Promise<string> {
  return new Promise((resolve, reject) => {
    const child = spawn(bin, args, {cwd, stdio: ['ignore', 'pipe', 'pipe']});
    let out = '';
    let err = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (err += d));
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`no answer within ${timeoutMs / 1000}s`));
    }, timeoutMs);
    child.on('error', (e) => {
      clearTimeout(timer);
      reject(e);
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      if (code === 0) resolve(out);
      else reject(new Error(`exit ${code}: ${err.slice(0, 300)}`));
    });
  });
}

const usedTotal = async (request: Parameters<typeof listKeys>[0]) =>
  (await listKeys(request)).reduce((s, k) => s + k.used_quota, 0);

test.beforeAll(async ({request}) => {
  await resetPool(request);
  await importRealKeysViaApi(request);
});

test('REST: search, extract, map, crawl and usage return real Tavily data', async ({request}) => {
  test.setTimeout(180_000);
  const post = (path: string, data: object) => request.post(`${BASE}${path}`, {headers: AUTH, data, timeout: 120_000});

  const search = await post('/search', {query: 'Who created the Rust programming language?', max_results: 2, include_answer: true});
  expect(search.status()).toBe(200);
  const s = await search.json();
  expect(s.results.length).toBeGreaterThan(0);
  expect(typeof s.answer).toBe('string');
  expect(s.results[0].url).toMatch(/^https?:\/\//);
  expect(search.headers()['x-proxy-request-id']).toMatch(/^[0-9a-f-]{36}$/);
  expect(search.headers()['x-tavily-request-id']).toBe(s.request_id);

  const extract = await post('/extract', {urls: ['https://www.rust-lang.org/']});
  expect(extract.status()).toBe(200);
  const e = await extract.json();
  expect(e.results[0].url).toContain('rust-lang.org');
  expect(e.results[0].raw_content.length).toBeGreaterThan(200);

  const map = await post('/map', {url: 'https://docs.tavily.com', max_depth: 1, limit: 5});
  expect(map.status()).toBe(200);
  const m = await map.json();
  expect(m.results.length).toBeGreaterThan(0);
  expect(m.results.every((u: string) => u.startsWith('https://docs.tavily.com'))).toBe(true);

  const crawl = await post('/crawl', {url: 'https://docs.tavily.com', max_depth: 1, limit: 2});
  expect(crawl.status()).toBe(200);
  expect((await crawl.json()).results.length).toBeGreaterThan(0);

  const usage = await request.get(`${BASE}/usage`, {headers: AUTH});
  expect(usage.status()).toBe(200);
  const u = await usage.json();
  expect(typeof u.key.usage).toBe('number');
  expect(u.account.plan_limit).toBeGreaterThan(0);

  // Four successful POSTs were counted, the GET was not.
  expect(await usedTotal(request)).toBe(4);
});

test('REST: every master-key location works and the key never reaches Tavily', async ({request}) => {
  const before = await usedTotal(request);
  const q = {query: 'tavily search api', max_results: 1, search_depth: 'basic'};
  const variants = [
    request.post(`${BASE}/search`, {headers: AUTH, data: q}),
    request.post(`${BASE}/search`, {data: {...q, api_key: MASTER}}),
    request.post(`${BASE}/search`, {data: {...q, apiKey: MASTER}}),
    request.post(`${BASE}/search?api_key=${MASTER}`, {data: q}),
    request.post(`${BASE}/search?apiKey=${MASTER}`, {data: q}),
  ];
  for (const res of await Promise.all(variants)) expect(res.status()).toBe(200);
  // Had the master key been forwarded, Tavily would have answered 401 and keys would be invalid.
  const items = await listKeys(request);
  expect(items.some((k) => k.invalid)).toBe(false);
  expect(await usedTotal(request)).toBe(before + 5);

  for (const res of await Promise.all([
    request.post(`${BASE}/search`, {data: q}),
    request.post(`${BASE}/search`, {headers: {Authorization: 'Bearer nope-nope-nope'}, data: q}),
    request.post(`${BASE}/search`, {data: {...q, api_key: realKeys()[0]}}),
  ])) {
    expect(res.status()).toBe(401);
    expect((await res.json()).error).toBe('unauthorized');
  }
  expect(await usedTotal(request)).toBe(before + 5);
});

test('REST: upstream validation errors are passed through untouched', async ({request}) => {
  const res = await request.post(`${BASE}/search`, {headers: AUTH, data: {max_results: 1}});
  // Tavily answers a missing query with 422 (FastAPI validation).
  expect(res.status()).toBe(422);
  expect(JSON.stringify(await res.json())).toMatch(/query/i);
  expect((await listKeys(request)).some((k) => k.invalid)).toBe(false);
});

test('REST: a body over max_body_bytes is rejected with 413 before any upstream call', async ({request}) => {
  const before = await usedTotal(request);
  const res = await request.post(`${BASE}/search`, {
    headers: {...AUTH, 'content-type': 'application/json'},
    data: JSON.stringify({query: 'x', pad: 'a'.repeat(11 * 1024 * 1024)}),
  });
  expect(res.status()).toBe(413);
  expect((await res.json()).error).toBe('payload_too_large');
  expect(await usedTotal(request)).toBe(before);
});

test('REST: 20 concurrent real searches all succeed and are counted exactly', async ({request}) => {
  test.setTimeout(180_000);
  const before = await listKeys(request);
  const results = await Promise.all(
    Array.from({length: 20}, (_, i) =>
      request.post(`${BASE}/search`, {
        headers: AUTH,
        data: {query: `concurrency test ${i} rust`, max_results: 1, search_depth: 'basic'},
        timeout: 120_000,
      }),
    ),
  );
  expect(results.map((r) => r.status())).toEqual(Array(20).fill(200));
  const after = await listKeys(request);
  const delta = after.map((k) => k.used_quota - before.find((b) => b.id === k.id)!.used_quota);
  expect(delta.reduce((a, b) => a + b, 0)).toBe(20);
  const spread = delta.filter((d) => d > 0).length;
  test.info().annotations.push({type: 'distribution', description: `20 requests over ${spread} keys: ${JSON.stringify(delta)}`});
  console.log(`concurrency distribution: ${spread} keys used, per-key deltas ${JSON.stringify(delta)}`);
});

test('MCP: the official SDK client calls every tool against real Tavily', async ({request}) => {
  test.setTimeout(240_000);
  const client = new Client({name: 'live-e2e', version: '1.0.0'});
  await client.connect(
    new StreamableHTTPClientTransport(new URL(`${BASE}/mcp`), {requestInit: {headers: {Authorization: `Bearer ${MASTER}`}}}),
  );
  try {
    const {tools} = await client.listTools();
    expect(tools.map((t) => t.name).sort()).toEqual(['tavily-crawl', 'tavily-extract', 'tavily-map', 'tavily-search', 'tavily-usage']);
    const before = await usedTotal(request);

    const search = await client.callTool({name: 'tavily-search', arguments: {query: 'Model Context Protocol', max_results: 2}});
    expect(search.isError).toBe(false);
    const sc = search.structuredContent as {results: {url: string}[]};
    expect(sc.results.length).toBeGreaterThan(0);
    expect((search.content as {type: string; text: string}[])[0].text).toContain(sc.results[0].url);

    const extract = await client.callTool({name: 'tavily-extract', arguments: {urls: ['https://modelcontextprotocol.io/']}});
    expect(extract.isError).toBe(false);
    expect((extract.structuredContent as {results: {raw_content: string}[]}).results[0].raw_content.length).toBeGreaterThan(100);

    const map = await client.callTool({name: 'tavily-map', arguments: {url: 'https://docs.tavily.com', limit: 3}});
    expect(map.isError).toBe(false);
    expect((map.structuredContent as {results: string[]}).results.length).toBeGreaterThan(0);

    const crawl = await client.callTool({name: 'tavily-crawl', arguments: {url: 'https://docs.tavily.com', limit: 1}});
    expect(crawl.isError).toBe(false);

    expect(await usedTotal(request)).toBe(before + 4);
    const usage = await client.callTool({name: 'tavily-usage', arguments: {}});
    const o = await overview(request);
    expect((usage.structuredContent as {key: {usage: number; limit: number}}).key).toEqual({usage: o.used_quota, limit: o.total_quota});

    const bad = await client.callTool({name: 'tavily-extract', arguments: {urls: ['not a url']}});
    expect(bad.isError).toBe(true);
  } finally {
    await client.close();
  }
});

test('MCP: the MCP Inspector CLI (third-party client) lists and calls tools', async ({request}) => {
  test.setTimeout(300_000);
  const base = ['-y', '@modelcontextprotocol/inspector', '--cli', `${BASE}/mcp`, '--transport', 'http', '--header', `Authorization: Bearer ${MASTER}`];
  // npx.cmd needs a shell on Windows, and the shell would split the header argument; run npx-cli.js directly.
  const npxCli = join(dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npx-cli.js');
  const [cmd, pre] = existsSync(npxCli) ? [process.execPath, [npxCli]] : ['npx', []];
  const opts = {timeout: 240_000, maxBuffer: 20 * 1024 * 1024};
  const npx = (args: string[]) => run(cmd, [...pre, ...args], opts);

  const list = await npx([...base, '--method', 'tools/list']);
  const names = (JSON.parse(list.stdout).tools as {name: string}[]).map((t) => t.name);
  expect(names).toContain('tavily-search');

  const before = await usedTotal(request);
  const call = await npx([...base, '--method', 'tools/call', '--tool-name', 'tavily-search', '--tool-arg', 'query=inspector cli test', 'max_results=1']);
  const out = JSON.parse(call.stdout);
  expect(out.isError).toBeFalsy();
  expect(out.structuredContent.results.length).toBeGreaterThan(0);
  expect(await usedTotal(request)).toBe(before + 1);
});

test('MCP: Claude Code uses the server as an MCP tool provider', async ({request}) => {
  test.setTimeout(300_000);
  const claude = process.env.CLAUDE_BIN ?? 'claude';
  const dir = await mkdtemp(join(tmpdir(), 'tavily-lb-claude-'));
  try {
    await runClaude(claude, ['--version'], dir, 30_000);
  } catch {
    test.skip(true, 'Claude Code CLI not installed');
  }
  // Without this preflight a broken Claude setup looks like an MCP server hang.
  const ping = await runClaude(claude, ['-p', 'Reply with the single word: pong'], dir, 90_000).catch((e: Error) => e);
  test.skip(ping instanceof Error, `Claude Code itself does not answer a plain prompt: ${ping instanceof Error ? ping.message : ''}`);

  const config = join(dir, 'mcp.json');
  await writeFile(
    config,
    JSON.stringify({mcpServers: {tavily: {type: 'http', url: `${BASE}/mcp`, headers: {Authorization: `Bearer ${MASTER}`}}}}),
  );
  const before = await usedTotal(request);
  const stdout = await runClaude(
    claude,
    [
      '-p',
      'Use the tavily-search tool exactly once with query "Rust 2024 edition release" and max_results 1, then reply with only the URL of the first result.',
      '--mcp-config',
      config,
      '--strict-mcp-config',
      '--allowedTools',
      'mcp__tavily__tavily-search',
      '--output-format',
      'json',
    ],
    dir,
    240_000,
  );
  const out = JSON.parse(stdout);
  expect(out.is_error).toBe(false);
  expect(out.result).toMatch(/https?:\/\//);
  // The search went through our pool.
  expect(await usedTotal(request)).toBeGreaterThanOrEqual(before + 1);
  console.log(`claude code answered: ${out.result}`);
});
