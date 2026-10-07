// Process lifecycle against real Tavily: each test runs its own tavily-lb process and state file.
import {expect, test, type APIRequestContext} from '@playwright/test';
import {expectOnlyAstryxControls, realKeys} from './live';
import {ctrlC, freshDir, hardKill, readState, startServer, writeState, type Server} from './proc';

test.describe.configure({mode: 'serial'});

const KEYS_FILE = process.env.TAVILY_KEYS_FILE!;
const MASTER = 'ops-live-master-key-0123456789';
const keys = realKeys();
let running: Server[] = [];

async function start(opts: Parameters<typeof startServer>[0]): Promise<Server> {
  const s = await startServer(opts);
  running.push(s);
  return s;
}

test.afterEach(async () => {
  for (const s of running) {
    if (s.child.exitCode === null) await hardKill(s).catch(() => {});
  }
  running = [];
});

const api = (request: APIRequestContext, s: Server, key: string) => ({
  get: (path: string) => request.get(`${s.url}${path}`, {headers: {Authorization: `Bearer ${key}`}}),
  search: (query: string) =>
    request.post(`${s.url}/search`, {
      headers: {Authorization: `Bearer ${key}`},
      data: {query, max_results: 1, search_depth: 'basic'},
      timeout: 60_000,
    }),
});
const usedSum = (s: Server) => readState(s).keys.reduce((a, k) => a + k.used_quota, 0);
const localMonth = () => {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}`;
};

test('generated master key: logged, shown as a warning in the UI, kept across a real Ctrl+C restart', async ({page, request}) => {
  test.setTimeout(180_000);
  const dir = freshDir('ops-generated');
  // A long flush interval proves that only the shutdown hook can have written the usage.
  const config = 'flush_interval = "1h"\n';
  const env = {MASTER_KEY: undefined, KEYS_FILE};
  const s1 = await start({dir, port: 18401, config, env});

  const logged = /generated a new master key.*master_key=(\S+)/.exec(s1.log())?.[1];
  expect(logged, 'generated master key is printed in the log').toBeTruthy();
  const master = logged!;
  expect(readState(s1).master_key).toBe(master);
  expect(readState(s1).keys).toHaveLength(keys.length);

  const a1 = api(request, s1, master);
  expect((await (await a1.get('/api/settings')).json()).master_key_source).toBe('generated');
  expect((await (await a1.get('/api/overview')).json()).key_count).toBe(keys.length);

  await page.goto(`${s1.url}/admin/`);
  await page.getByLabel('Master Key').fill(master);
  await page.getByRole('button', {name: '登录'}).click();
  await expect(page.getByText('Master Key 为自动生成')).toBeVisible();
  await expectOnlyAstryxControls(page);
  await page.screenshot({path: 'test-results/live-shots/ops-generated-key-banner.png', fullPage: true});

  expect((await a1.search('graceful shutdown test')).status()).toBe(200);
  expect(usedSum(s1), 'usage is not flushed yet').toBe(0);

  expect(await ctrlC(s1), 'exit code after Ctrl+C').toBe(0);
  expect(s1.log()).toContain('shutting down');
  expect(s1.log()).toContain('state saved, bye');
  expect(usedSum(s1), 'Ctrl+C saved the in-memory usage').toBe(1);

  // Restart on the same state file; the keys file is imported again and must not duplicate.
  const s2 = await start({dir, port: 18402, config, env});
  expect(s2.log()).toContain('using master key from state file');
  const a2 = api(request, s2, master);
  expect((await (await a2.get('/api/settings')).json()).master_key_source).toBe('state_file');
  const o = await (await a2.get('/api/overview')).json();
  expect(o).toMatchObject({key_count: keys.length, used_quota: 1});

  await page.goto(`${s2.url}/admin/`);
  await page.getByLabel('Master Key').fill(master);
  await page.getByRole('button', {name: '登录'}).click();
  await expect(page.getByRole('heading', {name: 'Key 列表'})).toBeVisible();
  await expect(page.getByText('Master Key 为自动生成')).toHaveCount(0);

  // Ctrl+Break must save too.
  expect((await a2.search('ctrl break test')).status()).toBe(200);
  expect(await ctrlC(s2, 'break'), 'exit code after Ctrl+Break').toBe(0);
  expect(s2.log()).toContain('state saved, bye');
  expect(usedSum(s2)).toBe(2);
});

test('periodic flush writes usage within flush_interval and survives a hard kill', async ({request}) => {
  test.setTimeout(120_000);
  const dir = freshDir('ops-flush');
  const config = 'flush_interval = "2s"\n';
  const env = {MASTER_KEY: MASTER, KEYS_FILE};
  const s1 = await start({dir, port: 18403, config, env});
  expect(usedSum(s1)).toBe(0);
  expect((await api(request, s1, MASTER).search('periodic flush test')).status()).toBe(200);

  const t0 = Date.now();
  await expect.poll(() => usedSum(s1), {timeout: 8_000, intervals: [250]}).toBe(1);
  test.info().annotations.push({type: 'flush', description: `state file updated ${Date.now() - t0} ms after the request`});

  await hardKill(s1);
  expect(s1.log()).not.toContain('state saved, bye');
  const s2 = await start({dir, port: 18404, config, env});
  expect((await (await api(request, s2, MASTER).get('/api/overview')).json()).used_quota).toBe(1);
});

test('monthly reset runs on startup after downtime across a month boundary, and can be disabled', async ({request}) => {
  const dir = freshDir('ops-month');
  const env = {MASTER_KEY: MASTER, KEYS_FILE};
  const s1 = await start({dir, port: 18405, env});
  expect(await ctrlC(s1)).toBe(0);
  const state = readState(s1);
  expect(state.month).toBe(localMonth());

  const stale = {...state, month: '2000-01', keys: state.keys.map((k) => ({...k, used_quota: 123}))};
  writeState(s1, stale);
  const s2 = await start({dir, port: 18406, env});
  expect(s2.log()).toContain('new month since last run');
  const items = (await (await api(request, s2, MASTER).get('/api/keys')).json()).items as {used_quota: number}[];
  expect(items.map((k) => k.used_quota)).toEqual(Array(keys.length).fill(0));
  expect(readState(s2).month).toBe(localMonth());
  expect(await ctrlC(s2)).toBe(0);

  writeState(s2, stale);
  const s3 = await start({dir, port: 18407, env: {...env, MONTHLY_RESET: 'false', ADMIN_UI: 'false', MCP_ENABLED: 'false'}});
  const a3 = api(request, s3, MASTER);
  expect((await (await a3.get('/api/overview')).json()).used_quota).toBe(123 * keys.length);
  // With ADMIN_UI / MCP off these paths fall through to the authenticated proxy like any other path.
  expect((await request.get(`${s3.url}/admin/`)).status()).toBe(401);
  const admin = await a3.get('/admin/');
  expect(admin.status()).not.toBe(200);
  expect(await admin.text()).not.toContain('id="root"');
  const mcp = await request.post(`${s3.url}/mcp`, {
    headers: {Authorization: `Bearer ${MASTER}`},
    data: {jsonrpc: '2.0', id: 1, method: 'ping'},
  });
  expect(await mcp.text()).not.toContain('"jsonrpc"');
  test.info().annotations.push({type: 'disabled-features', description: `/admin/ -> upstream ${admin.status()}, /mcp -> upstream ${mcp.status()}`});
  console.log(`disabled features: /admin/ -> upstream ${admin.status()}, /mcp -> upstream ${mcp.status()}`);
});

test('scheduled usage sync runs on start and again after the interval, matching Tavily', async ({request}) => {
  test.setTimeout(200_000);
  const dir = freshDir('ops-sync');
  const s = await start({dir, port: 18408, env: {MASTER_KEY: MASTER, KEYS_FILE, USAGE_SYNC_ENABLED: 'true', USAGE_SYNC_INTERVAL: '60s'}});
  const a = api(request, s, MASTER);
  type Sync = {running: boolean; succeeded: number; failed: number; started_at: string | null; finished_at: string | null};
  const status = async () => (await (await a.get('/api/sync')).json()) as Sync;

  await expect.poll(async () => (await status()).finished_at, {timeout: 60_000, intervals: [1000]}).not.toBeNull();
  const first = await status();
  expect(first).toMatchObject({succeeded: keys.length, failed: 0});

  // Compare with Tavily directly, key by key (GET /usage costs no credits).
  const items = (await (await a.get('/api/keys')).json()).items as {id: number; used_quota: number; total_quota: number}[];
  for (const [i, item] of items.entries()) {
    const raw = await (await a.get(`/api/keys/${item.id}/raw`)).json();
    const res = await request.get('https://api.tavily.com/usage', {headers: {Authorization: `Bearer ${raw.key}`}});
    expect(res.status()).toBe(200);
    const u = await res.json();
    expect(item.used_quota, `key #${i + 1} usage`).toBe(Math.round(u.key.usage ?? 0));
    expect(item.total_quota, `key #${i + 1} limit`).toBe(Math.round(u.key.limit ?? u.account.plan_limit));
  }

  await expect
    .poll(async () => (await status()).started_at, {timeout: 90_000, intervals: [2000], message: 'second scheduled run'})
    .not.toBe(first.started_at);
  const startedGap = Date.parse((await status()).started_at!) - Date.parse(first.started_at!);
  expect(startedGap).toBeGreaterThanOrEqual(59_000);
  expect(startedGap).toBeLessThan(75_000);
});
