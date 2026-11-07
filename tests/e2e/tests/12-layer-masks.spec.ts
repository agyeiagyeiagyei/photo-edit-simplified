import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import {
  openApp, trackErrors, recordMetrics, importFiles, canvas, canvasSize, clickTab, setRange,
  thumbs, PNG_SMALL, RING,
} from './helpers';

// Issue #36 Phase B: overlay layers (text, path, brush, image) composite
// through their mask on the same canvas path in the preview and the export.
// Acceptance: preview vs export >= 33 dB at 1024 px; the mask survives a
// reload; brushing one layer's mask leaves every other layer untouched.

type Frame = { w: number; h: number; bytes: Buffer };

const SMALL_W = 800;
const SMALL_H = 600;

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

/** Downscale the stage canvas or a decoded PNG export to a long edge. */
async function scaledFrame(page: Page, src: 'canvas' | Buffer, longEdge: number): Promise<Frame> {
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
  const deadline = Date.now() + 90_000;
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
    .toEqual({ w: SMALL_W, h: SMALL_H });
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

/** Drag a brush stroke. Do not park the pointer: callers read after parking. */
async function stroke(page: Page, pts: [number, number][], steps = 8) {
  const b = await wrapBox(page);
  await page.mouse.move(b.x + b.width * pts[0][0], b.y + b.height * pts[0][1]);
  await page.mouse.down();
  for (const [x, y] of pts.slice(1)) {
    await page.mouse.move(b.x + b.width * x, b.y + b.height * y, { steps });
  }
  await page.mouse.up();
  await page.waitForTimeout(120);
}

/** Park the pointer off-canvas so the brush ring is not drawn. */
async function parkPointer(page: Page) {
  await page.mouse.move(2, 2);
  await page.waitForTimeout(80);
}

const selectChip = (page: Page, name: string) =>
  page.locator('.select-tab .chip', { hasText: new RegExp(`^${name}$`) });

async function selectRect(page: Page) {
  await clickTab(page, 'Select');
  await selectChip(page, 'Rect').click();
}

async function selectBrush(page: Page) {
  await clickTab(page, 'Select');
  await selectChip(page, 'Brush').click();
}

async function addText(page: Page, text: string, color: string, size = 0.3) {
  await page.getByRole('button', { name: '＋ Text' }).click();
  await page.waitForTimeout(300);
  await page.locator('.text-editor input[type=text]').fill(text);
  await page.locator('.text-editor input[type=range]').nth(0).fill(String(size));
  await page.locator('.text-editor input[type=color]').nth(0).fill(color);
  await page.waitForTimeout(300);
}

const maskThumb = (page: Page, row = 0) =>
  page.locator('.layer-row').nth(row).locator('.mask-thumb');

async function exportPng(page: Page): Promise<Buffer> {
  await clickTab(page, 'Export');
  await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
  const [dl] = await Promise.all([
    page.waitForEvent('download', { timeout: 120_000 }),
    page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
  ]);
  return fs.readFileSync((await dl.path())!);
}

/** Red/blue pixel counts per half of the stage canvas. */
const halfColors = (page: Page) =>
  canvas(page).evaluate((c: HTMLCanvasElement) => {
    const d = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
    let redL = 0;
    let blueL = 0;
    let redR = 0;
    let blueR = 0;
    const mid = c.width / 2;
    for (let y = 0; y < c.height; y++) {
      for (let x = 0; x < c.width; x++) {
        const i = (y * c.width + x) * 4;
        const r = d[i];
        const g = d[i + 1];
        const b = d[i + 2];
        const left = x < mid;
        if (r > 150 && g < 100 && b < 100) {
          if (left) redL++;
          else redR++;
        }
        if (b > 150 && r < 100 && g < 100) {
          if (left) blueL++;
          else blueR++;
        }
      }
    }
    return { redL, blueL, redR, blueR };
  });

test.describe('overlay layer masks (issue #36 Phase B)', () => {
  test('a masked text layer matches the PNG export at 1024 px and stays inside its mask', async ({ page }, testInfo) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [RING]);
    await expect.poll(() => canvasSize(page).then((s) => s.w), { timeout: 60_000 }).toBeGreaterThan(0);
    await settle(page, 800);
    // The stage backs either the 1024 px working preview or the sharp frame;
    // compare both sides at a 1024 px long edge (issue #36 Phase B bar).
    const baseline = await scaledFrame(page, 'canvas', 1024);

    await clickTab(page, 'Layers');
    await addText(page, 'MMMMMMMM', '#ff0000');
    await settle(page, 600);
    const withText = await scaledFrame(page, 'canvas', 1024);

    // A rect over the left half becomes the layer's mask through the row
    // thumbnail, exactly like the shared Mask section's Add mask button.
    await selectRect(page);
    await rectDrag(page, 0.06, 0.15, 0.48, 0.85);
    await clickTab(page, 'Layers');
    await maskThumb(page).click();
    await expect(page.locator('.layer-row .mask-thumb.has-mask')).toHaveCount(1);
    await settle(page, 800);
    const masked = await scaledFrame(page, 'canvas', 1024);

    const inside = regionDiff(baseline.bytes, masked.bytes, masked.w, masked.h, 0.10, 0.25, 0.44, 0.75);
    const outside = regionDiff(baseline.bytes, masked.bytes, masked.w, masked.h, 0.54, 0.25, 0.94, 0.75);
    const cut = regionDiff(withText.bytes, masked.bytes, masked.w, masked.h, 0.54, 0.25, 0.94, 0.75);
    expect(inside, 'the masked text did not show inside its mask').toBeGreaterThan(3);
    expect(outside, 'the masked text leaked outside its mask').toBeLessThanOrEqual(1);
    expect(cut, 'the mask did not cut the text out of the right half').toBeGreaterThan(3);

    const png = await exportPng(page);
    const exported = await scaledFrame(page, png, 1024);
    expect({ w: masked.w, h: masked.h }).toEqual({ w: exported.w, h: exported.h });
    const db = psnr(masked.bytes, exported.bytes);
    const parityIn = regionDiff(masked.bytes, exported.bytes, masked.w, masked.h, 0.10, 0.25, 0.44, 0.75);
    recordMetrics(testInfo, {
      mask_text_inside_delta: Math.round(inside * 100) / 100,
      mask_text_outside_delta: Math.round(outside * 100) / 100,
      mask_text_cut_delta: Math.round(cut * 100) / 100,
      mask_text_parity_psnr_db: Math.round(db * 100) / 100,
      mask_text_parity_inside_delta: Math.round(parityIn * 100) / 100,
      mask_text_parity_px: masked.w,
    });
    expect(db, `masked text preview vs export PSNR ${db.toFixed(2)} dB at ${masked.w} px < 33 dB`).toBeGreaterThanOrEqual(33);
    expect(parityIn, 'preview vs export differ inside the mask').toBeLessThanOrEqual(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('an overlay mask survives a session reload', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);
    const baseline = await readCanvas(page);

    await clickTab(page, 'Layers');
    await addText(page, 'MMMMMMMM', '#ff0000');
    await selectRect(page);
    await rectDrag(page, 0.1, 0.2, 0.45, 0.8);
    await clickTab(page, 'Layers');
    await maskThumb(page).click();
    await expect(page.locator('.layer-row .mask-thumb.has-mask')).toHaveCount(1);
    await settle(page, 600);
    const masked = await readCanvas(page);
    const onDelta = regionDiff(baseline.bytes, masked.bytes, SMALL_W, SMALL_H, 0.14, 0.25, 0.41, 0.75);
    const offDelta = regionDiff(baseline.bytes, masked.bytes, SMALL_W, SMALL_H, 0.55, 0.25, 0.9, 0.75);
    expect(onDelta, 'the mask had no visible effect before reload').toBeGreaterThan(3);
    expect(offDelta, 'the mask leaked before reload').toBeLessThanOrEqual(1);

    await page.waitForTimeout(2200);
    await page.reload();
    await expect(thumbs(page)).toHaveCount(1, { timeout: 60_000 });
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await waitSmall(page);
    await settle(page, 800);

    await clickTab(page, 'Layers');
    await expect(page.locator('.layer-row')).toHaveCount(1);
    await expect(page.locator('.layer-row .mask-thumb.has-mask'), 'the layer mask was not parsed on reload')
      .toHaveCount(1);
    // The shared mask section reads the restored mask, not just its class.
    await page.locator('.layer-row').first().locator('.layer-label').click();
    await expect(page.locator('.mask-section button.btn', { hasText: 'Clear mask' })).toBeEnabled();

    const restored = await readCanvas(page);
    const reloadDiff = meanDiff(masked.bytes, restored.bytes);
    const restoredOn = regionDiff(baseline.bytes, restored.bytes, SMALL_W, SMALL_H, 0.14, 0.25, 0.41, 0.75);
    const restoredOff = regionDiff(baseline.bytes, restored.bytes, SMALL_W, SMALL_H, 0.55, 0.25, 0.9, 0.75);
    recordMetrics(testInfo, {
      mask_reload_mean_delta: Math.round(reloadDiff * 100) / 100,
      mask_reload_inside_delta: Math.round(restoredOn * 100) / 100,
      mask_reload_outside_delta: Math.round(restoredOff * 100) / 100,
    });
    expect(reloadDiff, 'the masked frame changed across the reload').toBeLessThanOrEqual(1);
    expect(restoredOn, 'the restored mask no longer shows the layer').toBeGreaterThan(3);
    expect(restoredOff, 'the restored mask leaked outside its region').toBeLessThanOrEqual(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('brushing one layer mask changes only that layer', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);

    // Layer A: red text masked to the left half.
    await clickTab(page, 'Layers');
    await selectRect(page);
    await rectDrag(page, 0.05, 0.2, 0.48, 0.8);
    await clickTab(page, 'Layers');
    await addText(page, 'MMMMMMMM', '#ff0000');
    await maskThumb(page).click();
    await settle(page, 600);
    const leftOnly = await readCanvas(page);

    // Layer B: blue text masked to the right half.
    await selectRect(page);
    await rectDrag(page, 0.52, 0.2, 0.95, 0.8);
    await clickTab(page, 'Layers');
    await addText(page, 'MMMMMMMM', '#0000ff');
    await maskThumb(page).click();
    await settle(page, 600);
    const both = await readCanvas(page);
    const before = await halfColors(page);
    expect(before.redL, 'the left layer is missing').toBeGreaterThan(500);
    expect(before.blueR, 'the right layer is missing').toBeGreaterThan(500);
    expect(before.redR, 'the left layer leaked into the right half').toBe(0);
    expect(before.blueL, 'the right layer leaked into the left half').toBe(0);

    // B is still the brush target from its mask-thumb click. The hint must
    // name this layer, not the global selection nor an adjustment layer.
    await selectBrush(page);
    await expect(page.locator('.select-tab p.dim', { hasText: /^Brush edits the selected layer's mask/ }))
      .toBeVisible();
    await setRange(page, /^Size:/, 0.2);
    await setRange(page, /^Softness:/, 0);
    await selectChip(page, 'Remove').click();
    await parkPointer(page);
    await settle(page, 400);
    await stroke(page, [[0.6, 0.5], [0.85, 0.5]]);
    await parkPointer(page);

    // Read outside the Select tab so its live red mask tint is not sampled.
    await clickTab(page, 'Layers');
    await settle(page, 600);
    const brushed = await readCanvas(page);
    const after = await halfColors(page);

    const otherLayer = regionDiff(both.bytes, brushed.bytes, SMALL_W, SMALL_H, 0.04, 0.2, 0.44, 0.8);
    const erased = regionDiff(both.bytes, brushed.bytes, SMALL_W, SMALL_H, 0.62, 0.44, 0.82, 0.56);
    const untouchedRight = regionDiff(both.bytes, brushed.bytes, SMALL_W, SMALL_H, 0.6, 0.2, 0.9, 0.3);
    // Erasing B's mask must reveal the frame as it was before B existed.
    const restored = regionDiff(leftOnly.bytes, brushed.bytes, SMALL_W, SMALL_H, 0.62, 0.44, 0.82, 0.56);
    recordMetrics(testInfo, {
      mask_brush_other_layer_delta: Math.round(otherLayer * 100) / 100,
      mask_brush_erased_delta: Math.round(erased * 100) / 100,
      mask_brush_untouched_region_delta: Math.round(untouchedRight * 100) / 100,
      mask_brush_restored_delta: Math.round(restored * 100) / 100,
      mask_brush_red_left_before: before.redL,
      mask_brush_red_left_after: after.redL,
      mask_brush_blue_right_before: before.blueR,
      mask_brush_blue_right_after: after.blueR,
    });
    expect(otherLayer, 'brushing B also changed the left layer').toBeLessThanOrEqual(1);
    expect(after.redL, 'brushing B changed the left layer pixels').toBe(before.redL);
    expect(after.blueR, 'the Remove stroke did not erase B').toBeLessThan(before.blueR - 500);
    expect(erased, 'the Remove stroke had no visible effect on B').toBeGreaterThan(3);
    expect(untouchedRight, 'the stroke changed B outside its path').toBeLessThanOrEqual(1);
    expect(restored, "erasing B's mask did not reveal the underlying frame").toBeLessThanOrEqual(2);
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
