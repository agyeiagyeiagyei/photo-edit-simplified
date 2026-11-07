import { test, expect } from '@playwright/test';
import type { Page } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';
import { openApp, trackErrors, recordMetrics, importFiles, canvasSize, clickTab, imageDims, rawFixtures, PNG_SMALL } from './helpers';

// One test per RAW in dl-raw/: CI generates a synthetic DNG (small embedded JPEG, so the
// proxy timing covers the sensor-develop path) and may also have a real ARW (camera-JPEG path).
const RAWS = rawFixtures();

/**
 * Widest proxy persisted at display size ("raw" develop or "display" camera
 * decode), or 0 while none is usable. A legacy "camera" record stores the
 * full-size embedded JPEG and is re-decoded in the worker on restore, so it
 * does not count as a restorable display proxy.
 */
async function storedDisplayProxyWidth(page: Page): Promise<number> {
  return page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((res, rej) => {
      const req = indexedDB.open('pes-session');
      req.onsuccess = () => res(req.result);
      req.onerror = () => rej(req.error);
    });
    let recs: any[] = [];
    try {
      recs = await new Promise<any[]>((res) => {
        const req = db.transaction('proxies', 'readonly').objectStore('proxies').getAll();
        req.onsuccess = () => res(req.result ?? []);
        req.onerror = () => res([]);
      });
    } finally {
      db.close();
    }
    let widest = 0;
    for (const rec of recs) {
      if (rec?.kind !== 'raw' && rec?.kind !== 'display') continue;
      const bytes = rec?.bytes;
      if (!bytes) continue;
      const blob = bytes instanceof Blob ? bytes : new Blob([bytes], { type: 'image/jpeg' });
      try {
        const bmp = await createImageBitmap(blob);
        widest = Math.max(widest, bmp.width);
        bmp.close();
      } catch {}
    }
    return widest;
  });
}

test.describe('RAW (ARW/DNG) import → develop → export', () => {
  test.skip(RAWS.length === 0, 'no RAW fixture in dl-raw; run gen_dng.py to create one');

  for (const RAW of RAWS) {
    const name = path.basename(RAW);

    test(`RAW ${name} shows a preview, then exports at sensor size`, async ({ page }, testInfo) => {
      const errors = trackErrors(page);
      await openApp(page);
      // Sample the visible backing width every frame: once a sharp frame lands,
      // the soft 1024px tier must never flash back while the developed proxy
      // replaces the camera JPEG (staleness was QA's ~210 ms flicker).
      await page.evaluate(() => {
        const w = window as any;
        w.__qaRawWidths = [] as number[];
        const tick = () => {
          const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement | null;
          if (c) w.__qaRawWidths.push(c.width);
          w.__qaRawRaf = requestAnimationFrame(tick);
        };
        tick();
      });
      const imp = await importFiles(page, [RAW]);
      // Hi-res proxy (camera JPEG first, sensor develop ~2 s later).
      const t0 = Date.now();
      await expect.poll(async () => (await canvasSize(page)).w, { timeout: 60_000 }).toBeGreaterThan(1200);
      const proxyMs = Date.now() - t0;
      // Wait for the sensor develop's frame to blit. This is a no-op for a
      // fixture whose camera JPEG is too small to beat the soft preview.
      await expect.poll(async () => (await canvasSize(page)).w, { timeout: 60_000 }).toBeGreaterThan(2000);
      const developedMs = Date.now() - t0;
      // The "Developing RAW…" badge marks the interim camera-JPEG look; once
      // the sensor develop has landed it must be gone, never stuck on screen.
      await expect.poll(
        () => page.evaluate(() => ((window as any).__pes?.perf?.() || []).some((e: any) => e.name === 'proxy.raw_develop' || e.name === 'pool.develop')),
        { timeout: 60_000 },
      ).toBe(true);
      await expect(page.locator('.developing-badge')).toHaveCount(0, { timeout: 30_000 });
      const swap = await page.evaluate(() => {
        const w = window as any;
        cancelAnimationFrame(w.__qaRawRaf);
        const widths: number[] = w.__qaRawWidths;
        const first = widths.findIndex((x) => x > 1200);
        const tail = first >= 0 ? widths.slice(first) : widths;
        return { frames: widths.length, first, firstW: first >= 0 ? widths[first] : 0, min: tail.length ? Math.min(...tail) : 0 };
      });
      expect(swap.first, 'saw a sharp backing width').toBeGreaterThanOrEqual(0);
      expect(swap.min, 'sharp backing fell back to the soft preview during the proxy swap').toBeGreaterThan(1200);
      // RAW items get their own RAW tab for the develop controls.
      await clickTab(page, 'RAW');
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
      recordMetrics(testInfo, { raw_fixture: name, raw_import_to_canvas_ms: imp.canvasMs, raw_hires_proxy_ms: proxyMs, raw_developed_canvas_ms: developedMs, raw_first_sharp_width: swap.firstW, raw_proxy_swap_min_width: swap.min, raw_proxy_swap_frames: swap.frames, raw_export_ms: exportMs, raw_export_w: dims.w, raw_export_h: dims.h, raw_export_longest_task_ms: longestMs, raw_export_main_thread_blocked_ms: blockedMs, raw_export_long_tasks: longTasks.length });
      expect(errors.filter((e) => !/favicon/.test(e)), errors.join('\n')).toEqual([]);
    });

    test(`RAW ${name} restores its stored proxy and is editable`, async ({ page }, testInfo) => {
      test.setTimeout(300_000);
      const errors = trackErrors(page);
      await openApp(page);
      await importFiles(page, [RAW]);
      // Wait until a display-size proxy at least 1200 px wide is persisted:
      // the ARW's downscaled camera decode ("display") or the synthetic DNG's
      // sensor develop ("raw"). A legacy full-size "camera" record is not
      // restorable within the budget, so it is not accepted here.
      await expect.poll(() => storedDisplayProxyWidth(page), { timeout: 180_000 }).toBeGreaterThan(1200);
      // The item record itself is written by the debounced session pass.
      await expect
        .poll(
          () =>
            page.evaluate(
              () =>
                ((window as any).__pes?.perf?.() || []).filter(
                  (e: any) => e.name === 'session.persist' && e.detail === '1 items',
                ).length,
            ),
          { timeout: 60_000 },
        )
        .toBeGreaterThan(0);

      // Acceptance: the selected RAW is editable from its stored proxy within
      // 600 ms of the reload, i.e. a >1200 px backing is on the canvas.
      await page.addInitScript(() => {
        const R: { proxy: number; overlayGone: number; width: number } = { proxy: -1, overlayGone: -1, width: 0 };
        (window as any).__rawRestore = R;
        const tick = () => {
          const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement | null;
          if (R.proxy < 0 && c && c.width > 1200) {
            R.proxy = performance.now();
            R.width = c.width;
          }
          if (R.proxy >= 0 && R.overlayGone < 0 && !document.querySelector('.overlay')) {
            R.overlayGone = performance.now();
          }
          if (R.proxy < 0 || R.overlayGone < 0) requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
      });
      await page.reload();
      await expect.poll(
        () => page.evaluate(() => (window as any).__rawRestore?.proxy ?? -1),
        { timeout: 60_000 },
      ).toBeGreaterThan(0);
      const probe = await page.evaluate(
        () => (window as any).__rawRestore as { proxy: number; overlayGone: number; width: number },
      );
      recordMetrics(testInfo, {
        raw_fixture: name,
        raw_restore_proxy_ms: Math.round(probe.proxy),
        raw_restore_overlay_gone_ms: Math.round(probe.overlayGone),
        raw_restore_canvas_w: probe.width,
      });
      expect(probe.width, 'restored proxy on canvas is sharp').toBeGreaterThan(1200);
      expect(probe.proxy, `stored proxy took ${Math.round(probe.proxy)} ms after reload (target ≤ 600 ms)`).toBeLessThanOrEqual(600);
      expect(errors.filter((e) => !/favicon/.test(e)), errors.join('\n')).toEqual([]);
    });
  }

  // Owner report (8 Nov): no RAW options on the opened RAW. The controls sat
  // at the top of Color while the editor opens on Crop.
  test('a RAW gets its own RAW tab, a PNG does not', async ({ page }) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    const raw = RAWS[0];
    await importFiles(page, [raw, PNG_SMALL]);
    const thumb = (p: string) => page.locator(`.filmstrip .thumb[title="${path.basename(p)}"]`);
    const rawTab = page.locator('.tabs button', { hasText: /^RAW$/ });

    await thumb(raw).click();
    await expect(rawTab).toBeVisible();
    await expect(page.locator('.tabs button').first(), 'RAW should be the first tab').toHaveText('RAW');
    await rawTab.click();
    await expect(page.locator('.raw-panel .panel-title', { hasText: 'RAW develop' })).toBeVisible();
    await expect(page.locator('label.slider', { hasText: /^Exposure/ })).toBeVisible();
    await clickTab(page, 'Color');
    await expect(page.locator('.raw-panel'), 'RAW controls belong on the RAW tab only').toHaveCount(0);

    // Switching to a PNG while on the RAW tab lands on Color.
    await rawTab.click();
    await thumb(PNG_SMALL).click();
    await expect(rawTab).toHaveCount(0);
    await expect(page.locator('.tabs button.active')).toHaveText('Color');
    await expect(page.locator('label.slider', { hasText: /^Brightness/ })).toBeVisible();
    expect(errors.filter((e) => !/favicon/.test(e)), errors.join('\n')).toEqual([]);
  });
});
