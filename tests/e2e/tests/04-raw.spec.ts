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
    // Texture runs from -100 (smooth) to +100 and starts at 0.
    const texture = page.locator('label.slider', { hasText: /^Texture/ }).locator('input');
    await expect(texture).toHaveValue('0');
    expect(await texture.evaluate((el: HTMLInputElement) => [el.min, el.max])).toEqual(['-100', '100']);
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

  test('Texture changes the developed RAW export in both directions', async ({ page }) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [RAWS[0]]);
    await expect(page.locator('.developing-badge')).toHaveCount(0, { timeout: 120_000 });
    // Mean absolute luma step between neighbouring pixels 8 px apart, over
    // the centre of the export: the mid-size contrast Texture works on.
    const midContrast = async (value: number) => {
      await clickTab(page, 'RAW');
      await page.locator('label.slider', { hasText: /^Texture/ }).locator('input').evaluate((el: HTMLInputElement, v) => {
        el.value = String(v); el.dispatchEvent(new Event('input', { bubbles: true })); el.dispatchEvent(new Event('change', { bubbles: true }));
      }, value);
      await clickTab(page, 'Export');
      await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
      const [dl] = await Promise.all([
        page.waitForEvent('download', { timeout: 180_000 }),
        page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
      ]);
      const b64 = fs.readFileSync((await dl.path())!).toString('base64');
      return page.evaluate(async (b64) => {
        const bmp = await createImageBitmap(await (await fetch(`data:image/png;base64,${b64}`)).blob());
        const [w, h] = [Math.min(1024, bmp.width), Math.min(1024, bmp.height)];
        const c = new OffscreenCanvas(w, h);
        const g = c.getContext('2d')!;
        g.drawImage(bmp, (bmp.width - w) >> 1, (bmp.height - h) >> 1, w, h, 0, 0, w, h);
        const d = g.getImageData(0, 0, w, h).data;
        const y = (i: number) => 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
        let s = 0, n = 0;
        for (let yy = 0; yy < h; yy += 2) for (let xx = 0; xx + 8 < w; xx += 2) { s += Math.abs(y((yy * w + xx) * 4) - y((yy * w + xx + 8) * 4)); n++; }
        return s / n;
      }, b64);
    };
    const base = await midContrast(0);
    const up = await midContrast(100);
    const down = await midContrast(-100);
    expect(up, `Texture +100 should add mid-size contrast (${base.toFixed(2)} -> ${up.toFixed(2)})`).toBeGreaterThan(base * 1.02);
    expect(down, `Texture -100 should smooth mid-size contrast (${base.toFixed(2)} -> ${down.toFixed(2)})`).toBeLessThan(base * 0.98);
    expect(errors.filter((e) => !/favicon/.test(e)), errors.join('\n')).toEqual([]);
  });
});

// #76: an export of a RAW sometimes produced no file and no message. A RAW
// is developed at full size only when it is exported (or zoomed), from the
// copy kept in the session store, so these tests get in the way of that read.
test.describe('A RAW export never stops without a word (#76)', () => {
  test.skip(RAWS.length === 0, 'no RAW fixture in dl-raw; run gen_dng.py to create one');
  // The synthetic DNG develops fastest; the path is the same for any RAW.
  const RAW = RAWS.find((r) => /\.dng$/i.test(r)) ?? RAWS[0];
  const download = (page: Page) => page.locator('button.btn.primary', { hasText: 'Download this' });
  const marks = (page: Page, name: string): Promise<any[]> =>
    page.evaluate((n) => ((window as any).__pes?.perf?.() || []).filter((e: any) => e.name === n), name);

  async function openDeveloped(page: Page) {
    await openApp(page);
    await importFiles(page, [RAW]);
    await expect(page.locator('.developing-badge')).toHaveCount(0, { timeout: 120_000 });
    await clickTab(page, 'Export');
    await page.evaluate(() => (window as any).__pes.reset());
  }

  test('waits for a slow read of the RAW and still downloads', async ({ page }, testInfo) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openDeveloped(page);
    // Keep a write transaction open on the stored RAWs for 35 s. The
    // develop's read waits behind it, as it can behind a busy disk. The
    // export used to give up after 30 s with no file and no message.
    const HOLD = 35_000;
    await page.evaluate((hold) => new Promise<void>((res, rej) => {
      const req = indexedDB.open('pes-session');
      req.onerror = () => rej(req.error);
      req.onsuccess = () => {
        const db = req.result;
        const store = db.transaction('raw_src', 'readwrite').objectStore('raw_src');
        const until = performance.now() + hold;
        // A transaction commits once no request is left, so chain them.
        const spin = () => { if (performance.now() < until) store.count().onsuccess = spin; else db.close(); };
        spin();
        res();
      };
    }), HOLD);
    const t0 = Date.now();
    const [dl] = await Promise.all([
      page.waitForEvent('download', { timeout: 180_000 }),
      download(page).click(),
    ]);
    const exportMs = Date.now() - t0;
    const bytes = fs.readFileSync((await dl.path())!);
    const dims = await imageDims(page, bytes, 'image/jpeg');
    expect(dims.w * dims.h, 'exported at sensor size').toBeGreaterThan(4_000_000);
    const [ensure] = await marks(page, 'export.ensure_full');
    expect(ensure?.dur_ms, 'the read of the RAW was held up').toBeGreaterThan(HOLD - 5_000);
    expect(await marks(page, 'export.failed')).toEqual([]);
    await expect(page.locator('.export-error')).toHaveCount(0);
    recordMetrics(testInfo, { raw_slow_read_export_ms: exportMs, raw_slow_read_ensure_full_ms: Math.round(ensure.dur_ms) });
    expect(errors.filter((e) => !/favicon/.test(e)), errors.join('\n')).toEqual([]);
  });

  test('says why when the RAW cannot be read back, and the next export clears it', async ({ page }) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openDeveloped(page);
    // Take the stored RAW away (and keep it, to put back afterwards).
    await page.evaluate(() => new Promise<void>((res, rej) => {
      const req = indexedDB.open('pes-session');
      req.onerror = () => rej(req.error);
      req.onsuccess = () => {
        const db = req.result;
        const tx = db.transaction('raw_src', 'readwrite');
        const store = tx.objectStore('raw_src');
        const keys = store.getAllKeys();
        const vals = store.getAll();
        vals.onsuccess = () => { (window as any).__qaRaw = { keys: keys.result, vals: vals.result }; store.clear(); };
        tx.oncomplete = () => { db.close(); res(); };
        tx.onerror = () => rej(tx.error);
      };
    }));
    let downloads = 0;
    page.on('download', () => downloads++);
    await download(page).click();
    const msg = page.locator('.export-error');
    await expect(msg).toBeVisible({ timeout: 120_000 });
    await expect(msg).toHaveText(`${path.basename(RAW)} was not exported: the full-size photo could not be prepared. Try again.`);
    expect((await marks(page, 'export.failed')).map((e) => e.detail)).toEqual(['full_size']);
    expect(await marks(page, 'export.full_retry'), 'tried once more before giving up').toHaveLength(1);
    await expect(download(page)).toBeEnabled();
    await expect(page.locator('.overlay')).toHaveCount(0);
    expect(downloads).toBe(0);

    // With the RAW back, the next export downloads and the message goes.
    await page.evaluate(() => new Promise<void>((res, rej) => {
      const req = indexedDB.open('pes-session');
      req.onerror = () => rej(req.error);
      req.onsuccess = () => {
        const db = req.result;
        const tx = db.transaction('raw_src', 'readwrite');
        const { keys, vals } = (window as any).__qaRaw;
        keys.forEach((k: IDBValidKey, i: number) => tx.objectStore('raw_src').put(vals[i], k));
        tx.oncomplete = () => { db.close(); res(); };
        tx.onerror = () => rej(tx.error);
      };
    }));
    const [dl] = await Promise.all([
      page.waitForEvent('download', { timeout: 180_000 }),
      download(page).click(),
    ]);
    expect(fs.statSync((await dl.path())!).size).toBeGreaterThan(0);
    await expect(msg).toHaveCount(0);
    expect(errors.filter((e) => !/favicon/.test(e)), errors.join('\n')).toEqual([]);
  });
});
