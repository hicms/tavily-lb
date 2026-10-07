// Runs extra real tavily-lb processes (own port, own state file) for lifecycle tests.
import {execFile, spawn, type ChildProcess} from 'node:child_process';
import {existsSync, mkdirSync, readFileSync, rmSync, writeFileSync} from 'node:fs';
import {dirname, join, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {promisify} from 'node:util';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..', '..');
export const EXE = join(root, 'target', 'release', process.platform === 'win32' ? 'tavily-lb.exe' : 'tavily-lb');
const run = promisify(execFile);

export interface Server {
  url: string;
  dir: string;
  stateFile: string;
  /** PID of tavily-lb itself (not the launcher). */
  pid: number;
  child: ChildProcess;
  log: () => string;
  exited: Promise<number | null>;
}

export function freshDir(name: string): string {
  const dir = join(here, '.tmp', name);
  rmSync(dir, {recursive: true, force: true});
  mkdirSync(dir, {recursive: true});
  return dir;
}

const stripAnsi = (s: string) => s.replace(/\x1b\[[0-9;]*m/g, '');

/** Starts the release binary and waits for /healthz. `env` entries set to undefined are removed. */
export async function startServer(opts: {
  dir: string;
  port: number;
  env?: Record<string, string | undefined>;
  config?: string;
}): Promise<Server> {
  if (!existsSync(EXE)) throw new Error(`missing ${EXE}; run cargo build --release`);
  const stateFile = join(opts.dir, 'state.json');
  const args: string[] = [];
  if (opts.config) {
    const cfg = join(opts.dir, 'config.toml');
    writeFileSync(cfg, opts.config);
    args.push('--config', cfg);
  }
  const env: Record<string, string> = {};
  for (const [k, v] of Object.entries({
    ...process.env,
    LISTEN_ADDR: `127.0.0.1:${opts.port}`,
    DATA_FILE: stateFile,
    KEYS_FILE: '',
    RUST_LOG: 'info',
    NO_COLOR: '1',
    ...opts.env,
  })) {
    if (v !== undefined) env[k] = v;
  }
  // windowsHide gives the launcher (and the server it starts) its own hidden console, so a
  // Ctrl+C sent there cannot reach the test runner.
  const child =
    process.platform === 'win32'
      ? spawn('pwsh', ['-NoProfile', '-NonInteractive', '-File', join(here, 'launch.ps1'), '-Exe', EXE, ...(args.length ? ['-Config', args[1]] : [])], {
          cwd: opts.dir,
          env,
          windowsHide: true,
          stdio: ['ignore', 'pipe', 'pipe'],
        })
      : spawn(EXE, args, {cwd: opts.dir, env, stdio: ['ignore', 'pipe', 'pipe']});
  let out = '';
  child.stdout!.on('data', (d) => (out += d));
  child.stderr!.on('data', (d) => (out += d));
  const exited = new Promise<number | null>((r) => child.on('exit', (code) => r(code)));
  const url = `http://127.0.0.1:${opts.port}`;
  let pid = process.platform === 'win32' ? 0 : child.pid!;
  for (let i = 0; i < 150; i++) {
    if (child.exitCode !== null) throw new Error(`tavily-lb exited early:\n${stripAnsi(out)}`);
    if (!pid) {
      const m = /CHILDPID (\d+)/.exec(out);
      if (m) pid = Number(m[1]);
    }
    try {
      if ((await fetch(`${url}/healthz`)).ok) break;
    } catch {
      /* not listening yet */
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  if (!pid) throw new Error(`tavily-lb did not start:\n${stripAnsi(out)}`);
  return {url, dir: opts.dir, stateFile, pid, child, log: () => stripAnsi(out), exited};
}

/** Sends a real Ctrl+C (CTRL_C_EVENT) or Ctrl+Break and returns the exit code. */
export async function ctrlC(server: Server, event: 'c' | 'break' = 'c'): Promise<number | null> {
  if (process.platform === 'win32') {
    const script = join(here, 'send-ctrl-c.ps1');
    await run('pwsh', ['-NoProfile', '-NonInteractive', '-File', script, '-ProcessId', String(server.pid), '-Event', event], {
      timeout: 30_000,
      windowsHide: true,
    });
  } else {
    server.child.kill('SIGINT');
  }
  return withTimeout(server.exited, 15_000, 'server did not exit after Ctrl+C');
}

/** Hard kill (TerminateProcess / SIGKILL): no shutdown hook runs. */
export async function hardKill(server: Server): Promise<void> {
  process.kill(server.pid, 'SIGKILL');
  await withTimeout(server.exited, 15_000, 'server did not die');
}

export function readState(server: Pick<Server, 'stateFile'>) {
  return JSON.parse(readFileSync(server.stateFile, 'utf8')) as {
    month: string | null;
    master_key: string | null;
    keys: {id: number; alias: string; key: string; used_quota: number; total_quota: number; active: boolean; invalid: boolean}[];
  };
}

export function writeState(server: Pick<Server, 'stateFile'>, state: unknown): void {
  writeFileSync(server.stateFile, JSON.stringify(state, null, 2));
}

async function withTimeout<T>(p: Promise<T>, ms: number, msg: string): Promise<T> {
  let t: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([p, new Promise<never>((_, rej) => (t = setTimeout(() => rej(new Error(msg)), ms)))]);
  } finally {
    clearTimeout(t);
  }
}
