import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import {
  openApp, trackErrors, recordMetrics, importFiles, canvas, canvasSize, clickTab, setRange,
  thumbs, PNG_SMALL,
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
    await page.locator('.layer-row').first().locator('.layer-label').click();
    expect(await layerSliderValue(page, 0), 'rect layer brightness lost on reload').toBeCloseTo(0.45, 2);
    await page.locator('.layer-row').nth(1).locator('.layer-label').click();
    expect(await layerSliderValue(page, 2), 'lasso layer saturation lost on reload').toBeCloseTo(-0.8, 2);

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

    // Move the bottom layer up one slot; the point ops do not commute.
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
});
