import {defineConfig, devices} from '@playwright/test';

const APP_PORT = 18290;
const MOCK_PORT = 18281;

export default defineConfig({
  testDir: './e2e',
  // All specs share one backend process and its key pool.
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  expect: {timeout: 10_000},
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: [['list'], ['html', {open: 'never', outputFolder: 'playwright-report'}]],
  outputDir: 'test-results',
  use: {
    baseURL: `http://127.0.0.1:${APP_PORT}`,
    locale: 'zh-CN',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    permissions: ['clipboard-read', 'clipboard-write'],
    viewport: {width: 1366, height: 900},
  },
  projects: [{name: 'chromium', use: {...devices['Desktop Chrome'], viewport: {width: 1366, height: 900}}}],
  webServer: [
    {
      command: 'node e2e/mock-upstream.mjs',
      url: `http://127.0.0.1:${MOCK_PORT}/__calls`,
      env: {MOCK_PORT: String(MOCK_PORT)},
      reuseExistingServer: false,
      timeout: 30_000,
    },
    {
      command: 'node e2e/start-server.mjs',
      url: `http://127.0.0.1:${APP_PORT}/healthz`,
      env: {APP_PORT: String(APP_PORT), MOCK_PORT: String(MOCK_PORT)},
      reuseExistingServer: false,
      timeout: 60_000,
    },
  ],
});
