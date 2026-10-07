// Real end-to-end UI test: real binary, real Tavily API, the user's real keys,
// imported and managed exclusively through the Astryx UI.
import {readFile} from 'node:fs/promises';
import {expect, test, type Page} from '@playwright/test';
import {
  AUTH,
  BASE,
  MASTER,
  alias,
  expectOnlyAstryxControls,
  fakeKey,
  keyRow,
  listKeys,
  login,
  overview,
  realKeys,
  resetPool,
  rowAction,
  statCard,
  toast,
} from './live';

test.describe.configure({mode: 'serial'});

// Tests that call Tavily GET /usage; the live config runs them in one browser only.
const USAGE = {tag: '@usage'};
const keys = realKeys();
const FAKE = fakeKey();
const shot = (page: Page, name: string) =>
  page.screenshot({path: `test-results/live-shots/${test.info().project.name}-${name}.png`, fullPage: true});

test.beforeAll(async ({request}) => {
  await resetPool(request);
});

test('login page: empty and wrong master keys are rejected, the right one logs in', async ({page}) => {
  await page.goto(`${BASE}/`);
  await expect(page).toHaveURL(/\/admin\/$/);
  await expectOnlyAstryxControls(page);
  await shot(page, '01-login');

  await page.getByRole('button', {name: '登录'}).click();
  await expect(page.getByText('请输入 Master Key')).toBeVisible();
  await page.getByLabel('Master Key').fill('wrong-master-key-wrong');
  await page.getByLabel('Master Key').press('Enter');
  await expect(page.getByText('Master Key 不正确')).toBeVisible();

  await page.getByLabel('Master Key').fill(MASTER);
  await page.getByRole('button', {name: '登录'}).click();
  await expect(page.getByText('还没有 Key')).toBeVisible();
  await expectOnlyAstryxControls(page);
  await shot(page, '02-empty');
});

test(`import ${keys.length} real keys plus a fake key and a malformed line through the dialog`, async ({page, request}) => {
  await login(page);
  await page.getByRole('button', {name: '导入 Key'}).first().click();
  const dialog = page.getByRole('dialog', {name: '导入 Key'});
  await expectOnlyAstryxControls(page);

  const lines = [
    // Highest quota, so the proxy tries it first.
    `${FAKE},fake-revoked,99999`,
    ...keys.map((k, i) => `${k},${alias(i)}`),
    'this-is-not-a-key',
  ];
  await dialog.getByLabel('Key 列表').fill(lines.join('\n'));
  await dialog.getByRole('button', {name: '导入'}).click();
  await expect(dialog.getByText(`已导入 ${keys.length + 1} 个，重复 0 个，1 行有误`)).toBeVisible();
  await expect(dialog.getByText(`第 ${lines.length} 行：key must look like tvly-xxxx`)).toBeVisible();
  await dialog.getByRole('button', {name: '取消'}).click();
  await expect(dialog).toBeHidden();

  await expect(statCard(page, 'Key 总数')).toContainText(String(keys.length + 1));
  for (let i = 0; i < keys.length; i++) await expect(keyRow(page, alias(i))).toContainText('可用');

  const items = await listKeys(request);
  expect(items).toHaveLength(keys.length + 1);
  // The list only ever shows masked keys.
  expect(items.every((k) => k.key.includes('...'))).toBe(true);
  const html = await page.content();
  expect(keys.some((k) => html.includes(k)), 'raw key leaked into the page').toBe(false);
  await shot(page, '03-imported');
});

test('real 401 failover: the fake key is rejected by Tavily, marked invalid, and a real key answers', async ({page, request}) => {
  const res = await request.post(`${BASE}/search`, {
    headers: AUTH,
    data: {query: 'What is the capital of France?', max_results: 1, search_depth: 'basic'},
  });
  expect(res.status()).toBe(200);
  const body = await res.json();
  expect(body.results.length).toBeGreaterThan(0);
  expect(res.headers()['x-tavily-request-id']).toBeTruthy();

  const items = await listKeys(request);
  const fake = items.find((k) => k.alias === 'fake-revoked')!;
  expect(fake.invalid).toBe(true);
  expect(items.filter((k) => k.alias.startsWith('real-')).reduce((s, k) => s + k.used_quota, 0)).toBe(1);

  await login(page);
  await expect(keyRow(page, 'fake-revoked')).toContainText('失效');
  await expect(statCard(page, '禁用 / 失效')).toContainText('1');
  await expect(page.getByRole('button', {name: '清理失效 Key'})).toBeEnabled();
});

test('purge invalid keys through the confirmation dialog', async ({page, request}) => {
  await login(page);
  await page.getByRole('button', {name: '清理失效 Key'}).click();
  const confirm = page.getByRole('alertdialog', {name: '清理失效 Key？'});
  await expect(confirm).toContainText('1 个');
  await expectOnlyAstryxControls(page);
  await confirm.getByRole('button', {name: '全部删除'}).click();
  await expect(toast(page, '已删除 1 个失效 Key')).toBeVisible();
  await expect(keyRow(page, 'fake-revoked')).toHaveCount(0);
  await expect(statCard(page, 'Key 总数')).toContainText(String(keys.length));
  expect(await listKeys(request)).toHaveLength(keys.length);
});

test('sync all pulls real usage and plan limits from Tavily', USAGE, async ({page, request}) => {
  test.setTimeout(150_000);
  await login(page);
  await page.getByRole('button', {name: '同步全部用量'}).click();
  await expect(toast(page, '已开始同步全部 Key 的用量')).toBeVisible();
  await expect(page.getByText(new RegExp(`正在同步用量：\\d+ / ${keys.length}`))).toBeVisible();
  await shot(page, '04-syncing');
  await expect(toast(page, `用量同步完成：成功 ${keys.length}，失败 0`)).toBeVisible({timeout: 120_000});

  const items = await listKeys(request);
  // Tavily reports a plan limit for every real key.
  expect(items.every((k) => k.total_quota > 0 && !k.invalid)).toBe(true);
  const o = await overview(request);
  await expect(page.getByText(`已用 ${o.used_quota.toLocaleString('zh-CN')} / 总计 ${o.total_quota.toLocaleString('zh-CN')}`)).toBeVisible();
  await shot(page, '05-synced');
});

test('local accounting agrees with Tavily: route traffic to one key and compare with /usage', USAGE, async ({page, request}) => {
  test.setTimeout(240_000);
  await login(page);
  // Leave only real-01 enabled, using the row menus.
  for (let i = 1; i < keys.length; i++) {
    await rowAction(page, alias(i), '禁用');
    await expect(keyRow(page, alias(i))).toContainText('已禁用');
  }
  await expect(statCard(page, '可用')).toContainText('1');

  await rowAction(page, alias(0), '同步用量');
  await expect(toast(page, `${alias(0)} 用量已同步`)).toBeVisible();
  const before = (await listKeys(request)).find((k) => k.alias === alias(0))!.used_quota;

  for (const q of ['rust tokio runtime', 'axum web framework']) {
    const r = await request.post(`${BASE}/search`, {headers: AUTH, data: {query: q, max_results: 1, search_depth: 'basic'}});
    expect(r.status()).toBe(200);
  }
  await page.getByRole('button', {name: '刷新'}).click();
  await expect(keyRow(page, alias(0))).toContainText(`${(before + 2).toLocaleString('zh-CN')} /`);

  // Tavily's counter can lag behind; poll by syncing from the UI, sparingly because /usage is
  // rate limited for all keys together.
  let synced = -1;
  await page.waitForTimeout(10_000);
  for (let attempt = 0; attempt < 5 && synced !== before + 2; attempt++) {
    await rowAction(page, alias(0), '同步用量');
    await expect(toast(page, `${alias(0)} 用量已同步`).first()).toBeVisible();
    synced = (await listKeys(request)).find((k) => k.alias === alias(0))!.used_quota;
    if (synced !== before + 2) await page.waitForTimeout(15_000);
  }
  expect(synced, 'usage reported by Tavily after 2 basic searches').toBe(before + 2);
});

test('with every key disabled the proxy answers 503, and re-enabling restores service', async ({page, request}) => {
  test.setTimeout(120_000);
  await login(page);
  for (const k of (await listKeys(request)).filter((k) => k.active)) {
    await rowAction(page, k.alias, '禁用');
    await expect(keyRow(page, k.alias)).toContainText('已禁用');
  }
  await expect(statCard(page, '可用')).toContainText('0');
  const res = await request.post(`${BASE}/search`, {headers: AUTH, data: {query: 'x'}});
  expect(res.status()).toBe(503);
  expect((await res.json()).error).toBe('no_available_keys');

  for (let i = 0; i < keys.length; i++) {
    await rowAction(page, alias(i), '启用');
    await expect(keyRow(page, alias(i))).toContainText('可用');
  }
  await expect(statCard(page, '可用')).toContainText(String(keys.length));
});

test('edit dialog renames a key and changes its quota', async ({page, request}) => {
  await login(page);
  const original = (await listKeys(request)).find((k) => k.alias === alias(1))!;
  await rowAction(page, alias(1), '编辑');
  const dialog = page.getByRole('dialog', {name: '编辑 Key'});
  await expectOnlyAstryxControls(page);
  await shot(page, '06-edit');
  await dialog.getByLabel('别名').fill('renamed-key');
  const total = dialog.getByLabel('总额度');
  await total.fill('1234');
  await total.press('Tab');
  await dialog.getByRole('button', {name: '保存'}).click();
  await expect(dialog).toBeHidden();
  await expect(keyRow(page, 'renamed-key')).toContainText('/ 1,234');
  const edited = (await listKeys(request)).find((k) => k.id === original.id)!;
  expect(edited).toMatchObject({alias: 'renamed-key', total_quota: 1234, used_quota: original.used_quota});

  // Restore through the UI.
  await rowAction(page, 'renamed-key', '编辑');
  await dialog.getByLabel('别名').fill(alias(1));
  await total.fill(String(original.total_quota));
  await total.press('Tab');
  await dialog.getByRole('button', {name: '保存'}).click();
  await expect(keyRow(page, alias(1))).toContainText(`/ ${original.total_quota.toLocaleString('zh-CN')}`);
});

test('copy full key writes the real key to the clipboard', async ({page, browserName}) => {
  if (browserName === 'firefox') {
    // Firefox cannot grant clipboard-read to automation; capture the write instead.
    await page.addInitScript(() => {
      const w = window as unknown as {__copied: string[]};
      w.__copied = [];
      navigator.clipboard.writeText = async (t: string) => {
        w.__copied.push(t);
      };
    });
  }
  await login(page);
  await rowAction(page, alias(2), '复制完整 Key');
  await expect(toast(page, '已复制到剪贴板')).toBeVisible();
  const copied =
    browserName === 'firefox'
      ? await page.evaluate(() => (window as unknown as {__copied: string[]}).__copied[0])
      : await page.evaluate(() => navigator.clipboard.readText());
  expect(copied === keys[2], 'clipboard holds the real key for real-03').toBe(true);
});

test('reset usage, then sync restores the value Tavily reports', USAGE, async ({page, request}) => {
  await login(page);
  const before = (await listKeys(request)).find((k) => k.alias === alias(0))!;
  expect(before.used_quota).toBeGreaterThan(0);
  await rowAction(page, alias(0), '重置已用额度');
  const confirm = page.getByRole('alertdialog', {name: '重置已用额度？'});
  await expectOnlyAstryxControls(page);
  await confirm.getByRole('button', {name: '重置'}).click();
  await expect(confirm).toBeHidden();
  await expect(keyRow(page, alias(0))).toContainText(`0 / ${before.total_quota.toLocaleString('zh-CN')}`);

  await rowAction(page, alias(0), '同步用量');
  await expect(toast(page, `${alias(0)} 用量已同步`)).toBeVisible();
  await expect(keyRow(page, alias(0))).toContainText(`${before.used_quota.toLocaleString('zh-CN')} /`);
});

test('export downloads exactly the real keys', async ({page}) => {
  await login(page);
  const downloadPromise = page.waitForEvent('download');
  await page.getByRole('button', {name: '导出'}).click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toBe('tavily-keys.txt');
  const exported = (await readFile(await download.path(), 'utf8')).trim().split(/\r?\n/).sort();
  expect(exported.length).toBe(keys.length);
  expect(JSON.stringify(exported) === JSON.stringify([...keys].sort()), 'export equals the real key set').toBe(true);
});

test('delete a real key and import it again', async ({page, request}) => {
  await login(page);
  await rowAction(page, alias(9 % keys.length), '删除');
  const confirm = page.getByRole('alertdialog', {name: '删除 Key？'});
  await confirm.getByRole('button', {name: '删除'}).click();
  await expect(toast(page, '已删除')).toBeVisible();
  await expect(keyRow(page, alias(9 % keys.length))).toHaveCount(0);
  expect(await listKeys(request)).toHaveLength(keys.length - 1);

  await page.getByRole('button', {name: '导入 Key'}).first().click();
  const dialog = page.getByRole('dialog', {name: '导入 Key'});
  await dialog.getByLabel('Key 列表').fill(keys.map((k, i) => `${k},${alias(i)}`).join('\n'));
  await dialog.getByRole('button', {name: '导入'}).click();
  await expect(dialog).toBeHidden();
  await expect(toast(page, `导入 1 个，重复 ${keys.length - 1} 个`)).toBeVisible();
  await expect(keyRow(page, alias(9 % keys.length))).toContainText('可用');
  expect(await listKeys(request)).toHaveLength(keys.length);
  await shot(page, '07-final');
});

test('logout clears the session', async ({page}) => {
  await login(page);
  await page.getByRole('button', {name: '退出登录'}).click();
  await expect(page.getByRole('heading', {name: '登录管理后台'})).toBeVisible();
  await page.reload();
  await expect(page.getByRole('heading', {name: '登录管理后台'})).toBeVisible();
});
