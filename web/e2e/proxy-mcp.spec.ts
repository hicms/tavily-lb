import {Client} from '@modelcontextprotocol/sdk/client/index.js';
import {StreamableHTTPClientTransport} from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import {expect, test} from '@playwright/test';
import {AUTH, MASTER, mockCalls, resetPool, seedKeys} from './helpers';

const BASE = 'http://127.0.0.1:18290';

test.beforeEach(async ({request}) => {
  await resetPool(request);
  await seedKeys(request, [{key: 'tvly-FAKE-TEST-mcp-key-1', alias: 'mcp', quota: 100}]);
});

test('REST proxy accepts Bearer, body and query master keys', async ({request}) => {
  const bearer = await request.post('/search', {headers: AUTH, data: {query: 'one'}});
  expect(bearer.status()).toBe(200);
  expect(bearer.headers()['x-proxy-request-id']).toBeTruthy();
  expect(bearer.headers()['x-tavily-request-id']).toMatch(/^mock-/);

  const body = await request.post('/search', {data: {query: 'two', api_key: MASTER}});
  expect(body.status()).toBe(200);

  const query = await request.get(`/usage?api_key=${MASTER}`);
  expect(query.status()).toBe(200);

  const denied = await request.post('/search', {data: {query: 'x', api_key: 'tvly-not-the-master'}});
  expect(denied.status()).toBe(401);

  const calls = await mockCalls(request);
  expect(calls.map((c) => c.path)).toEqual(['/search', '/search', '/usage']);
  expect(calls.every((c) => c.key === 'tvly-FAKE-TEST-mcp-key-1')).toBe(true);
  // The master key never reaches the upstream.
  expect(calls[1].body).toEqual({query: 'two'});

  const keys = await (await request.get('/api/keys', {headers: AUTH})).json();
  expect(keys.items[0].used_quota).toBe(2);
});

test('official MCP SDK client can list and call every tool', async ({request}) => {
  const client = new Client({name: 'e2e', version: '1.0.0'});
  const transport = new StreamableHTTPClientTransport(new URL(`${BASE}/mcp`), {
    requestInit: {headers: {Authorization: `Bearer ${MASTER}`}},
  });
  await client.connect(transport);
  try {
    expect(client.getServerVersion()?.name).toBe('tavily-lb-mcp');

    const {tools} = await client.listTools();
    expect(tools.map((t) => t.name)).toEqual([
      'tavily-search',
      'tavily-extract',
      'tavily-crawl',
      'tavily-map',
      'tavily-usage',
    ]);

    const search = await client.callTool({name: 'tavily-search', arguments: {query: 'mcp e2e', max_results: 1}});
    expect(search.isError).toBe(false);
    expect((search.structuredContent as {results: {title: string}[]}).results[0].title).toBe('Mock result for mcp e2e');

    const extract = await client.callTool({name: 'tavily-extract', arguments: {urls: ['https://example.com']}});
    expect((extract.structuredContent as {results: unknown[]}).results).toHaveLength(1);

    const crawl = await client.callTool({name: 'tavily-crawl', arguments: {url: 'https://example.com'}});
    expect(crawl.isError).toBe(false);

    const map = await client.callTool({name: 'tavily-map', arguments: {url: 'https://example.com'}});
    expect((map.structuredContent as {results: string[]}).results).toEqual(['https://example.com/a', 'https://example.com/b']);

    const usage = await client.callTool({name: 'tavily-usage', arguments: {}});
    expect((usage.structuredContent as {key: {usage: number; limit: number}}).key).toEqual({usage: 4, limit: 100});

    await expect(client.callTool({name: 'tavily-nope', arguments: {}})).rejects.toThrow(/Unknown tool/);
  } finally {
    await client.close();
  }

  const calls = await mockCalls(request);
  expect(calls.map((c) => c.path)).toEqual(['/search', '/extract', '/crawl', '/map']);
});

test('MCP rejects requests without the master key', async ({request}) => {
  const res = await request.post('/mcp', {data: {jsonrpc: '2.0', id: 1, method: 'tools/list'}});
  expect(res.status()).toBe(401);
  const get = await request.get('/mcp', {headers: AUTH});
  expect(get.status()).toBe(405);
});
