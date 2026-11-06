import { test, expect } from '@playwright/test';
import { openApp, importFiles, canvas, recordMetrics, trackErrors, JPEG_A } from './helpers';

// Bars (approved scope, issue #3 comment 6681440642): encode <= 6 s,
// refine decode <= 0.5 s, worker peak <= 400 MB, tab within +50 MB of
// baseline after the tool closes, no third-party requests. Encode/decode are
// recorded rather than hard-gated: CI runners are slower and shared, so a
// wall-clock assertion here would flake. The two structural gates below —
// worker termination and the memory return — are what the tool must guarantee.
test('subject tool segments, refines, and releases the worker', async ({ page }, testInfo) => {
  test.setTimeout(300_000);
  const errors = trackErrors(page);
  const thirdParty = new Set<string>();
  page.on('request', (r) => {
    const u = new URL(r.url());
    if (u.protocol === 'blob:' || u.protocol === 'data:') return;
    if (u.hostname !== '127.0.0.1' && u.hostname !== 'localhost') thirdParty.add(u.host);
  });
  const logs: string[] = [];
  page.on('console', (m) => logs.push(m.text()));

  await openApp(page);
  await importFiles(page, [JPEG_A]);
  await page.evaluate(() => {
    const w = window as any;
    w.__subjectResults = [];
    const orig = w.pesSegmentSubject;
    w.pesSegmentSubject = async function (...args: any[]) {
      const res = await orig.apply(this, args);
      w.__subjectResults.push({
        outW: res.outW,
        outH: res.outH,
        iou: res.iou,
        candidate: res.candidate,
        usedCrop: res.usedCrop,
        encodeMs: res.encodeMs ?? null,
        decodeMs: res.decodeMs ?? null,
        wasmPeakBytes: res.wasmPeakBytes ?? null,
        maskLen: res.mask?.length ?? null,
      });
      return res;
    };
    const close = w.pesSegmentSubjectClose;
    w.pesSegmentSubjectClose = function (...args: any[]) {
      w.__subjectClosed = (w.__subjectClosed || 0) + 1;
      return close.apply(this, args);
    };
  });

  const mem = () =>
    page.evaluate(() => ({
      heap: (performance as any).memory?.usedJSHeapSize ?? null,
      main: (window as any).__pes?.mem?.()?.main ?? null,
    }));
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('HeapProfiler.collectGarbage');
  await page.waitForTimeout(500);
  const baseline = await mem();

  await page.locator('.tabs .tab', { hasText: 'Select' }).click();
  await page.locator('.select-tab .chip', { hasText: 'Subject' }).click();
  await page.waitForTimeout(2500);

  const click = async (nx: number, ny: number) => {
    const b = await canvas(page).boundingBox();
    await canvas(page).click({ position: { x: b!.width * nx, y: b!.height * ny } });
  };

  const maskCount = () => logs.filter((l) => l.includes('subject mask')).length;
  const t0 = Date.now();
  await click(0.57, 0.55);
  await expect(page.locator('.overlay')).toContainText(/subject/i, { timeout: 20_000 });
  await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 240_000 });
  const firstTapMs = Date.now() - t0;
  await expect.poll(maskCount, { timeout: 20_000 }).toBeGreaterThan(0);
  await page.waitForTimeout(1500);

  const maskLog = logs.find((l) => l.includes('subject mask'))!;
  const setPx = Number(/set=(\d+)/.exec(maskLog)?.[1] || 0);
  expect(setPx, maskLog).toBeGreaterThan(1000);

  await page.evaluate(() => {
    const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement;
    (window as any).__tinted = c.getContext('2d')!.getImageData(0, 0, c.width, c.height);
  });

  // A second positive tap reuses the cached embedding: no re-encode.
  const masksBefore = maskCount();
  const t1 = Date.now();
  await click(0.6, 0.58);
  await expect.poll(maskCount, { timeout: 60_000 }).toBeGreaterThan(masksBefore);
  const refineWallMs = Date.now() - t1;
  await page.waitForTimeout(1000);

  const results = await page.evaluate(() => (window as any).__subjectResults);
  expect(results).toHaveLength(2);
  const fresh = results[0];
  const refine = results[1];
  expect(refine.encodeMs, 'refine tap re-encoded instead of reusing the embedding').toBeNull();
  expect(fresh.maskLen).toBe(fresh.outW * fresh.outH);
  expect(refine.maskLen).toBe(refine.outW * refine.outH);
  expect(fresh.iou).toBeGreaterThan(0);
  expect(refine.iou).toBeGreaterThan(0);

  // Clear drops the tint, proving the mask was on the canvas.
  await page.locator('.select-tab button', { hasText: /^Clear$/ }).click();
  await page.waitForTimeout(500);
  const tint = await page.evaluate(() => {
    const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement;
    const after = c.getContext('2d')!.getImageData(0, 0, c.width, c.height);
    const before = (window as any).__tinted as ImageData;
    if (before.width !== after.width || before.height !== after.height) return -1;
    let diff = 0;
    for (let i = 0; i < after.data.length; i += 4) {
      if (
        Math.abs(before.data[i] - after.data[i]) > 10 ||
        Math.abs(before.data[i + 1] - after.data[i + 1]) > 10 ||
        Math.abs(before.data[i + 2] - after.data[i + 2]) > 10
      ) {
        diff++;
      }
    }
    return diff;
  });
  expect(tint).toBeGreaterThan(0);

  // Leaving Select terminates the worker; wasm memory cannot shrink otherwise.
  await page.locator('.tabs .tab', { hasText: 'Crop' }).click();
  await page.waitForTimeout(1000);
  expect(await page.evaluate(() => (window as any).__subjectClosed || 0)).toBe(1);

  await cdp.send('HeapProfiler.collectGarbage');
  await page.waitForTimeout(500);
  const after = await mem();
  expect(baseline.heap).not.toBeNull();
  expect(after.heap).not.toBeNull();
  const MiB = 1048576;
  const deltaMb =
    (after.heap! - baseline.heap!) / MiB + ((after.main || 0) - (baseline.main || 0)) / MiB;

  const peakMb = Math.max(fresh.wasmPeakBytes || 0, refine.wasmPeakBytes || 0) / MiB;
  recordMetrics(testInfo, {
    subject_first_tap_ms: firstTapMs,
    subject_encode_ms: Math.round(fresh.encodeMs || 0),
    subject_decode_ms: Math.round(refine.decodeMs || 0),
    subject_refine_wall_ms: refineWallMs,
    subject_candidate: fresh.candidate,
    subject_used_crop: fresh.usedCrop,
    subject_iou: Math.round(fresh.iou * 1000) / 1000,
    subject_peak_mb: Math.round(peakMb),
    subject_tab_delta_mb: Math.round(deltaMb * 10) / 10,
    subject_mask_px: setPx,
  });

  expect(peakMb, 'worker wasm peak over the 400 MB bar').toBeLessThanOrEqual(400);
  expect(deltaMb, 'tab grew more than 50 MB after closing the subject worker').toBeLessThanOrEqual(50);
  expect([...thirdParty], 'subject tool made third-party requests').toEqual([]);
  expect(errors, errors.join('\n')).toEqual([]);
});
