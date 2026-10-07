import {expect, test} from '@playwright/test';
import {MASTER, expectOnlyAstryxControls, login, resetPool} from './helpers';

test.beforeEach(async ({request}) => {
  await resetPool(request);
});

test('browser visiting / is redirected to the admin login page', async ({page}) => {
  await page.goto('/');
  await expect(page).toHaveURL(/\/admin\/$/);
  await expect(page.getByRole('heading', {name: '登录管理后台'})).toBeVisible();
  await expectOnlyAstryxControls(page);
});

test('/admin without trailing slash and deep links serve the SPA', async ({page}) => {
  await page.goto('/admin');
  await expect(page).toHaveURL(/\/admin\/$/);
  await page.goto('/admin/some/deep/link');
  await expect(page.getByRole('heading', {name: '登录管理后台'})).toBeVisible();
});

test('login validates input and rejects a wrong master key', async ({page}) => {
  await page.goto('/admin/');
  await page.getByRole('button', {name: '登录'}).click();
  await expect(page.getByText('请输入 Master Key')).toBeVisible();

  await page.getByLabel('Master Key').fill('definitely-not-the-master-key');
  await page.getByLabel('Master Key').press('Enter');
  await expect(page.getByText('Master Key 不正确')).toBeVisible();
  await expect(page.getByRole('heading', {name: 'Key 列表'})).toHaveCount(0);
  await expectOnlyAstryxControls(page);
});

test('login persists across reloads and logout returns to login', async ({page}) => {
  await login(page);
  await expect(page.getByText('还没有 Key')).toBeVisible();
  await expectOnlyAstryxControls(page);

  await page.reload();
  await expect(page.getByRole('heading', {name: 'Key 列表'})).toBeVisible();

  await page.getByRole('button', {name: '退出登录'}).click();
  await expect(page.getByRole('heading', {name: '登录管理后台'})).toBeVisible();
  expect(await page.evaluate(() => localStorage.getItem('tavily-lb.master-key'))).toBeNull();
});

test('a stale stored token is dropped and the login page is shown', async ({page}) => {
  await page.goto('/admin/');
  await page.evaluate(() => localStorage.setItem('tavily-lb.master-key', 'stale-token-stale-token'));
  await page.reload();
  await expect(page.getByRole('heading', {name: '登录管理后台'})).toBeVisible();

  await page.getByLabel('Master Key').fill(MASTER);
  await page.getByRole('button', {name: '登录'}).click();
  await expect(page.getByRole('heading', {name: 'Key 列表'})).toBeVisible();
});
