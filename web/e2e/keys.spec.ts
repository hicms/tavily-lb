import {readFile} from 'node:fs/promises';
import {expect, test} from '@playwright/test';
import {AUTH, expectOnlyAstryxControls, keyRow, login, mockCalls, resetPool, rowAction, seedKeys, statCard, toast} from './helpers';

test.beforeEach(async ({request}) => {
  await resetPool(request);
});

test('import dialog adds keys, reports bad lines and updates the overview', async ({page}) => {
  await login(page);
  await expect(page.getByText('还没有 Key')).toBeVisible();
  await page.getByRole('button', {name: '导入 Key'}).first().click();

  const dialog = page.getByRole('dialog', {name: '导入 Key'});
  await expect(dialog).toBeVisible();
  await expectOnlyAstryxControls(page);

  // Empty submit is rejected client-side.
  await dialog.getByRole('button', {name: '导入'}).click();
  await expect(dialog.getByText('请至少输入一个 Key')).toBeVisible();

  await dialog.getByLabel('Key 列表').fill(
    [
      '# comment line',
      'tvly-FAKE-TEST-alpha-1,alpha',
      'tvly-FAKE-TEST-beta-2,beta,500',
      'not-a-key',
      'tvly-FAKE-TEST-alpha-1',
    ].join('\n'),
  );
  const quota = dialog.getByLabel('默认总额度');
  await quota.fill('2000');
  await quota.press('Tab');
  await dialog.getByRole('button', {name: '导入'}).click();

  // Line 4 is invalid, so the dialog stays open with a warning listing it.
  await expect(dialog.getByText('已导入 2 个，重复 1 个，1 行有误')).toBeVisible();
  await expect(dialog.getByText(/第 4 行/)).toBeVisible();
  await dialog.getByRole('button', {name: '取消'}).click();
  await expect(dialog).toBeHidden();
  // The closed dialog stays mounted; it must not keep the pasted raw keys.
  expect(await page.content()).not.toContain('tvly-FAKE-TEST-alpha-1');

  await expect(keyRow(page, 'alpha')).toContainText('0 / 2,000');
  await expect(keyRow(page, 'beta')).toContainText('0 / 500');
  await expect(keyRow(page, 'alpha')).toContainText('可用');
  await expect(statCard(page, 'Key 总数')).toContainText('2');
  await expect(page.getByText('已用 0 / 总计 2,500，剩余 2,500')).toBeVisible();
  await expectOnlyAstryxControls(page);
});

test('a fully valid import closes the dialog and shows a toast', async ({page}) => {
  await login(page);
  await page.getByRole('button', {name: '导入 Key'}).first().click();
  const dialog = page.getByRole('dialog', {name: '导入 Key'});
  await dialog.getByLabel('Key 列表').fill('tvly-FAKE-TEST-solo-1,solo');
  await dialog.getByRole('button', {name: '导入'}).click();
  await expect(dialog).toBeHidden();
  expect(await page.content()).not.toContain('tvly-FAKE-TEST-solo-1');
  await expect(toast(page, '导入 1 个，重复 0 个')).toBeVisible();
  await expect(keyRow(page, 'solo')).toContainText('0 / 1,000');
});

test('edit dialog updates alias and quotas', async ({page, request}) => {
  await seedKeys(request, [{key: 'tvly-FAKE-TEST-edit-1', alias: 'old-name', quota: 1000}]);
  await login(page);

  await rowAction(page, 'old-name', '编辑');
  const dialog = page.getByRole('dialog', {name: '编辑 Key'});
  await expect(dialog).toBeVisible();
  await expectOnlyAstryxControls(page);

  await dialog.getByLabel('别名').fill('new-name');
  const total = dialog.getByLabel('总额度');
  await total.fill('300');
  await total.press('Tab');
  const used = dialog.getByLabel('已用额度');
  await used.fill('120');
  await used.press('Tab');
  await dialog.getByRole('button', {name: '保存'}).click();
  await expect(dialog).toBeHidden();

  const row = keyRow(page, 'new-name');
  await expect(row).toContainText('120 / 300');
  await expect(row).toContainText('180');
  await expect(keyRow(page, 'old-name')).toHaveCount(0);

  const res = await request.get('/api/keys', {headers: AUTH});
  const [item] = (await res.json()).items;
  expect(item).toMatchObject({alias: 'new-name', total_quota: 300, used_quota: 120, remaining: 180});
});

test('disable and enable a key from the row menu', async ({page, request}) => {
  await seedKeys(request, [{key: 'tvly-FAKE-TEST-toggle-1', alias: 'toggler'}]);
  await login(page);
  await page.getByRole('button', {name: 'toggler 的操作'}).click();
  await expectOnlyAstryxControls(page);
  await page.getByRole('menuitem', {name: '禁用'}).click();
  await expect(keyRow(page, 'toggler')).toContainText('已禁用');
  await expect(statCard(page, '禁用 / 失效')).toContainText('1');

  await rowAction(page, 'toggler', '启用');
  await expect(keyRow(page, 'toggler')).toContainText('可用');
  await expect(statCard(page, '禁用 / 失效')).toContainText('0');
});

test('copy full key puts the raw key on the clipboard', async ({page, request}) => {
  await seedKeys(request, [{key: 'tvly-FAKE-TEST-copy-secret-1', alias: 'copyme'}]);
  await login(page);
  await expect(keyRow(page, 'copyme')).not.toContainText('tvly-FAKE-TEST-copy-secret-1');
  await rowAction(page, 'copyme', '复制完整 Key');
  await expect(toast(page, '已复制到剪贴板')).toBeVisible();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('tvly-FAKE-TEST-copy-secret-1');
});

test('sync one key pulls usage from upstream', async ({page, request}) => {
  await seedKeys(request, [{key: 'tvly-FAKE-TEST-sync-1', alias: 'syncer', quota: 5000}]);
  await login(page);
  await rowAction(page, 'syncer', '同步用量');
  await expect(toast(page, 'syncer 用量已同步')).toBeVisible();
  await expect(keyRow(page, 'syncer')).toContainText('42 / 1,000');
  const calls = await mockCalls(request);
  expect(calls).toContainEqual(expect.objectContaining({method: 'GET', path: '/usage', key: 'tvly-FAKE-TEST-sync-1'}));
});

test('sync one revoked key shows an error and marks it invalid', async ({page, request}) => {
  await seedKeys(request, [{key: 'tvly-FAKE-TEST-revoked-1', alias: 'gone'}]);
  await login(page);
  await rowAction(page, 'gone', '同步用量');
  await expect(toast(page, /upstream status 401/)).toBeVisible();
  await expect(keyRow(page, 'gone')).toContainText('失效');
});

test('sync all shows progress, then a summary toast and failure banner', async ({page, request}) => {
  await seedKeys(request, [
    {key: 'tvly-FAKE-TEST-all-good-1', alias: 'good-1'},
    {key: 'tvly-FAKE-TEST-all-good-2', alias: 'good-2'},
    {key: 'tvly-FAKE-TEST-all-revoked-1', alias: 'bad-1'},
  ]);
  await login(page);
  await page.getByRole('button', {name: '同步全部用量'}).click();
  await expect(toast(page, '已开始同步全部 Key 的用量')).toBeVisible();
  await expect(page.getByText(/正在同步用量：\d+ \/ 3/)).toBeVisible();
  await expect(page.getByRole('button', {name: '同步中…'})).toBeDisabled();

  await expect(toast(page, '用量同步完成：成功 2，失败 1')).toBeVisible({timeout: 20_000});
  await expect(page.getByText('上次同步有 1 个 Key 失败')).toBeVisible();
  await expect(page.getByText(/bad-1：upstream status 401/)).toBeVisible();
  await expect(keyRow(page, 'good-1')).toContainText('42 / 1,000');
  await expect(keyRow(page, 'good-2')).toContainText('42 / 1,000');
  await expect(keyRow(page, 'bad-1')).toContainText('失效');
  await expect(page.getByRole('button', {name: '同步全部用量'})).toBeEnabled();
  await expectOnlyAstryxControls(page);
});

test('reset usage asks for confirmation and clears used quota', async ({page, request}) => {
  await seedKeys(request, [{key: 'tvly-FAKE-TEST-reset-1', alias: 'resetme'}]);
  const {items} = await (await request.get('/api/keys', {headers: AUTH})).json();
  await request.patch(`/api/keys/${items[0].id}`, {headers: AUTH, data: {used_quota: 77}});
  await login(page);
  await expect(keyRow(page, 'resetme')).toContainText('77 / 1,000');

  await rowAction(page, 'resetme', '重置已用额度');
  const confirm = page.getByRole('alertdialog', {name: '重置已用额度？'});
  await expect(confirm).toBeVisible();
  await expectOnlyAstryxControls(page);

  await confirm.getByRole('button', {name: '取消'}).click();
  await expect(confirm).toBeHidden();
  await expect(keyRow(page, 'resetme')).toContainText('77 / 1,000');

  await rowAction(page, 'resetme', '重置已用额度');
  await page.getByRole('alertdialog').getByRole('button', {name: '重置'}).click();
  await expect(page.getByRole('alertdialog')).toBeHidden();
  await expect(keyRow(page, 'resetme')).toContainText('0 / 1,000');
});

test('delete a key after confirmation', async ({page, request}) => {
  await seedKeys(request, [
    {key: 'tvly-FAKE-TEST-delete-1', alias: 'doomed'},
    {key: 'tvly-FAKE-TEST-delete-2', alias: 'survivor'},
  ]);
  await login(page);
  await rowAction(page, 'doomed', '删除');
  const confirm = page.getByRole('alertdialog', {name: '删除 Key？'});
  await expect(confirm).toContainText('doomed');
  await confirm.getByRole('button', {name: '删除'}).click();
  await expect(toast(page, '已删除')).toBeVisible();
  await expect(keyRow(page, 'doomed')).toHaveCount(0);
  await expect(keyRow(page, 'survivor')).toBeVisible();
  await expect(statCard(page, 'Key 总数')).toContainText('1');
});

test('proxy failures surface as invalid/cooling badges and invalid keys can be purged', async ({page, request}) => {
  // The revoked and rate-limited keys have the most remaining quota, so they are tried first.
  await seedKeys(request, [
    {key: 'tvly-FAKE-TEST-px-revoked-1', alias: 'revoked', quota: 9000},
    {key: 'tvly-FAKE-TEST-px-limited-1', alias: 'limited', quota: 8000},
    {key: 'tvly-FAKE-TEST-px-good-1', alias: 'healthy', quota: 1000},
  ]);
  const res = await request.post('/search', {headers: AUTH, data: {query: 'failover'}});
  expect(res.status()).toBe(200);
  expect((await res.json()).results[0].title).toBe('Mock result for failover');

  await login(page);
  await expect(keyRow(page, 'revoked')).toContainText('失效');
  await expect(keyRow(page, 'limited')).toContainText(/冷却中 \d+s/);
  await expect(keyRow(page, 'healthy')).toContainText('1 / 1,000');
  await expect(statCard(page, '冷却中')).toContainText('1');

  await page.getByRole('button', {name: '清理失效 Key'}).click();
  const confirm = page.getByRole('alertdialog', {name: '清理失效 Key？'});
  await expect(confirm).toContainText('1 个');
  await confirm.getByRole('button', {name: '全部删除'}).click();
  await expect(toast(page, '已删除 1 个失效 Key')).toBeVisible();
  await expect(keyRow(page, 'revoked')).toHaveCount(0);
  await expect(page.getByRole('button', {name: '清理失效 Key'})).toBeDisabled();
});

test('exhausted keys are shown as exhausted', async ({page, request}) => {
  await seedKeys(request, [
    {key: 'tvly-FAKE-TEST-exhausted-1', alias: 'drained', quota: 9000},
    {key: 'tvly-FAKE-TEST-ex-good-1', alias: 'fresh'},
  ]);
  const res = await request.post('/search', {headers: AUTH, data: {query: 'q'}});
  expect(res.status()).toBe(200);
  await login(page);
  await expect(keyRow(page, 'drained')).toContainText('已耗尽');
  await expect(statCard(page, '已耗尽')).toContainText('1');
});

test('export downloads every non-invalid key', async ({page, request}) => {
  await seedKeys(request, [
    {key: 'tvly-FAKE-TEST-export-1', alias: 'e1'},
    {key: 'tvly-FAKE-TEST-export-2', alias: 'e2'},
  ]);
  await login(page);
  const downloadPromise = page.waitForEvent('download');
  await page.getByRole('button', {name: '导出'}).click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toBe('tavily-keys.txt');
  const text = await readFile(await download.path(), 'utf8');
  expect(text.trim().split('\n').sort()).toEqual(['tvly-FAKE-TEST-export-1', 'tvly-FAKE-TEST-export-2']);
});

test('refresh button reloads data changed elsewhere', async ({page, request}) => {
  await login(page);
  await expect(page.getByText('还没有 Key')).toBeVisible();
  await seedKeys(request, [{key: 'tvly-FAKE-TEST-refresh-1', alias: 'appeared'}]);
  await page.getByRole('button', {name: '刷新'}).click();
  await expect(keyRow(page, 'appeared')).toBeVisible();
});
