// Live suite: real binary, real https://api.tavily.com, real keys from TAVILY_KEYS_FILE.
// Traces, videos and automatic screenshots stay off because they would capture raw keys.
import {defineConfig, devices} from '@playwright/test';

const PORT = 18390;
const desktop = {viewport: {width: 1440, height: 900}, locale: 'zh-CN'};
const ui = /ui\.live\.spec\.ts/;

export default defineConfig({
  testDir: './e2e-live',
  testMatch: /\.live\.spec\.ts$/,
  fullyParallel: false,
  workers: 1,
  timeout: 120_000,
  expect: {timeout: 15_000},
  retries: 0,
  reporter: [['list'], ['html', {open: 'never', outputFolder: 'playwright-report-live'}]],
  outputDir: 'test-results/live',
  use: {
    trace: 'off',
    video: 'off',
    screenshot: 'off',
    permissions: ['clipboard-read', 'clipboard-write'],
  },
  projects: [
    {name: 'api', testMatch: /(api-mcp|ops)\.live\.spec\.ts/, use: {...devices['Desktop Chrome'], ...desktop}},
    {name: 'chromium', testMatch: ui, use: {...devices['Desktop Chrome'], ...desktop}},
    // Tavily rate limits GET /usage for all keys together, so usage-sync tests run in Chromium only.
    {name: 'msedge', testMatch: ui, grepInvert: /@usage/, use: {...devices['Desktop Edge'], channel: 'msedge', ...desktop}},
    {name: 'chrome', testMatch: ui, grepInvert: /@usage/, use: {...devices['Desktop Chrome'], channel: 'chrome', ...desktop}},
  ],
  webServer: {
    command: 'node e2e-live/start-server.mjs',
    url: `http://127.0.0.1:${PORT}/healthz`,
    env: {LIVE_PORT: String(PORT)},
    reuseExistingServer: false,
    timeout: 60_000,
  },
});
