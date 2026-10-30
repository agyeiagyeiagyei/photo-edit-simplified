import { test, expect } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';
import { openApp, trackErrors, recordMetrics, importFiles, canvasSize, clickTab, imageDims, rawFixtures } from './helpers';

// One test per RAW in dl-raw/: CI generates a synthetic DNG (small embedded JPEG, so the
// proxy timing covers the sensor-develop path) and may also have a real ARW (camera-JPEG path).
const RAWS = rawFixtures();

test.describe('RAW (ARW/DNG) import → develop → export', () => {
  test.skip(RAWS.length === 0, 'no RAW fixture in dl-raw; run gen_dng.py to create one');

  for (const RAW of RAWS) {
    const name = path.basename(RAW);

    test(`RAW ${name} shows a preview, then exports at sensor size`, async ({ page }, testInfo) => {
      const errors = trackErrors(page);
      await openApp(page);
      const imp = await importFiles(page, [RAW]);
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
      // Main-thread responsiveness during export: record long tasks (>50 ms) until the download lands.
      await page.evaluate(() => {
        (window as any).__qaLong = [] as number[];
        const obs = new PerformanceObserver((l) => l.getEntries().forEach((e) => (window as any).__qaLong.push(Math.round(e.duration))));
        obs.observe({ type: 'longtask', buffered: false });
        (window as any).__qaLongObs = obs;
      });
      const t1 = Date.now();
      const [dl] = await Promise.all([
        page.waitForEvent('download', { timeout: 180_000 }),
        page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
      ]);
      const bytes = fs.readFileSync((await dl.path())!);
      const exportMs = Date.now() - t1;
      const longTasks: number[] = await page.evaluate(() => { (window as any).__qaLongObs?.disconnect(); return (window as any).__qaLong; });
      const longestMs = Math.max(0, ...longTasks);
      const blockedMs = longTasks.reduce((a, b) => a + b, 0);
      const dims = await imageDims(page, bytes, 'image/jpeg');
      expect(dims.w * dims.h).toBeGreaterThan(4_000_000); // sensor-resolution develop, not the 1024px preview
      recordMetrics(testInfo, { raw_fixture: name, raw_import_to_canvas_ms: imp.canvasMs, raw_hires_proxy_ms: proxyMs, raw_export_ms: exportMs, raw_export_w: dims.w, raw_export_h: dims.h, raw_export_longest_task_ms: longestMs, raw_export_main_thread_blocked_ms: blockedMs, raw_export_long_tasks: longTasks.length });
      expect(errors.filter((e) => !/favicon/.test(e)), errors.join('\n')).toEqual([]);
    });
  }
});
