// Starts the release binary against the mock upstream with a fresh state file.
import {spawn} from 'node:child_process';
import {existsSync, mkdirSync, rmSync} from 'node:fs';
import {dirname, join, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '..', '..');
const exe = join(root, 'target', 'release', process.platform === 'win32' ? 'tavily-lb.exe' : 'tavily-lb');
if (!existsSync(exe)) {
  console.error(`missing ${exe}; run "cargo build --release" first`);
  process.exit(1);
}

const dataDir = join(here, '.tmp');
rmSync(dataDir, {recursive: true, force: true});
mkdirSync(dataDir, {recursive: true});

const child = spawn(exe, [], {
  stdio: 'inherit',
  env: {
    ...process.env,
    LISTEN_ADDR: `127.0.0.1:${process.env.APP_PORT ?? 18290}`,
    TAVILY_BASE_URL: `http://127.0.0.1:${process.env.MOCK_PORT ?? 18281}`,
    MASTER_KEY: process.env.E2E_MASTER_KEY ?? 'e2e-master-key-0123456789',
    DATA_FILE: join(dataDir, 'state.json'),
    KEYS_FILE: '',
    ADMIN_UI: 'true',
    MCP_ENABLED: 'true',
    COOLDOWN_ON_429: '60s',
    RUST_LOG: process.env.RUST_LOG ?? 'warn',
  },
});

const stop = () => child.kill();
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
process.on('exit', stop);
child.on('exit', (code) => process.exit(code ?? 0));
