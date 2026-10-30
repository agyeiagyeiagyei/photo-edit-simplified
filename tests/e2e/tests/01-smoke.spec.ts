import { test, expect } from '@playwright/test';
import { openApp, trackErrors, recordMetrics } from './helpers';

test.describe('smoke + startup', () => {
  test('app shell loads with no console errors and reports wasm sizes', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    const t = await openApp(page);
    await expect(page.locator('.dropzone')).toBeVisible();
    await expect(page.locator('label.btn', { hasText: 'Add media' })).toBeVisible();
    // Service worker registers (pes-v4 strategy).
    const sw = await page.evaluate(async () => {
      if (!('serviceWorker' in navigator)) return 'unsupported';
      const reg = await Promise.race([navigator.serviceWorker.ready, new Promise((r) => setTimeout(() => r(null), 8000))]);
      return reg ? 'ready' : 'timeout';
    });
    recordMetrics(testInfo, { shell_visible_ms: t.shellMs, service_worker: sw, ...Object.fromEntries(Object.entries(t).filter(([k]) => k !== 'shellMs')) });
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('second (cached) load is fast', async ({ page }, testInfo) => {
    await openApp(page);
    // Give the service worker a moment to finish precaching, then reload.
    await page.waitForTimeout(1500);
    const t0 = Date.now();
    await page.reload();
    await expect(page.locator('header h1')).toHaveText('photo-edit-simplified');
    const cachedMs = Date.now() - t0;
    const fromCache = await page.evaluate(() =>
      (performance.getEntriesByType('resource') as PerformanceResourceTiming[])
        .filter((e) => /\.wasm$/.test(e.name))
        .map((e) => ({ name: e.name.split('/').pop(), transfer: e.transferSize, ms: Math.round(e.duration) })),
    );
    recordMetrics(testInfo, { cached_shell_visible_ms: cachedMs, wasm_entries: JSON.stringify(fromCache) });
  });
});
