// Shared helpers for the live suite. Everything here talks to the real Tavily API.
// Never log raw keys: compare them with boolean assertions only.
import {readFileSync} from 'node:fs';
import {expect, type APIRequestContext, type Page} from '@playwright/test';

export const MASTER = process.env.LIVE_MASTER_KEY ?? 'live-e2e-master-key-0123456789';
export const BASE = process.env.LIVE_BASE_URL ?? 'http://127.0.0.1:18390';
export const AUTH = {Authorization: `Bearer ${MASTER}`};

export function realKeys(): string[] {
  const file = process.env.TAVILY_KEYS_FILE;
  if (!file) throw new Error('set TAVILY_KEYS_FILE to a file with real Tavily keys (one per line)');
  const keys = readFileSync(file, 'utf8')
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith('#'))
    .map((l) => l.split(',')[0].trim());
  if (keys.length === 0) throw new Error(`${file} contains no keys`);
  return keys;
}

export const alias = (i: number) => `real-${String(i + 1).padStart(2, '0')}`;

/** A well-formed key Tavily has never issued; the real API answers 401 for it. */
export function fakeKey(): string {
  const rand = Array.from({length: 32}, () => 'abcdefghijklmnopqrstuvwxyz0123456789'[Math.floor(Math.random() * 36)]).join('');
  return `tvly-dev-${rand}`;
}

export interface KeyItem {
  id: number;
  key: string;
  alias: string;
  total_quota: number;
  used_quota: number;
  remaining: number;
  active: boolean;
  invalid: boolean;
  status: string;
}

export async function listKeys(request: APIRequestContext): Promise<KeyItem[]> {
  const res = await request.get(`${BASE}/api/keys`, {headers: AUTH});
  expect(res.status()).toBe(200);
  return (await res.json()).items;
}

export async function overview(request: APIRequestContext) {
  const res = await request.get(`${BASE}/api/overview`, {headers: AUTH});
  expect(res.status()).toBe(200);
  return res.json();
}

export async function resetPool(request: APIRequestContext): Promise<void> {
  for (const k of await listKeys(request)) {
    expect((await request.delete(`${BASE}/api/keys/${k.id}`, {headers: AUTH})).status()).toBe(200);
  }
}

export async function importRealKeysViaApi(request: APIRequestContext): Promise<void> {
  const text = realKeys()
    .map((k, i) => `${k},${alias(i)}`)
    .join('\n');
  const res = await request.post(`${BASE}/api/keys`, {headers: AUTH, data: {text}});
  expect(res.status()).toBe(200);
  expect((await res.json()).errors).toEqual([]);
}

export async function syncAllAndWait(request: APIRequestContext) {
  const start = await request.post(`${BASE}/api/sync`, {headers: AUTH});
  expect([202, 409]).toContain(start.status());
  for (let i = 0; i < 120; i++) {
    const s = await (await request.get(`${BASE}/api/sync`, {headers: AUTH})).json();
    if (!s.running) return s;
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error('usage sync did not finish in 120s');
}

export async function login(page: Page): Promise<void> {
  await page.goto(`${BASE}/admin/`);
  await page.getByLabel('Master Key').fill(MASTER);
  await page.getByRole('button', {name: '登录'}).click();
  await expect(page.getByRole('heading', {name: 'Key 列表'})).toBeVisible();
}

export function toast(page: Page, text: string | RegExp) {
  return page.getByRole('region', {name: 'Notifications'}).getByText(text);
}

export function keyRow(page: Page, name: string) {
  return page.getByRole('row').filter({has: page.getByText(name, {exact: true})});
}

export function statCard(page: Page, label: string) {
  return page
    .locator('.astryx-card')
    .filter({has: page.getByText(label, {exact: true}), hasNot: page.locator('table')});
}

export async function rowAction(page: Page, name: string, action: string): Promise<void> {
  await page.getByRole('button', {name: `${name} 的操作`}).click();
  await page.getByRole('menuitem', {name: action}).click();
}

export {expectOnlyAstryxControls} from '../e2e/helpers';
