const TOKEN_KEY = 'tavily-lb.master-key';
export const LOGOUT_EVENT = 'tavily-lb:logout';

export type KeyStatus = 'available' | 'cooling' | 'exhausted' | 'disabled' | 'invalid';

export interface KeyItem extends Record<string, unknown> {
  id: number;
  key: string;
  alias: string;
  total_quota: number;
  used_quota: number;
  remaining: number;
  active: boolean;
  invalid: boolean;
  status: KeyStatus;
  cooldown_seconds: number;
  last_used_at: string | null;
  created_at: string;
}

export interface Overview {
  key_count: number;
  available: number;
  cooling: number;
  exhausted: number;
  disabled: number;
  invalid: number;
  total_quota: number;
  used_quota: number;
  remaining_quota: number;
}

export interface SyncStatus {
  running: boolean;
  total: number;
  done: number;
  succeeded: number;
  failed: number;
  skipped: number;
  started_at: string | null;
  finished_at: string | null;
  errors: {id: number; alias: string; error: string}[];
}

export interface ImportResult {
  added: number;
  duplicates: number;
  errors: {line: number; reason: string}[];
}

export interface Settings {
  version: string;
  master_key_source: 'config' | 'state_file' | 'generated';
  config: Record<string, unknown>;
}

export interface KeyPatch {
  alias?: string;
  total_quota?: number;
  used_quota?: number;
  active?: boolean;
}

export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token);
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
}

async function send(path: string, init: RequestInit = {}, token = getToken()): Promise<Response> {
  const headers = new Headers(init.headers);
  if (token) headers.set('Authorization', `Bearer ${token}`);
  if (init.body !== undefined) headers.set('Content-Type', 'application/json');
  const res = await fetch(`/api${path}`, {...init, headers});
  if (res.ok) return res;

  let code = 'http_error';
  let message = `HTTP ${res.status}`;
  try {
    const body = (await res.json()) as {error?: string; message?: string};
    code = body.error ?? code;
    message = body.message ?? message;
  } catch {
    // Non-JSON error bodies keep the generic message.
  }
  if (res.status === 401) {
    clearToken();
    window.dispatchEvent(new Event(LOGOUT_EVENT));
  }
  throw new ApiError(res.status, code, message);
}

async function json<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await send(path, init);
  return (await res.json()) as T;
}

export const api = {
  /** Validates a candidate master key without storing it. */
  async verify(token: string): Promise<void> {
    const res = await fetch('/api/overview', {headers: {Authorization: `Bearer ${token}`}});
    if (res.status === 401) throw new ApiError(401, 'unauthorized', 'Master Key 不正确');
    if (!res.ok) throw new ApiError(res.status, 'http_error', `HTTP ${res.status}`);
  },
  overview: () => json<Overview>('/overview'),
  settings: () => json<Settings>('/settings'),
  listKeys: async () => (await json<{items: KeyItem[]}>('/keys')).items,
  importKeys: (text: string, totalQuota?: number) =>
    json<ImportResult>('/keys', {
      method: 'POST',
      body: JSON.stringify({text, total_quota: totalQuota}),
    }),
  updateKey: (id: number, patch: KeyPatch) =>
    json<KeyItem>(`/keys/${id}`, {method: 'PATCH', body: JSON.stringify(patch)}),
  deleteKey: (id: number) => json<{ok: boolean}>(`/keys/${id}`, {method: 'DELETE'}),
  deleteInvalid: () => json<{deleted: number}>('/keys/invalid', {method: 'DELETE'}),
  rawKey: async (id: number) => (await json<{key: string}>(`/keys/${id}/raw`)).key,
  resetKey: (id: number) => json<KeyItem>(`/keys/${id}/reset`, {method: 'POST'}),
  syncKey: (id: number) => json<KeyItem>(`/keys/${id}/sync`, {method: 'POST'}),
  syncStatus: () => json<SyncStatus>('/sync'),
  async startSync(): Promise<{started: boolean; status: SyncStatus}> {
    try {
      return {started: true, status: await json<SyncStatus>('/sync', {method: 'POST'})};
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        return {started: false, status: await api.syncStatus()};
      }
      throw e;
    }
  },
  async exportKeys(): Promise<Blob> {
    const res = await send('/keys/export');
    return res.blob();
  },
};

export function errorMessage(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}
