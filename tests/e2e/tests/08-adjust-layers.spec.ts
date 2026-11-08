import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import {
  openApp, trackErrors, recordMetrics, importFiles, canvas, canvasSize, clickTab, setRange,
  thumbs, PNG_SMALL, rawFixtures,
} from './helpers';

type Frame = { w: number; h: number; bytes: Buffer };
type Mark = { name: string; dur_ms: number; detail?: string };

const W = 800;
const H = 600;

// Issue #26 verification. The Drive manifest stores `edit_recipe_json` and the
// session store writes the same portable mask wire form
// (`selection_portable_json`), parsed back by `selection_from_layer_value`;
// reload here therefore exercises the portable rect/lasso mask round trip.
// The worker's '"ref"' mask form is exercised by every masked preview/export,
// which sends the mask ahead of the render that references it.

function b64ToBytes(b64: string): Buffer {
  return Buffer.from(b64, 'base64');
}

async function readCanvas(page: Page): Promise<Frame> {
  const r = await canvas(page).evaluate((c: HTMLCanvasElement) => {
    const d = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
    let s = '';
    for (let i = 0; i < d.length; i += 0x8000) {
      s += String.fromCharCode.apply(null, Array.from(d.subarray(i, i + 0x8000)) as number[]);
    }
    return { w: c.width, h: c.height, b64: btoa(s) };
  });
  return { w: r.w, h: r.h, bytes: b64ToBytes(r.b64) };
}

async function decodePng(page: Page, bytes: Buffer): Promise<Frame> {
  const r = await page.evaluate(async (b64) => {
    const bin = atob(b64);
    const u8 = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
    const bmp = await createImageBitmap(new Blob([u8], { type: 'image/png' }));
    const c = document.createElement('canvas');
    c.width = bmp.width;
    c.height = bmp.height;
    const x = c.getContext('2d')!;
    x.drawImage(bmp, 0, 0);
    const d = x.getImageData(0, 0, bmp.width, bmp.height).data;
    let s = '';
    for (let i = 0; i < d.length; i += 0x8000) {
      s += String.fromCharCode.apply(null, Array.from(d.subarray(i, i + 0x8000)) as number[]);
    }
    bmp.close();
    return { w: c.width, h: c.height, b64: btoa(s) };
  }, bytes.toString('base64'));
  return { w: r.w, h: r.h, bytes: b64ToBytes(r.b64) };
}

function psnr(a: Buffer, b: Buffer): number {
  if (a.length !== b.length) return -1;
  let se = 0;
  let n = 0;
  for (let i = 0; i < a.length; i += 4) {
    for (let c = 0; c < 3; c++) {
      const d = a[i + c] - b[i + c];
      se += d * d;
      n++;
    }
  }
  const mse = se / n;
  return mse === 0 ? 99 : 10 * Math.log10(65025 / mse);
}

function meanDiff(a: Buffer, b: Buffer): number {
  if (a.length !== b.length) return -1;
  let sum = 0;
  let n = 0;
  for (let i = 0; i < a.length; i += 4) {
    for (let c = 0; c < 3; c++) {
      sum += Math.abs(a[i + c] - b[i + c]);
      n++;
    }
  }
  return sum / n;
}

/** Mean |ΔRGB| (0..255) inside a normalized rect. */
function regionDiff(a: Buffer, b: Buffer, w: number, h: number, x0: number, y0: number, x1: number, y1: number): number {
  if (a.length !== b.length) return -1;
  const X0 = Math.max(0, Math.floor(x0 * w));
  const X1 = Math.min(w, Math.ceil(x1 * w));
  const Y0 = Math.max(0, Math.floor(y0 * h));
  const Y1 = Math.min(h, Math.ceil(y1 * h));
  let sum = 0;
  let n = 0;
  for (let y = Y0; y < Y1; y++) {
    for (let x = X0; x < X1; x++) {
      const i = (y * w + x) * 4;
      for (let c = 0; c < 3; c++) {
        sum += Math.abs(a[i + c] - b[i + c]);
        n++;
      }
    }
  }
  return n ? sum / n : 0;
}

const marks = (page: Page): Promise<Mark[]> =>
  page.evaluate(() => (((window as any).__pes?.perf?.() ?? []) as Mark[]));

const markCount = async (page: Page, name: string) =>
  (await marks(page)).filter((m) => m.name === name).length;

const markNames = async (page: Page) => (await marks(page)).map((m) => m.name);

async function waitMark(page: Page, name: string, before: number, timeout = 60_000) {
  await expect.poll(() => markCount(page, name), { timeout, intervals: [100, 200, 400] })
    .toBeGreaterThan(before);
}

const canvasHash = (page: Page) =>
  canvas(page).evaluate((c: HTMLCanvasElement) => {
    const n = document.createElement('canvas');
    n.width = 64;
    n.height = Math.max(1, Math.round((64 * c.height) / c.width));
    const x = n.getContext('2d')!;
    x.drawImage(c, 0, 0, n.width, n.height);
    const d = x.getImageData(0, 0, n.width, n.height).data;
    let h = 2166136261 >>> 0;
    for (let i = 0; i < d.length; i += 8) {
      h ^= d[i];
      h = Math.imul(h, 16777619) >>> 0;
    }
    return String(h);
  });

async function settle(page: Page, stableMs = 400) {
  let prev = '';
  let since = Date.now();
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const cur = await canvasHash(page);
    const now = Date.now();
    if (cur !== prev) {
      prev = cur;
      since = now;
    } else if (now - since >= stableMs) {
      return;
    }
    await page.waitForTimeout(120);
  }
  throw new Error('canvas never settled');
}

async function waitSmall(page: Page) {
  await expect.poll(() => canvasSize(page).then((s) => ({ w: s.w, h: s.h })), { timeout: 30_000 })
    .toEqual({ w: W, h: H });
  await settle(page);
}

async function wrapBox(page: Page) {
  const b = await page.locator('.canvas-wrap').boundingBox();
  if (!b) throw new Error('no .canvas-wrap box');
  return b;
}

async function rectDrag(page: Page, x0: number, y0: number, x1: number, y1: number) {
  const b = await wrapBox(page);
  const px = (f: number) => b.x + b.width * f;
  const py = (f: number) => b.y + b.height * f;
  await page.mouse.move(px(x0), py(y0));
  await page.mouse.down();
  await page.mouse.move(px((x0 + x1) / 2), py((y0 + y1) / 2), { steps: 5 });
  await page.mouse.move(px(x1), py(y1), { steps: 5 });
  await page.mouse.up();
  await page.waitForTimeout(120);
}

async function lassoDrag(page: Page, pts: [number, number][]) {
  const b = await wrapBox(page);
  const px = (f: number) => b.x + b.width * f;
  const py = (f: number) => b.y + b.height * f;
  await page.mouse.move(px(pts[0][0]), py(pts[0][1]));
  await page.mouse.down();
  for (const [x, y] of pts.slice(1)) {
    await page.mouse.move(px(x), py(y), { steps: 8 });
  }
  await page.mouse.up();
  await page.waitForTimeout(120);
}

async function selectTool(page: Page, name: 'Rect' | 'Lasso') {
  await clickTab(page, 'Select');
  await page.locator('.select-tab .chip', { hasText: new RegExp(`^${name}$`) }).click();
}

const addAdjust = (page: Page) =>
  page.locator('button.btn', { hasText: /＋ Adjust\s*$/ });
const addAdjustFromSel = (page: Page) =>
  page.locator('button.btn', { hasText: /＋ Adjust from sel\./ });
const takeSelection = (page: Page) =>
  page.locator('button.btn', { hasText: 'New adjustment from selection' });

async function exportPng(page: Page): Promise<Buffer> {
  await clickTab(page, 'Export');
  await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
  const [dl] = await Promise.all([
    page.waitForEvent('download', { timeout: 120_000 }),
    page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
  ]);
  return fs.readFileSync((await dl.path())!);
}

async function layerSliderValue(page: Page, idx: number): Promise<number> {
  const v = await page.locator('.adjust-editor label.slider').nth(idx).locator('input').inputValue();
  return Number(v);
}

test.describe('adjustment layers (issue #26)', () => {
  test('gpu=1: masked layers hand off to the CPU path and match the PNG export', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page, './?gpu=1');
    await importFiles(page, [PNG_SMALL]);
    await expect.poll(() => canvasSize(page).then((s) => s.w), { timeout: 30_000 }).toBeGreaterThan(0);
    await settle(page);
    const gpuInitial = await markCount(page, 'gpu.render');

    // Switching tabs repaints the existing GPU frame; start the marker
    // window after that so only the layer-triggered render is measured.
    await clickTab(page, 'Layers');
    await page.waitForTimeout(350);
    await page.evaluate(() => (window as any).__pes.reset());

    const blits0 = await markCount(page, 'hi.blit');
    await addAdjust(page).click();
    await waitMark(page, 'hi.blit', blits0);
    await waitSmall(page);
    base: {
      const names = await markNames(page);
      expect(names, 'an adjustment layer must disable the GPU shader path').not.toContain('gpu.render');
      expect(names).toContain('hi.blit');
    }
    const baseline = await readCanvas(page);

    // Left: rect selection moved into the open layer through the editor.
    await selectTool(page, 'Rect');
    await rectDrag(page, 0.1, 0.2, 0.42, 0.8);
    await clickTab(page, 'Layers');
    let blits = await markCount(page, 'hi.blit');
    await takeSelection(page).click();
    await setRange(page, /^Brightness/, 0.45);
    await waitMark(page, 'hi.blit', blits);
    await settle(page);

    // Right: rect selection that creates its own layer from the add row.
    await selectTool(page, 'Rect');
    await rectDrag(page, 0.58, 0.2, 0.9, 0.8);
    await clickTab(page, 'Layers');
    blits = await markCount(page, 'hi.blit');
    await addAdjustFromSel(page).click();
    await setRange(page, /^Contrast/, -0.5);
    await waitMark(page, 'hi.blit', blits);
    await settle(page, 600);

    await clickTab(page, 'Layers');
    await expect(page.locator('.layer-row')).toHaveCount(2);

    const preview = await readCanvas(page);
    expect({ w: preview.w, h: preview.h }).toEqual({ w: W, h: H });
    const leftBefore = regionDiff(baseline.bytes, preview.bytes, W, H, 0.14, 0.24, 0.38, 0.76);
    const rightBefore = regionDiff(baseline.bytes, preview.bytes, W, H, 0.62, 0.24, 0.86, 0.76);
    const topBefore = regionDiff(baseline.bytes, preview.bytes, W, H, 0.45, 0.02, 0.55, 0.12);
    expect(leftBefore, 'the left masked layer did not change its region').toBeGreaterThan(3);
    expect(rightBefore, 'the right masked layer did not change its region').toBeGreaterThan(3);
    expect(topBefore, 'a masked layer leaked outside its mask').toBeLessThanOrEqual(1);

    const png = await exportPng(page);
    const exported = await decodePng(page, png);
    expect({ w: exported.w, h: exported.h }).toEqual({ w: W, h: H });
    const db = psnr(preview.bytes, exported.bytes);
    const leftOut = regionDiff(preview.bytes, exported.bytes, W, H, 0.14, 0.24, 0.38, 0.76);
    const rightOut = regionDiff(preview.bytes, exported.bytes, W, H, 0.62, 0.24, 0.86, 0.76);
    recordMetrics(testInfo, {
      gpu_draws_before_layer: gpuInitial,
      gpu_draws_after_layer: (await markNames(page)).filter((n) => n === 'gpu.render').length,
      adj_left_region_delta: Math.round(leftBefore * 100) / 100,
      adj_right_region_delta: Math.round(rightBefore * 100) / 100,
      adj_outside_region_delta: Math.round(topBefore * 100) / 100,
      adj_parity_psnr_db: Math.round(db * 100) / 100,
      adj_parity_left_delta: Math.round(leftOut * 100) / 100,
      adj_parity_right_delta: Math.round(rightOut * 100) / 100,
    });
    expect(db, `two masked layers preview vs export PSNR ${db.toFixed(2)} dB < 33 dB`).toBeGreaterThanOrEqual(33);
    expect(leftOut, 'preview vs export differ inside the left mask').toBeLessThanOrEqual(1);
    expect(rightOut, 'preview vs export differ inside the right mask').toBeLessThanOrEqual(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('rect and lasso masks confine their layer and survive a reload', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);
    const baseline = await readCanvas(page);

    // Layer 1: rect mask, brightness.
    await selectTool(page, 'Rect');
    await rectDrag(page, 0.1, 0.2, 0.45, 0.8);
    await clickTab(page, 'Layers');
    await addAdjustFromSel(page).click();
    let blits = await markCount(page, 'hi.blit');
    await setRange(page, /^Brightness/, 0.45);
    await waitMark(page, 'hi.blit', blits);
    await settle(page);

    // Layer 2: lasso mask, saturation.
    await selectTool(page, 'Lasso');
    await lassoDrag(page, [
      [0.6, 0.3],
      [0.9, 0.35],
      [0.85, 0.7],
      [0.65, 0.65],
    ]);
    await clickTab(page, 'Layers');
    blits = await markCount(page, 'hi.blit');
    await addAdjust(page).click();
    await takeSelection(page).click();
    await setRange(page, /^Saturation/, -0.8);
    await waitMark(page, 'hi.blit', blits);
    await settle(page, 600);

    const masked = await readCanvas(page);
    const leftOn = regionDiff(baseline.bytes, masked.bytes, W, H, 0.15, 0.25, 0.4, 0.75);
    const rightOn = regionDiff(baseline.bytes, masked.bytes, W, H, 0.66, 0.36, 0.84, 0.64);
    const outside = regionDiff(baseline.bytes, masked.bytes, W, H, 0.45, 0.02, 0.55, 0.12);
    expect(leftOn, 'rect-masked layer did not change its region').toBeGreaterThan(3);
    expect(rightOn, 'lasso-masked layer did not change its region').toBeGreaterThan(3);
    expect(outside, 'masked layers leaked outside their masks').toBeLessThanOrEqual(1);

    // Debounced persist (800 ms) + blob writes.
    await page.waitForTimeout(2200);
    await page.reload();
    await expect(thumbs(page)).toHaveCount(1, { timeout: 60_000 });
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await expect.poll(() => canvasSize(page).then((s) => ({ w: s.w, h: s.h })), { timeout: 30_000 })
      .toEqual({ w: W, h: H });
    await settle(page, 800);
    const restored = await readCanvas(page);

    await clickTab(page, 'Layers');
    await expect(page.locator('.layer-row')).toHaveCount(2);
    // Click the label, not the row: the row's centre is its opacity slider.
    // The panel lists the topmost layer first: the lasso layer was added last.
    await page.locator('.layer-row').first().locator('.layer-label').click();
    expect(await layerSliderValue(page, 2), 'lasso layer saturation lost on reload').toBeCloseTo(-0.8, 2);
    await page.locator('.layer-row').nth(1).locator('.layer-label').click();
    expect(await layerSliderValue(page, 0), 'rect layer brightness lost on reload').toBeCloseTo(0.45, 2);

    const reloadMean = meanDiff(masked.bytes, restored.bytes);
    recordMetrics(testInfo, {
      mask_reload_mean_delta: Math.round(reloadMean * 100) / 100,
      mask_reload_psnr_db: Math.round(psnr(masked.bytes, restored.bytes) * 100) / 100,
      rect_region_delta: Math.round(leftOn * 100) / 100,
      lasso_region_delta: Math.round(rightOn * 100) / 100,
      outside_region_delta: Math.round(outside * 100) / 100,
    });
    expect(reloadMean, 'reloaded frame differs from the edited frame').toBeLessThanOrEqual(1);
    expect(regionDiff(baseline.bytes, restored.bytes, W, H, 0.15, 0.25, 0.4, 0.75)).toBeGreaterThan(3);
    expect(regionDiff(baseline.bytes, restored.bytes, W, H, 0.66, 0.36, 0.84, 0.64)).toBeGreaterThan(3);
    expect(regionDiff(baseline.bytes, restored.bytes, W, H, 0.45, 0.02, 0.55, 0.12)).toBeLessThanOrEqual(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('a layer edit forces a re-render and stacking order changes the export', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);
    const baseline = await readCanvas(page);

    await clickTab(page, 'Layers');
    let blits = await markCount(page, 'hi.blit');
    await addAdjust(page).click();
    // Keep headroom: a +45% brightness lift clips this teal fixture to white
    // and makes the contrast layer's effect nearly invisible.
    await setRange(page, /^Brightness/, 0.2);
    await waitMark(page, 'hi.blit', blits);

    blits = await markCount(page, 'hi.blit');
    await addAdjust(page).click();
    await setRange(page, /^Contrast/, 0.4);
    await waitMark(page, 'hi.blit', blits);
    await settle(page, 600);
    const stacked = await readCanvas(page);
    expect(meanDiff(baseline.bytes, stacked.bytes), 'stacked layers did not change the frame').toBeGreaterThan(3);
    const exportA = await decodePng(page, await exportPng(page));

    // Changing a layer value must invalidate the sharp-frame fingerprint.
    await clickTab(page, 'Layers');
    await page.locator('.layer-row').nth(1).locator('.layer-label').click();
    blits = await markCount(page, 'hi.blit');
    const requestsBefore = await markCount(page, 'hi.request');
    await setRange(page, /^Contrast/, 0.1);
    await waitMark(page, 'hi.blit', blits);
    expect(await markCount(page, 'hi.request'), 'layer edit did not request a new sharp frame')
      .toBeGreaterThan(requestsBefore);
    await settle(page, 600);
    const edited = await readCanvas(page);
    const exportB = await decodePng(page, await exportPng(page));
    const editDelta = meanDiff(exportA.bytes, exportB.bytes);
    expect(editDelta, 'layer edit did not change the export').toBeGreaterThan(1);

    // Move the top layer down one slot; the point ops do not commute.
    await clickTab(page, 'Layers');
    blits = await markCount(page, 'hi.blit');
    await page.locator('.layer-row').first().locator('button.icon-btn', { hasText: '↓' }).click();
    await waitMark(page, 'hi.blit', blits);
    await settle(page, 600);
    const reordered = await readCanvas(page);
    const exportC = await decodePng(page, await exportPng(page));
    const reorderDelta = meanDiff(exportB.bytes, exportC.bytes);
    recordMetrics(testInfo, {
      stacked_vs_baseline_delta: Math.round(meanDiff(baseline.bytes, stacked.bytes) * 100) / 100,
      layer_edit_export_delta: Math.round(editDelta * 100) / 100,
      layer_reorder_export_delta: Math.round(reorderDelta * 100) / 100,
      layer_edit_preview_delta: Math.round(meanDiff(stacked.bytes, edited.bytes) * 100) / 100,
      layer_reorder_preview_delta: Math.round(meanDiff(edited.bytes, reordered.bytes) * 100) / 100,
    });
    expect(reorderDelta, 'reordering the layers did not change the export').toBeGreaterThan(1);
    expect(meanDiff(edited.bytes, reordered.bytes), 'reordering did not change the preview').toBeGreaterThan(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('a session without adjustment layers restores and renders identically', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);
    await clickTab(page, 'Color');
    const blits = await markCount(page, 'hi.blit');
    await setRange(page, /^Brightness/, 0.3);
    await waitMark(page, 'hi.blit', blits);
    await settle(page, 600);
    const before = await readCanvas(page);

    // Debounced persist; this record has no `layers`/`nlid` entries, exactly
    // the shape a session saved before adjustment layers shipped.
    await page.waitForTimeout(2200);
    await page.reload();
    await expect(thumbs(page)).toHaveCount(1, { timeout: 60_000 });
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await expect.poll(() => canvasSize(page).then((s) => ({ w: s.w, h: s.h })), { timeout: 30_000 })
      .toEqual({ w: W, h: H });
    await settle(page, 800);

    await clickTab(page, 'Layers');
    await expect(page.locator('.layer-row')).toHaveCount(0);
    await clickTab(page, 'Color');
    const brightness = await page.locator('label.slider', { hasText: /^Brightness/ }).locator('input').inputValue();
    expect(Number(brightness), 'global brightness lost on restore').toBeCloseTo(0.3, 2);

    const after = await readCanvas(page);
    const delta = meanDiff(before.bytes, after.bytes);
    const db = psnr(before.bytes, after.bytes);
    recordMetrics(testInfo, {
      legacy_restore_mean_delta: Math.round(delta * 100) / 100,
      legacy_restore_psnr_db: Math.round(db * 100) / 100,
    });
    expect(delta, 'legacy session restore changed the frame').toBeLessThanOrEqual(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  // A RAW shows from its proxy, which lives under a different worker buffer id
  // than the item; raster (wand) masks are sent to the worker by id, so this
  // covers the preview of a raster-masked layer on the proxy path.
  const RAW = rawFixtures()[0];
  test('a wand-masked layer on a RAW shows in the preview and matches the export', async ({ page }, testInfo) => {
    test.skip(!RAW, 'no RAW fixture in dl-raw');
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page, './?gpu=1');
    await importFiles(page, [RAW]);
    await expect.poll(
      () => page.evaluate(() => ((window as any).__pes?.perf?.() || []).some((e: any) => e.name === 'proxy.raw_develop' || e.name === 'pool.develop')),
      { timeout: 120_000 },
    ).toBe(true);
    await expect(page.locator('.developing-badge')).toHaveCount(0, { timeout: 60_000 });
    await settle(page, 1000);
    const SMALL = 512;
    // Downscale the stage canvas (or a decoded export) in the page; RAW frames
    // are far too large to ship to the test process.
    const small = (src: 'canvas' | string) => page.evaluate(async ([src, W]) => {
      let img: CanvasImageSource & { width: number; height: number };
      let close = () => {};
      if (src === 'canvas') {
        img = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement;
      } else {
        const bin = atob(src as string);
        const u8 = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
        const bmp = await createImageBitmap(new Blob([u8], { type: 'image/png' }));
        img = bmp;
        close = () => bmp.close();
      }
      const H = Math.round(((W as number) * 2) / 3);
      const c = document.createElement('canvas');
      c.width = W as number;
      c.height = H;
      const x = c.getContext('2d')!;
      x.imageSmoothingQuality = 'high';
      x.drawImage(img, 0, 0, W as number, H);
      close();
      const d = x.getImageData(0, 0, W as number, H).data;
      let s = '';
      for (let i = 0; i < d.length; i += 0x8000) {
        s += String.fromCharCode.apply(null, Array.from(d.subarray(i, i + 0x8000)) as number[]);
      }
      return btoa(s);
    }, [src, SMALL] as const).then(b64ToBytes);
    const baseline = await small('canvas');

    // Non-contiguous wand at full tolerance: a raster mask covering most of
    // the frame, so the layer's effect is unmistakable.
    await clickTab(page, 'Select');
    await page.locator('.select-tab .chip', { hasText: /^Wand$/ }).click();
    await setRange(page, /^Tolerance/, 100);
    const contiguous = page.locator('.select-tab label.row', { hasText: 'Contiguous' }).locator('input');
    if (await contiguous.isChecked()) await contiguous.uncheck();
    const b = await wrapBox(page);
    await page.mouse.click(b.x + b.width * 0.5, b.y + b.height * 0.5);
    await clickTab(page, 'Layers');
    await expect(addAdjustFromSel(page)).toBeEnabled({ timeout: 10_000 });
    await addAdjustFromSel(page).click();
    await setRange(page, /^Brightness/, 0.45);
    await settle(page, 1500);
    const layered = await small('canvas');
    const effect = meanDiff(baseline, layered);

    const png = await exportPng(page);
    const exported = await small(png.toString('base64'));
    // Compare at 128 px (4x4 box average of the 512 px frames): a
    // non-contiguous wand mask on real foliage is noise-like at its 1024 px
    // working resolution, so per-pixel comparisons at 512 px mostly measure
    // how two different downscales alias that pattern, not mask placement.
    const box4 = (f: Buffer) => {
      const w = SMALL, h = Math.round((SMALL * 2) / 3), tw = w / 4, th = Math.floor(h / 4);
      const out = Buffer.alloc(tw * th * 4);
      for (let y = 0; y < th; y++) for (let x = 0; x < tw; x++) for (let c = 0; c < 4; c++) {
        let sum = 0;
        for (let dy = 0; dy < 4; dy++) for (let dx = 0; dx < 4; dx++) sum += f[((y * 4 + dy) * w + x * 4 + dx) * 4 + c];
        out[(y * tw + x) * 4 + c] = Math.round(sum / 16);
      }
      return out;
    };
    const db = psnr(box4(layered), box4(exported));
    recordMetrics(testInfo, {
      raw_wand_layer_preview_delta: Math.round(effect * 100) / 100,
      raw_wand_layer_parity_psnr_db: Math.round(db * 100) / 100,
    });
    expect(effect, 'the wand-masked layer did not change the RAW preview').toBeGreaterThan(3);
    expect(db, `RAW wand layer preview vs export PSNR ${db.toFixed(2)} dB at 128 px < 30 dB`).toBeGreaterThanOrEqual(30);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  // setRange sets a value with one scripted event, so it can't see an editor
  // that rebuilds itself mid-drag. This drags with the mouse like a person.
  test('layer sliders follow a real mouse drag, and rows are told apart', async ({ page }) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);
    const baseline = await readCanvas(page);

    await clickTab(page, 'Layers');
    const labels = () => page.locator('.layer-row .layer-label').allTextContents();
    await addAdjust(page).click();
    await addAdjust(page).click();
    expect(await labels()).toEqual(['Adjust 2', 'Adjust 1']);
    await page.locator('.layer-row').first().locator('button.icon-btn', { hasText: '↓' }).click();
    expect(await labels(), 'moving a layer must show in the list').toEqual(['Adjust 1', 'Adjust 2']);
    await expect(page.locator('.layer-row.selected .layer-label')).toHaveText('Adjust 2');

    const input = page.locator('.adjust-editor label.slider', { hasText: /^Brightness/ }).locator('input');
    await input.evaluate((el) => { (el as any).__dragTag = 1; });
    const b = (await input.boundingBox())!;
    const y = b.y + b.height / 2;
    // Grab the thumb where a person would (value 0 sits mid-track) and drag
    // past the right end.
    await page.mouse.move(b.x + b.width / 2, y);
    await page.mouse.down();
    for (let i = 1; i <= 20; i++) {
      await page.mouse.move(b.x + b.width / 2 + (b.width / 2 + 10) * (i / 20), y);
      await page.waitForTimeout(16);
    }
    await page.mouse.up();
    expect(await input.inputValue(), 'the drag stopped short of the right end').toBe('1');
    await expect(page.locator('.adjust-editor label.slider', { hasText: /^Brightness/ }).locator('span').first())
      .toHaveText('Brightness: +100%');
    expect(await input.evaluate((el) => (el as any).__dragTag === 1), 'the slider was rebuilt mid-drag').toBe(true);
    await settle(page, 600);
    expect(meanDiff(baseline.bytes, (await readCanvas(page)).bytes), 'the drag did not change the photo')
      .toBeGreaterThan(3);

    await selectTool(page, 'Rect');
    await rectDrag(page, 0.2, 0.2, 0.6, 0.6);
    await clickTab(page, 'Layers');
    await addAdjustFromSel(page).click();
    expect(await labels()).toEqual(['Adjust 3', 'Adjust 1', 'Adjust 2']);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  // Owner request (8 Nov): with an adjustment selected, the Select tab shows
  // and edits its mask. Selections stay ordinary selections (so "+ Adjust from
  // sel." keeps working) and are folded in with the mask panel's buttons.
  // Exports carry no editor tint, so they show where the adjustment applies.
  test('the Select tab\'s mask panel adds selections to and removes them from an adjustment mask', async ({ page }) => {
    test.setTimeout(240_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);
    const base = await decodePng(page, await exportPng(page));
    const lift = (f: { w: number; h: number; bytes: Buffer }, x0: number, y0: number, x1: number, y1: number) =>
      regionDiff(base.bytes, f.bytes, f.w, f.h, x0, y0, x1, y1);
    const A = [0.25, 0.25, 0.45, 0.45] as const; // inside the first rect
    const B = [0.65, 0.65, 0.8, 0.8] as const; // added later
    const C = [0.05, 0.75, 0.15, 0.9] as const; // never selected
    const panel = page.locator('.select-tab .mask-panel');
    const copyToLayer = page.locator('.select-row button', { hasText: /^Copy to layer$/ });

    await selectTool(page, 'Rect');
    await rectDrag(page, 0.2, 0.2, 0.5, 0.5);
    await clickTab(page, 'Layers');
    await addAdjustFromSel(page).click();
    await setRange(page, /^Brightness/, 0.5);
    await settle(page, 600);
    // The Layers toolbar offers only Brush for an adjustment.
    await expect(page.locator('.toolbar .chip', { hasText: /^Pen$/ })).toBeHidden();

    await clickTab(page, 'Select');
    await expect(panel.locator('.editing-line')).toContainText('Editing: Adjust mask');
    await expect(panel.getByRole('button', { name: 'Add selection to mask' })).toBeDisabled();

    // A new rect is an ordinary selection until it is added.
    await rectDrag(page, 0.6, 0.6, 0.85, 0.85);
    await expect(copyToLayer).toBeEnabled();
    await panel.getByRole('button', { name: 'Add selection to mask' }).click();
    await settle(page, 600);
    await expect(copyToLayer, 'adding to the mask should use up the selection').toBeDisabled();
    let e = await decodePng(page, await exportPng(page));
    expect(lift(e, ...A), 'the first area lost the effect').toBeGreaterThan(3);
    expect(lift(e, ...B), 'Add selection to mask did not extend the effect').toBeGreaterThan(3);
    expect(lift(e, ...C), 'the effect leaked outside the mask').toBeLessThanOrEqual(1);

    // Remove part of it, then undo that.
    await clickTab(page, 'Select');
    await rectDrag(page, 0.15, 0.15, 0.55, 0.55);
    await panel.getByRole('button', { name: 'Remove selection from mask' }).click();
    await settle(page, 600);
    e = await decodePng(page, await exportPng(page));
    expect(lift(e, ...A), 'Remove selection from mask did not take the effect away').toBeLessThanOrEqual(1);
    expect(lift(e, ...B)).toBeGreaterThan(3);
    await clickTab(page, 'Select');
    await panel.getByRole('button', { name: 'Undo' }).click();
    await settle(page, 600);
    e = await decodePng(page, await exportPng(page));
    expect(lift(e, ...A), 'Undo did not restore the removed area').toBeGreaterThan(3);

    // Clear mask: the adjustment applies everywhere again.
    await clickTab(page, 'Select');
    await panel.getByRole('button', { name: 'Clear mask' }).click();
    await settle(page, 600);
    e = await decodePng(page, await exportPng(page));
    expect(lift(e, ...C), 'after Clear mask the adjustment should apply everywhere').toBeGreaterThan(3);

    // Back to the photo's own selection.
    await clickTab(page, 'Select');
    await panel.getByRole('button', { name: "Edit the photo's selection instead" }).click();
    await expect(page.locator('.select-tab > .editing-line')).toHaveText("Editing: the photo's selection");
    await expect(panel).toHaveCount(0);

    // The Layers-tab Brush opens the Select tab's mask brush.
    await clickTab(page, 'Layers');
    await page.locator('.layer-row .layer-label', { hasText: /^Adjust/ }).click();
    await page.locator('.toolbar .chip', { hasText: /^Brush$/ }).first().click();
    await expect(page.locator('.tabs button.active')).toHaveText('Select');
    await expect(page.locator('.select-tab .chip.active', { hasText: /^Brush$/ })).toHaveCount(1);
    await expect(panel.locator('.editing-line')).toContainText('Editing: Adjust mask');
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
