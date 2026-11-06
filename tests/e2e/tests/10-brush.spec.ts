import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import {
  openApp, trackErrors, recordMetrics, importFiles, canvas, canvasSize, clickTab, setRange,
  thumbs, PNG_SMALL, JPEG_A, rawFixtures,
} from './helpers';

// Selection brush (approved scope, issue #3 comment 6681440642): Add/Remove,
// size/softness, per-stroke undo, layer-mask targeting, reload persistence,
// preview-vs-export parity >= 30 dB at 128 px, no main-thread long task > 50 ms
// during a stroke, and <= 700 ms preview settle on a 24 MP ARW.

type Frame = { w: number; h: number; bytes: Buffer };
type Mark = { name: string; dur_ms: number; detail?: string };

const SMALL = 800;

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
    .toEqual({ w: SMALL, h: 600 });
  await settle(page);
}

async function wrapBox(page: Page) {
  const b = await page.locator('.canvas-wrap').boundingBox();
  if (!b) throw new Error('no .canvas-wrap box');
  return b;
}

/** Park the pointer on the top-left corner so the brush ring renders deterministically. */
async function parkPointer(page: Page) {
  await page.mouse.move(2, 2);
  await page.waitForTimeout(80);
}

/** Drag a brush stroke; returns the wall-clock time at pointerup. */
async function stroke(page: Page, pts: [number, number][], steps = 6): Promise<number> {
  const b = await wrapBox(page);
  await page.mouse.move(b.x + b.width * pts[0][0], b.y + b.height * pts[0][1]);
  await page.mouse.down();
  for (const [x, y] of pts.slice(1)) {
    await page.mouse.move(b.x + b.width * x, b.y + b.height * y, { steps });
  }
  await page.mouse.up();
  const tUp = Date.now();
  await page.waitForTimeout(100);
  return tUp;
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

const selectChip = (page: Page, name: string) =>
  page.locator('.select-tab .chip', { hasText: new RegExp(`^${name}$`) });

async function selectBrush(page: Page) {
  await clickTab(page, 'Select');
  await selectChip(page, 'Brush').click();
}

async function selectRect(page: Page) {
  await clickTab(page, 'Select');
  await selectChip(page, 'Rect').click();
}

const addAdjust = (page: Page) =>
  page.locator('button.btn', { hasText: /＋ Adjust\s*$/ });

async function exportPng(page: Page): Promise<Buffer> {
  await clickTab(page, 'Export');
  await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
  const [dl] = await Promise.all([
    page.waitForEvent('download', { timeout: 120_000 }),
    page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
  ]);
  return fs.readFileSync((await dl.path())!);
}

/** Downscale the stage canvas or a decoded export to a 128 px frame. */
async function smallFrame(page: Page, src: 'canvas' | Buffer, longEdge = 128): Promise<Frame> {
  const arg = src === 'canvas' ? null : src.toString('base64');
  const r = await page.evaluate(async ([src, longEdge]) => {
    let img: CanvasImageSource & { width: number; height: number };
    let close = () => {};
    if (src === null) {
      img = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement;
    } else {
      const bin = atob(src as string);
      const u8 = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
      const bmp = await createImageBitmap(new Blob([u8], { type: 'image/png' }));
      img = bmp;
      close = () => bmp.close();
    }
    const scale = Math.min(1, (longEdge as number) / Math.max(img.width, img.height));
    const w = Math.max(1, Math.round(img.width * scale));
    const h = Math.max(1, Math.round(img.height * scale));
    const c = document.createElement('canvas');
    c.width = w;
    c.height = h;
    const x = c.getContext('2d')!;
    x.imageSmoothingEnabled = true;
    x.imageSmoothingQuality = 'high';
    x.drawImage(img, 0, 0, w, h);
    close();
    const d = x.getImageData(0, 0, w, h).data;
    let s = '';
    for (let i = 0; i < d.length; i += 0x8000) {
      s += String.fromCharCode.apply(null, Array.from(d.subarray(i, i + 0x8000)) as number[]);
    }
    return { w, h, b64: btoa(s) };
  }, [arg, longEdge] as const);
  return { w: r.w, h: r.h, bytes: b64ToBytes(r.b64) };
}

function installLongTaskProbe(page: Page) {
  return page.evaluate(() => {
    const w = window as any;
    w.__brushLong = [] as number[];
    try {
      const po = new PerformanceObserver((list) => {
        for (const e of list.getEntries()) w.__brushLong.push(e.duration);
      });
      po.observe({ entryTypes: ['longtask'] });
      w.__brushPo = po;
    } catch {
      w.__brushLong = [-1];
    }
  });
}

const readLongTasks = (page: Page): Promise<number[]> =>
  page.evaluate(() => ((window as any).__brushLong as number[]) || []);

const clearLongTasks = (page: Page) =>
  page.evaluate(() => { (window as any).__brushLong = []; });

test.describe('selection brush (issue #3)', () => {
  test('the brush adds and removes strokes with per-stroke undo', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);

    await selectBrush(page);
    await setRange(page, /^Size:/, 0.2);
    await setRange(page, /^Softness:/, 0);
    await parkPointer(page);
    await settle(page);
    const baseline = await readCanvas(page);
    const { w, h } = baseline;

    await installLongTaskProbe(page);
    const maxLongTasks: number[] = [];

    // Stroke 1: left third.
    await clearLongTasks(page);
    await stroke(page, [[0.2, 0.5], [0.4, 0.5]]);
    maxLongTasks.push(...(await readLongTasks(page)));
    await parkPointer(page);
    await settle(page);
    const added = await readCanvas(page);
    const stroke1 = regionDiff(baseline.bytes, added.bytes, w, h, 0.16, 0.4, 0.44, 0.6);
    const outside1 = regionDiff(baseline.bytes, added.bytes, w, h, 0.7, 0.05, 0.95, 0.3);
    expect(stroke1, 'the Add stroke did not tint its region').toBeGreaterThan(3);
    expect(outside1, 'the Add stroke tinted outside its path').toBeLessThanOrEqual(1);
    await expect(selectChip(page, 'Undo')).toBeEnabled();

    // Stroke 2: right side.
    await clearLongTasks(page);
    await stroke(page, [[0.62, 0.3], [0.82, 0.3]]);
    maxLongTasks.push(...(await readLongTasks(page)));
    await parkPointer(page);
    await settle(page);
    const added2 = await readCanvas(page);
    const stroke2 = regionDiff(added.bytes, added2.bytes, w, h, 0.58, 0.2, 0.86, 0.4);
    expect(stroke2, 'the second stroke did not tint its region').toBeGreaterThan(3);

    // One undo step per stroke.
    await selectChip(page, 'Undo').click();
    await parkPointer(page);
    await settle(page);
    const undone1 = await readCanvas(page);
    expect(meanDiff(added.bytes, undone1.bytes), 'undo did not restore the previous stroke state').toBeLessThanOrEqual(1);
    await expect(selectChip(page, 'Redo')).toBeEnabled();

    await selectChip(page, 'Redo').click();
    await parkPointer(page);
    await settle(page);
    const redone = await readCanvas(page);
    expect(meanDiff(added2.bytes, redone.bytes), 'redo did not restore the second stroke').toBeLessThanOrEqual(1);

    await selectChip(page, 'Undo').click();
    await selectChip(page, 'Undo').click();
    await parkPointer(page);
    await settle(page);
    const undoneAll = await readCanvas(page);
    expect(meanDiff(baseline.bytes, undoneAll.bytes), 'undoing every stroke did not restore the unselected frame').toBeLessThanOrEqual(1);
    await expect(selectChip(page, 'Undo')).toBeDisabled();
    await expect(selectChip(page, 'Redo')).toBeEnabled();

    // Remove over the same path at softness 0 restores the prior (empty) mask.
    await clearLongTasks(page);
    await stroke(page, [[0.2, 0.75], [0.4, 0.75]]);
    maxLongTasks.push(...(await readLongTasks(page)));
    await parkPointer(page);
    await settle(page);
    const added3 = await readCanvas(page);
    expect(regionDiff(baseline.bytes, added3.bytes, w, h, 0.16, 0.65, 0.44, 0.85),
      'the third Add stroke did not tint its region').toBeGreaterThan(3);

    await selectChip(page, 'Remove').click();
    await clearLongTasks(page);
    await stroke(page, [[0.2, 0.75], [0.4, 0.75]]);
    maxLongTasks.push(...(await readLongTasks(page)));
    await parkPointer(page);
    await settle(page);
    const removed = await readCanvas(page);
    expect(meanDiff(baseline.bytes, removed.bytes),
      'Add then Remove at softness 0 did not restore the prior mask exactly').toBeLessThanOrEqual(1);

    // The Remove is its own undo step.
    await selectChip(page, 'Undo').click();
    await parkPointer(page);
    await settle(page);
    const removeUndone = await readCanvas(page);
    expect(meanDiff(added3.bytes, removeUndone.bytes), 'undo did not revert the Remove stroke').toBeLessThanOrEqual(1);

    const maxLong = maxLongTasks.reduce((m, v) => Math.max(m, v), 0);
    recordMetrics(testInfo, {
      brush_stroke_region_delta: Math.round(stroke1 * 100) / 100,
      brush_outside_region_delta: Math.round(outside1 * 100) / 100,
      brush_undo_restore_delta: Math.round(meanDiff(added.bytes, undone1.bytes) * 100) / 100,
      brush_add_remove_restore_delta: Math.round(meanDiff(baseline.bytes, removed.bytes) * 100) / 100,
      brush_longtask_max_ms: Math.round(maxLong * 10) / 10,
      brush_longtasks_over_50ms: maxLongTasks.filter((v) => v > 50).length,
    });
    expect(maxLongTasks.filter((v) => v > 50), 'a brush stroke produced a main-thread long task > 50 ms').toEqual([]);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('the first stroke bakes a vector selection and tints while dragging', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);

    await selectRect(page);
    await rectDrag(page, 0.6, 0.2, 0.85, 0.4);
    await parkPointer(page);
    await settle(page);
    const vector = await readCanvas(page);
    const { w, h } = vector;

    await selectBrush(page);
    await setRange(page, /^Size:/, 0.15);
    await setRange(page, /^Softness:/, 0);
    await parkPointer(page);
    await settle(page);
    const before = await readCanvas(page);

    // Hold the button and drag: the live overlay must show the baked rect and
    // the in-flight stroke before release.
    const b = await wrapBox(page);
    await page.mouse.move(b.x + b.width * 0.2, b.y + b.height * 0.5);
    await page.mouse.down();
    await page.mouse.move(b.x + b.width * 0.3, b.y + b.height * 0.5, { steps: 6 });
    await page.waitForTimeout(150);
    const live = await readCanvas(page);
    const rectLive = regionDiff(before.bytes, live.bytes, w, h, 0.63, 0.23, 0.82, 0.37);
    const strokeLive = regionDiff(before.bytes, live.bytes, w, h, 0.17, 0.42, 0.33, 0.58);
    expect(rectLive, 'the vector selection was not baked into the first stroke').toBeGreaterThan(3);
    expect(strokeLive, 'the in-flight stroke did not tint live').toBeGreaterThan(3);

    await page.mouse.up();
    await parkPointer(page);
    await settle(page);
    const released = await readCanvas(page);
    expect(regionDiff(before.bytes, released.bytes, w, h, 0.63, 0.23, 0.82, 0.37),
      'the baked rect disappeared after release').toBeGreaterThan(3);

    // Undo restores the rasterized rect, not the vector outline.
    await selectChip(page, 'Undo').click();
    await parkPointer(page);
    await settle(page);
    const undone = await readCanvas(page);
    const rectAfterUndo = regionDiff(before.bytes, undone.bytes, w, h, 0.63, 0.23, 0.82, 0.37);
    const strokeAfterUndo = regionDiff(before.bytes, undone.bytes, w, h, 0.17, 0.42, 0.33, 0.58);
    recordMetrics(testInfo, {
      brush_live_rect_delta: Math.round(rectLive * 100) / 100,
      brush_live_stroke_delta: Math.round(strokeLive * 100) / 100,
      brush_baked_rect_after_undo_delta: Math.round(rectAfterUndo * 100) / 100,
      brush_stroke_after_undo_delta: Math.round(strokeAfterUndo * 100) / 100,
    });
    expect(rectAfterUndo, 'undo lost the rasterized selection').toBeGreaterThan(3);
    expect(strokeAfterUndo, 'undo did not remove the stroke').toBeLessThanOrEqual(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('a brushed adjustment-layer mask persists and leaves the global selection untouched', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);

    // Zero-valued adjustment layer: the mask must persist without changing
    // the rendered frame, so any frame difference isolates the overlays.
    await clickTab(page, 'Layers');
    await addAdjust(page).click();
    await expect(page.locator('.layer-row')).toHaveCount(1);
    await settle(page);

    // Global rect selection, drawn while the layer stays selected.
    await selectRect(page);
    await rectDrag(page, 0.15, 0.25, 0.45, 0.75);
    await parkPointer(page);
    await settle(page);
    const withRect = await readCanvas(page);

    // Brush the layer mask; the target hint must say the layer, not global.
    await selectBrush(page);
    await expect(page.locator('.select-tab p.dim', { hasText: /^Brush edits the selected adjustment layer's mask/ })).toBeVisible();
    await setRange(page, /^Size:/, 0.15);
    await setRange(page, /^Softness:/, 0);
    await parkPointer(page);
    await settle(page);
    const beforeStroke = await readCanvas(page);

    await stroke(page, [[0.55, 0.55], [0.78, 0.55]]);
    await parkPointer(page);
    await settle(page);
    const tinted = await readCanvas(page);
    const { w, h } = tinted;
    const layerTint = regionDiff(beforeStroke.bytes, tinted.bytes, w, h, 0.5, 0.45, 0.84, 0.65);
    expect(layerTint, 'the brush did not tint the adjustment layer mask').toBeGreaterThan(3);

    // With the brush closed, the Select tab shows the global selection again.
    await selectRect(page);
    await parkPointer(page);
    await settle(page);
    const afterRect = await readCanvas(page);
    const globalDiff = meanDiff(withRect.bytes, afterRect.bytes);
    expect(globalDiff, 'brushing the layer mask changed the global selection').toBeLessThanOrEqual(1);

    // Reload: the brushed mask is stored with the layer.
    await page.waitForTimeout(2200);
    await page.reload();
    await expect(thumbs(page)).toHaveCount(1, { timeout: 60_000 });
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await waitSmall(page);
    await clickTab(page, 'Layers');
    await expect(page.locator('.layer-row')).toHaveCount(1);
    await page.locator('.layer-row').first().locator('.layer-label').click();
    await selectBrush(page);
    await setRange(page, /^Size:/, 0.15);
    await setRange(page, /^Softness:/, 0);
    await parkPointer(page);
    await settle(page, 800);
    const restored = await readCanvas(page);
    const reloadDiff = meanDiff(tinted.bytes, restored.bytes);

    recordMetrics(testInfo, {
      brush_layer_tint_delta: Math.round(layerTint * 100) / 100,
      brush_global_untouched_delta: Math.round(globalDiff * 100) / 100,
      brush_mask_reload_delta: Math.round(reloadDiff * 100) / 100,
    });
    expect(reloadDiff, 'the brushed layer mask was not restored on reload').toBeLessThanOrEqual(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('a brushed layer mask matches the PNG export', async ({ page }, testInfo) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [JPEG_A]);
    await expect.poll(() => canvasSize(page).then((s) => s.w), { timeout: 60_000 }).toBeGreaterThan(0);
    await settle(page, 800);
    const baseline = await smallFrame(page, 'canvas');

    await clickTab(page, 'Layers');
    await addAdjust(page).click();
    await setRange(page, /^Brightness/, 0.5);
    await settle(page, 600);

    await selectBrush(page);
    await setRange(page, /^Size:/, 0.25);
    await setRange(page, /^Softness:/, 0.4);
    await parkPointer(page);

    const blits0 = await markCount(page, 'hi.blit');
    const tUp = await stroke(page, [[0.48, 0.25], [0.52, 0.75]], 8);
    await expect.poll(() => markCount(page, 'hi.blit'), { timeout: 20_000, intervals: [20, 20, 50] })
      .toBeGreaterThan(blits0);
    const settleMs = Date.now() - tUp;

    // The select-tab tint must not leak into the parity read.
    await clickTab(page, 'Layers');
    await parkPointer(page);
    await settle(page, 800);
    const preview = await smallFrame(page, 'canvas');
    const png = await exportPng(page);
    const exported = await smallFrame(page, png);

    expect({ w: preview.w, h: preview.h }).toEqual({ w: exported.w, h: exported.h });
    const db = psnr(preview.bytes, exported.bytes);
    const onDelta = regionDiff(baseline.bytes, preview.bytes, preview.w, preview.h, 0.35, 0.15, 0.65, 0.85);
    const outsideDelta = regionDiff(baseline.bytes, preview.bytes, preview.w, preview.h, 0.02, 0.02, 0.25, 0.25);
    const parityOn = regionDiff(preview.bytes, exported.bytes, preview.w, preview.h, 0.35, 0.15, 0.65, 0.85);

    recordMetrics(testInfo, {
      brush_parity_psnr_db: Math.round(db * 100) / 100,
      brush_parity_region_delta: Math.round(parityOn * 100) / 100,
      brush_mask_effect_delta: Math.round(onDelta * 100) / 100,
      brush_mask_outside_delta: Math.round(outsideDelta * 100) / 100,
      brush_preview_settle_ms: settleMs,
    });
    expect(onDelta, 'the brushed mask had no visible effect').toBeGreaterThan(2);
    expect(outsideDelta, 'the brushed mask leaked outside the stroke').toBeLessThanOrEqual(1);
    expect(db, `brushed preview vs export PSNR ${db.toFixed(2)} dB at 128 px < 30 dB`).toBeGreaterThanOrEqual(30);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  const RAW = rawFixtures()[0];
  test('a brush stroke settles within 700 ms on a 24 MP ARW', async ({ page }, testInfo) => {
    test.skip(!RAW, 'no RAW fixture in dl-raw');
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [RAW]);
    await expect.poll(
      () => page.evaluate(() => ((window as any).__pes?.perf?.() || []).some((e: any) => e.name === 'proxy.raw_develop' || e.name === 'pool.develop')),
      { timeout: 180_000 },
    ).toBe(true);
    await expect(page.locator('.developing-badge')).toHaveCount(0, { timeout: 60_000 });
    await settle(page, 1000);

    await clickTab(page, 'Layers');
    await addAdjust(page).click();
    await setRange(page, /^Brightness/, 0.4);
    await settle(page, 800);

    await selectBrush(page);
    await setRange(page, /^Size:/, 0.15);
    await setRange(page, /^Softness:/, 0.2);
    await parkPointer(page);

    const blits0 = await markCount(page, 'hi.blit');
    const tUp = await stroke(page, [[0.45, 0.3], [0.55, 0.7]], 8);
    await expect.poll(() => markCount(page, 'hi.blit'), { timeout: 20_000, intervals: [20, 20, 50] })
      .toBeGreaterThan(blits0);
    const settleMs = Date.now() - tUp;

    recordMetrics(testInfo, { brush_raw_settle_ms: settleMs });
    expect(settleMs, `RAW brush preview settle ${settleMs} ms > 700 ms`).toBeLessThanOrEqual(700);
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
