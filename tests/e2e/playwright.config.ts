import { defineConfig, devices } from '@playwright/test';
import * as path from 'path';

// PES_BASE_URL selects the target:
//   local dist  : http://127.0.0.1:8787/            (default; webServer serves $PES_DIST)
//   live site   : https://agyeiagyeiagyei.github.io/photo-edit-simplified/
const BASE = process.env.PES_BASE_URL || 'http://127.0.0.1:8787/';
const LOCAL = BASE.startsWith('http://127.0.0.1');
const DIST = process.env.PES_DIST || path.resolve(__dirname, '..', '..', 'dist');

export default defineConfig({
  testDir: './tests',
  timeout: 120_000,
  expect: { timeout: 20_000, toHaveScreenshot: { maxDiffPixelRatio: 0.02 } },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [['list'], ['json', { outputFile: 'results/report.json' }], ['html', { open: 'never', outputFolder: 'results/html' }]],
  outputDir: 'results/artifacts',
  use: {
    baseURL: BASE,
    ...devices['Desktop Chrome'],
    viewport: { width: 1600, height: 1000 },
    deviceScaleFactor: 2,          // desktop retina: forces the hi-res render path
    acceptDownloads: true,
    trace: 'retain-on-failure',
    video: 'off',
    launchOptions: {
      // PES_CHROMIUM points at a system Chromium; otherwise Playwright's own build.
      executablePath: process.env.PES_CHROMIUM || undefined,
      // Container /dev/shm is 64 MB; Chromium renderers crash without this.
      args: ['--enable-features=SharedArrayBuffer', '--disable-dev-shm-usage'],
    },
  },
  webServer: LOCAL
    ? {
        command: `python3 -m http.server 8787 --bind 127.0.0.1 --directory "${DIST}"`,
        url: BASE,
        reuseExistingServer: true,
        timeout: 20_000,
      }
    : undefined,
});
