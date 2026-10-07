import {expect, type APIRequestContext, type Page} from '@playwright/test';

export const MASTER = 'e2e-master-key-0123456789';
export const MOCK = 'http://127.0.0.1:18281';
export const AUTH = {Authorization: `Bearer ${MASTER}`};

export interface SeedKey {
  key: string;
  alias?: string;
  quota?: number;
}

/** Removes every key so each test starts from an empty pool. */
export async function resetPool(request: APIRequestContext): Promise<void> {
  const res = await request.get('/api/keys', {headers: AUTH});
  expect(res.ok()).toBeTruthy();
  const {items} = (await res.json()) as {items: {id: number}[]};
  for (const k of items) {
    const del = await request.delete(`/api/keys/${k.id}`, {headers: AUTH});
    expect(del.ok()).toBeTruthy();
  }
  await request.post(`${MOCK}/__reset`);
}

export async function seedKeys(request: APIRequestContext, keys: SeedKey[]): Promise<void> {
  const text = keys.map((k) => [k.key, k.alias ?? '', k.quota ?? ''].join(',').replace(/,+$/, '')).join('\n');
  const res = await request.post('/api/keys', {headers: AUTH, data: {text}});
  expect(res.ok()).toBeTruthy();
  const body = (await res.json()) as {added: number; errors: unknown[]};
  expect(body.errors).toEqual([]);
  expect(body.added).toBe(keys.length);
}

export async function mockCalls(request: APIRequestContext): Promise<{method: string; path: string; key: string; body: unknown}[]> {
  return (await request.get(`${MOCK}/__calls`)).json();
}

export async function login(page: Page): Promise<void> {
  await page.goto('/admin/');
  await page.getByLabel('Master Key').fill(MASTER);
  await page.getByRole('button', {name: '登录'}).click();
  await expect(page.getByRole('heading', {name: 'Key 列表'})).toBeVisible();
}

/** Astryx mirrors toast text into an aria-live region; match only the visible toast. */
export function toast(page: Page, text: string | RegExp) {
  return page.getByRole('region', {name: 'Notifications'}).getByText(text);
}

export function statCard(page: Page, label: string) {
  return page
    .locator('.astryx-card')
    .filter({has: page.getByText(label, {exact: true}), hasNot: page.locator('table')});
}

export function keyRow(page: Page, alias: string) {
  return page.getByRole('row').filter({has: page.getByText(alias, {exact: true})});
}

export async function rowAction(page: Page, alias: string, action: string): Promise<void> {
  await page.getByRole('button', {name: `${alias} 的操作`}).click();
  await page.getByRole('menuitem', {name: action}).click();
}

/**
 * Asserts that every interactive form control on the page is rendered by an Astryx
 * component: the control itself, or a close ancestor, must carry an Astryx component
 * class. Generic layout wrappers (stack/layout/card/grid/...) do not count, otherwise any
 * raw <button> dropped into a VStack would pass.
 */
export async function expectOnlyAstryxControls(page: Page): Promise<void> {
  const offenders = await page.evaluate(() => {
    const layout = /^astryx-(stack|hstack|vstack|layout|card|grid|center|section|theme|text|heading)(-|$)/;
    const isAstryxControl = (el: Element) =>
      Array.from(el.classList).some((c) => c.startsWith('astryx-') && !layout.test(c));
    const bad: string[] = [];
    for (const el of Array.from(document.querySelectorAll('button, input, select, textarea, [role="button"]'))) {
      let n: Element | null = el;
      let ok = false;
      for (let depth = 0; depth < 4 && n; depth++, n = n.parentElement) {
        if (isAstryxControl(n)) {
          ok = true;
          break;
        }
      }
      if (!ok) bad.push(el.outerHTML.slice(0, 160));
    }
    return {bad, total: document.querySelectorAll('button, input, select, textarea').length};
  });
  expect(offenders.total, 'page should contain form controls to check').toBeGreaterThan(0);
  expect(offenders.bad, 'non-Astryx form controls found').toEqual([]);
}
