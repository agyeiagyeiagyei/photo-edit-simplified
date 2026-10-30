import { test, expect } from '@playwright/test';
import * as fs from 'fs';
import { openApp, trackErrors, recordMetrics, importFiles, canvasSize, clickTab, imageDims, rawFixture } from './helpers';

const RAW = rawFixture();

test.describe('RAW (ARW/DNG) import → develop → export', () => {
  test.skip(!RAW, 'no RAW fixture in dl-raw');

  test('RAW shows a camera preview quickly, then a developed proxy, then exports at sensor size', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    await openApp(page);
    const imp = await importFiles(page, [RAW!]);
    // Hi-res proxy (camera JPEG first, sensor develop ~2 s later).
    const t0 = Date.now();
    await expect.poll(async () => (await canvasSize(page)).w, { timeout: 60_000 }).toBeGreaterThan(1200);
    const proxyMs = Date.now() - t0;
    // The RAW develop panel lives inside the Color tab.
    await clickTab(page, 'Color');
    await expect(page.locator('.raw-panel .panel-title', { hasText: 'RAW develop' })).toBeVisible();
    await page.locator('label.slider', { hasText: /^Exposure/ }).locator('input').evaluate((el: HTMLInputElement) => {
      el.value = '0.5'; el.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await page.waitForTimeout(3500); // allow the background sensor develop to land
    await clickTab(page, 'Export');
    const t1 = Date.now();
    const [dl] = await Promise.all([
      page.waitForEvent('download', { timeout: 180_000 }),
      page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
    ]);
    const bytes = fs.readFileSync((await dl.path())!);
    const exportMs = Date.now() - t1;
    const dims = await imageDims(page, bytes, 'image/jpeg');
    expect(dims.w * dims.h).toBeGreaterThan(4_000_000); // sensor-resolution develop (fixture is a 5 MP crop-mode ARW), not the 1024px preview
    recordMetrics(testInfo, { raw_import_to_canvas_ms: imp.canvasMs, raw_hires_proxy_ms: proxyMs, raw_export_ms: exportMs, raw_export_w: dims.w, raw_export_h: dims.h });
    expect(errors.filter((e) => !/favicon/.test(e)), errors.join('\n')).toEqual([]);
  });
});
