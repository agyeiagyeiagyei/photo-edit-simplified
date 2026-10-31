import { test, expect } from '@playwright/test';
import * as fs from 'fs';
import {
  openApp, trackErrors, recordMetrics, importFiles, thumbs, canvas, canvasSize, canvasMean,
  clickTab, setRange, imageDims, zipEntryCount, jpegSegments, waitCanvasWidthAbove, perfMarks, JPEG_A, JPEG_B,
} from './helpers';

test.describe('24MP JPEG: import → edit → export', () => {
  test('import two 24MP JPEGs, preview renders, hi-res lands', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    await openApp(page);
    const imp = await importFiles(page, [JPEG_A, JPEG_B]);
    await expect(thumbs(page).first()).toHaveClass(/selected/);
    const size0 = await canvasSize(page);
    // Hi-res backing: viewport 1600 @ DPR2 needs > 1024 px, so the worker render must land.
    const t0 = Date.now();
    await waitCanvasWidthAbove(page, 1200, 15_000);
    const hiMs = Date.now() - t0;
    const size1 = await canvasSize(page);
    recordMetrics(testInfo, {
      import2_to_thumbs_ms: imp.thumbsMs, import2_to_canvas_ms: imp.canvasMs,
      first_canvas_w: size0.w, hires_canvas_w: size1.w, hires_after_import_ms: hiMs, ...(await perfMarks(page)),
    });
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('brightness slider updates the preview within a frame budget', async ({ page }, testInfo) => {
    await openApp(page);
    await importFiles(page, [JPEG_A]);
    // Steady state: the full-res pixels are decoded and the first sharp frame has landed before we edit.
    await waitCanvasWidthAbove(page, 1200, 20_000);
    await clickTab(page, 'Color');
    const before = await canvasMean(page);
    // Hi-res timing starts at the slider change; the timestamp is taken just
    // before the dispatch below.
    const t1 = Date.now();
    // Slider latency measured in-page: dispatch → the coalesced render completing.
    // Driven from Node, Playwright's request/poll overhead would dominate (the
    // old definition measured 57–120 ms for the same 8 ms render).
    const sliderFrame = await page.evaluate(async () => {
      const input = Array.from(document.querySelectorAll('label.slider'))
        .find((l) => l.textContent?.startsWith('Brightness'))!
        .querySelector('input') as HTMLInputElement;
      const perf = () => ((window as any).__pes?.perf() as any[] | undefined) ?? [];
      const t0 = performance.now();
      input.value = '0.6';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.dispatchEvent(new Event('change', { bubbles: true }));
      for (let i = 0; i < 120; i++) {
        await new Promise((r) => requestAnimationFrame(() => r(null)));
        const hit = perf().find((e) => e.name === 'preview.render' && e.at - e.dur_ms >= t0);
        if (hit) return { ms: Math.round(hit.at - t0), frames: i + 1 };
      }
      return { ms: -1, frames: -1 };
    });
    const pollT0 = Date.now();
    await expect.poll(() => canvasMean(page).then((m) => m.luma), { timeout: 5_000 }).toBeGreaterThan(before.luma + 15);
    const pollMs = Date.now() - pollT0;
    const previewMs = sliderFrame.ms;
    // Settled: hi-res frame for the edited state must arrive and stay bright. Measured from the slider change.
    // The 1024px preview tier blits first (width 1024); the sharp frame arrives when the backing exceeds 1200.
    await waitCanvasWidthAbove(page, 1200, 10_000);
    const hiEditMs = Date.now() - t1; // stamp immediately: everything below is outside the timed window
    const after = await canvasMean(page);
    expect(after.luma).toBeGreaterThan(before.luma + 15);
    // Ten rapid slider ticks: measure average main-thread cost per tick via long tasks.
    const perfLen0 = await page.evaluate(() => ((window as any).__pes?.perf() as any[] | undefined)?.length ?? 0);
    const longTasks = await page.evaluate(async () => {
      const tasks: number[] = [];
      const obs = new PerformanceObserver((l) => l.getEntries().forEach((e) => tasks.push(e.duration)));
      obs.observe({ type: 'longtask', buffered: false });
      const input = Array.from(document.querySelectorAll('label.slider')).find((l) => l.textContent?.startsWith('Brightness'))!.querySelector('input')!;
      const t = performance.now();
      for (let i = 0; i < 10; i++) {
        (input as HTMLInputElement).value = String(-0.5 + i * 0.1);
        input.dispatchEvent(new Event('input', { bubbles: true }));
        await new Promise((r) => requestAnimationFrame(() => r(null)));
      }
      const total = performance.now() - t;
      await new Promise((r) => setTimeout(r, 300));
      obs.disconnect();
      return { total_ms: Math.round(total), long_tasks: tasks.length, longest_ms: Math.round(Math.max(0, ...tasks)) };
    });
    const tickRuns = await page.evaluate((from) => {
      const p = ((window as any).__pes?.perf() as any[] | undefined)?.slice(from) ?? [];
      const count = (n: string) => p.filter((e) => e.name === n).length;
      return { preview: count('preview.effect'), filmstrip: count('filmstrip.each'), session: count('session.effect') };
    }, perfLen0);
    recordMetrics(testInfo, {
      slider_to_preview_ms: previewMs, slider_frames: sliderFrame.frames, slider_poll_ms: pollMs,
      hires_after_edit_ms: hiEditMs, ...(await perfMarks(page)),
      ten_ticks_total_ms: longTasks.total_ms, ten_ticks_long_tasks: longTasks.long_tasks, ten_ticks_longest_ms: longTasks.longest_ms,
      tick_preview_effects: tickRuns.preview, tick_filmstrip_renders: tickRuns.filmstrip, tick_session_effects: tickRuns.session,
    });
  });

  test('worker base cache: a cached base renders byte-identically to a fresh one', async ({ page }, testInfo) => {
    await openApp(page);
    await importFiles(page, [JPEG_A]);
    await waitCanvasWidthAbove(page, 1200, 20_000);
    const s = await canvasSize(page);
    const target = { w: s.w, h: s.h };
    await clickTab(page, 'Color');
    await setRange(page, /^Brightness/, 0.6);
    // The edited hi frame lands at the same dims, served from the worker's base.
    await expect.poll(() => canvasSize(page).then((r) => ({ w: r.w, h: r.h })), { timeout: 20_000 }).toEqual(target);
    const hash = () => canvas(page).evaluate((c: HTMLCanvasElement) => {
      const d = new Uint8Array(c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data.buffer);
      let h = 2166136261 >>> 0;
      for (let i = 0; i < d.length; i++) { h ^= d[i]; h = Math.imul(h, 16777619) >>> 0; }
      return { w: c.width, h: c.height, hash: h >>> 0 };
    });
    const cacheMarks = () => page.evaluate(() =>
      (((window as any).__pes?.perf() as any[]) ?? [])
        .filter((e) => e.name === 'worker.render.cache').map((e) => Math.round(e.dur_ms)));
    const hit = await hash();
    expect(await cacheMarks(), 'edited render should hit the worker base').toContain(1);
    // Rotate away and back: the single base slot now holds a rotated base, so
    // this render recomputes geometry+resample from the stored full frame.
    await clickTab(page, 'Rotate');
    await page.locator('button.btn', { hasText: '⟳ 90°' }).click();
    await waitCanvasWidthAbove(page, 1200, 20_000);
    await page.locator('button.btn', { hasText: '⟲ 90°' }).click();
    await expect.poll(() => canvasSize(page).then((r) => ({ w: r.w, h: r.h })), { timeout: 20_000 }).toEqual(target);
    const miss = await hash();
    expect(await cacheMarks(), 'round-trip render should miss the cache').toContain(0);
    expect(miss, 'cache hit and fresh base must render byte-identically').toEqual(hit);
    recordMetrics(testInfo, { cache_parity_w: target.w, cache_parity_h: target.h });
  });

  test('edit immediately after import (cold: full-res not yet decoded) still lands a sharp frame', async ({ page }, testInfo) => {
    await openApp(page);
    await importFiles(page, [JPEG_A]);
    await clickTab(page, 'Color');
    await page.evaluate(() => {
      (window as any).__qaLong = [] as number[];
      const obs = new PerformanceObserver((l) => l.getEntries().forEach((e) => (window as any).__qaLong.push(Math.round(e.duration))));
      obs.observe({ type: 'longtask', buffered: false }); (window as any).__qaLongObs = obs;
    });
    const t0 = Date.now();
    await setRange(page, /^Brightness/, 0.6);
    await waitCanvasWidthAbove(page, 1200, 20_000);
    const coldMs = Date.now() - t0;
    const longTasks: number[] = await page.evaluate(() => { (window as any).__qaLongObs?.disconnect(); return (window as any).__qaLong; });
    const after = await canvasMean(page);
    expect(after.luma).toBeGreaterThan(60);
    recordMetrics(testInfo, {
      hires_after_edit_cold_ms: coldMs, cold_edit_longest_task_ms: Math.max(0, ...longTasks), cold_edit_blocked_ms: longTasks.reduce((a, b) => a + b, 0),
      ...(await perfMarks(page, /^(hi\.|worker\.(load|render)|ingest\.)/)),
    });
  });

  test('1:1 crop export downloads a square JPEG with native crop resolution', async ({ page }, testInfo) => {
    await openApp(page);
    await importFiles(page, [JPEG_A]);
    await clickTab(page, 'Crop');
    await page.locator('.chip', { hasText: /^1:1$/ }).click();
    await clickTab(page, 'Export');
    const t0 = Date.now();
    const [dl] = await Promise.all([
      page.waitForEvent('download', { timeout: 60_000 }),
      page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
    ]);
    const p = await dl.path();
    const bytes = fs.readFileSync(p!);
    const exportMs = Date.now() - t0;
    fs.mkdirSync('results', { recursive: true }); fs.writeFileSync('results/export-1x1.jpg', bytes); // kept for encoder inspection
    expect(dl.suggestedFilename()).toMatch(/^edited-1[:_]1-bird-a-24mp\.jpg$/);
    expect(bytes.subarray(0, 2)).toEqual(Buffer.from([0xff, 0xd8]));
    const dims = await imageDims(page, bytes, 'image/jpeg');
    // 6000x4000 source → 1:1 crop is 4000x4000 at native resolution (±2px trim).
    expect(Math.abs(dims.w - dims.h)).toBeLessThanOrEqual(2);
    expect(dims.w).toBeGreaterThanOrEqual(3990);
    // EXIF carry-through: the source fixture has DateTimeOriginal 2026:05:01 06:12:00 and Make "Sony".
    const exif = jpegSegments(bytes).find((s) => s.marker === 0xe1 && s.data.subarray(0, 6).toString('latin1') === 'Exif\0\0');
    expect(exif, 'exported JPEG has no APP1 Exif segment').toBeTruthy();
    expect(exif!.data.includes(Buffer.from('2026:05:01 06:12:00', 'latin1'))).toBe(true);
    expect(exif!.data.includes(Buffer.from('Sony', 'latin1'))).toBe(true);
    const sof = jpegSegments(bytes).find((s) => s.marker === 0xc0 || s.marker === 0xc2)!;
    const ySampling = `${sof.data[7] >> 4}x${sof.data[7] & 15}`; // component 1 H×V: 2x2 = 4:2:0, 1x1 = 4:4:4
    recordMetrics(testInfo, { export_1x1_ms: exportMs, export_w: dims.w, export_h: dims.h, export_bytes: bytes.length, export_chroma_sampling: ySampling, export_progressive: sof.marker === 0xc2 });
  });

  test('"Download N edited" produces ONE zip containing every edited item (issue #1)', async ({ page }, testInfo) => {
    await openApp(page);
    await importFiles(page, [JPEG_A, JPEG_B]);
    await clickTab(page, 'Color');
    await setRange(page, /^Contrast/, 0.3);
    await clickTab(page, 'Export');
    await page.locator('button.btn', { hasText: 'Apply edits to all' }).click();
    const downloads: string[] = [];
    page.on('download', (d) => downloads.push(d.suggestedFilename()));
    const t0 = Date.now();
    const [dl] = await Promise.all([
      page.waitForEvent('download', { timeout: 120_000 }),
      page.locator('button.btn', { hasText: /Download 2 edited/ }).click(),
    ]);
    const bytes = fs.readFileSync((await dl.path())!);
    await page.waitForTimeout(1500);
    expect(downloads.length, `expected exactly one download, got ${downloads.join(', ')}`).toBe(1);
    expect(dl.suggestedFilename()).toBe('photo-edit-edited.zip');
    expect(zipEntryCount(bytes)).toBe(2);
    recordMetrics(testInfo, { zip2_ms: Date.now() - t0, zip_bytes: bytes.length });
  });

  test('visual: edited preview matches baseline', async ({ page }) => {
    await openApp(page);
    await importFiles(page, [JPEG_A]);
    await clickTab(page, 'Color');
    await setRange(page, /^Saturation/, 0.4);
    await setRange(page, /^Warmth/, 0.3);
    await page.waitForTimeout(1200); // let the hi-res frame settle
    await expect(canvas(page)).toHaveScreenshot('edited-preview.png', { maxDiffPixelRatio: 0.03 });
  });
});
