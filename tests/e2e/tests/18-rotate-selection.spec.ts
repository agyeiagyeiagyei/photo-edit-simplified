import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import {
  openApp, trackErrors, recordMetrics, importFiles, canvas, canvasSize, clickTab, setRange, PNG_SMALL,
} from './helpers';

// Selections and adjustment masks sit on the photo's pixels, so turning or
// straightening the photo carries them along (owner report: a subject
// selection stayed put on screen when the photo was rotated).

type Frame = { w: number; h: number; bytes: Buffer };

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

/** The frame turned a quarter clockwise (or counter-clockwise). */
function turn(f: Frame, cw: boolean): Frame {
  const { w, h } = f;
  const out = Buffer.alloc(f.bytes.length);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const [nx, ny] = cw ? [h - 1 - y, x] : [y, w - 1 - x];
      f.bytes.copy(out, (ny * h + nx) * 4, (y * w + x) * 4, (y * w + x) * 4 + 4);
    }
  }
  return { w: h, h: w, bytes: out };
}

/** Pixels whose largest channel difference exceeds `thr`. */
function pixelsOff(a: Frame, b: Frame, thr = 24): number {
  expect([b.w, b.h], 'frame sizes differ').toEqual([a.w, a.h]);
  let n = 0;
  for (let i = 0; i < a.bytes.length; i += 4) {
    const d = Math.max(
      Math.abs(a.bytes[i] - b.bytes[i]),
      Math.abs(a.bytes[i + 1] - b.bytes[i + 1]),
      Math.abs(a.bytes[i + 2] - b.bytes[i + 2]),
    );
    if (d > thr) n++;
  }
  return n;
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
    return `${c.width}x${c.height}:${h}`;
  });

async function settle(page: Page, stableMs = 500) {
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

async function waitSize(page: Page, w: number, h: number) {
  await expect.poll(() => canvasSize(page).then((s) => ({ w: s.w, h: s.h })), { timeout: 30_000 })
    .toEqual({ w, h });
  await settle(page);
}

async function parkPointer(page: Page) {
  await page.mouse.move(2, 2);
  await page.waitForTimeout(80);
}

async function stroke(page: Page, pts: [number, number][]) {
  const b = await page.locator('.canvas-wrap').boundingBox();
  if (!b) throw new Error('no .canvas-wrap box');
  await page.mouse.move(b.x + b.width * pts[0][0], b.y + b.height * pts[0][1]);
  await page.mouse.down();
  for (const [x, y] of pts.slice(1)) {
    await page.mouse.move(b.x + b.width * x, b.y + b.height * y, { steps: 6 });
  }
  await page.mouse.up();
  await page.waitForTimeout(100);
}

const turnButton = (page: Page, label: '⟳ 90°' | '⟲ 90°') =>
  page.locator('button.btn', { hasText: label });

test.describe('selections turn with the photo', () => {
  test('a brushed selection and an adjustment mask follow a turn and a straighten', async ({ page }, testInfo) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSize(page, 800, 600);
    const base = await readCanvas(page);

    // An off-centre brushed selection: a turn moves it somewhere else.
    await clickTab(page, 'Select');
    await page.locator('.select-tab .chip', { hasText: /^Brush$/ }).click();
    await setRange(page, /^Size:/, 0.1);
    await setRange(page, /^Softness:/, 0);
    await parkPointer(page);
    await stroke(page, [[0.12, 0.2], [0.34, 0.24], [0.3, 0.4]]);
    // Off the brush, so its cursor ring (screen UI, not the photo) isn't drawn.
    await page.locator('.select-tab .chip', { hasText: /^Rect$/ }).click();
    await parkPointer(page);
    await settle(page);
    const tinted = await readCanvas(page);

    await clickTab(page, 'Rotate');
    await turnButton(page, '⟳ 90°').click();
    await clickTab(page, 'Select');
    await parkPointer(page);
    await waitSize(page, 600, 800);
    const tintedCw = await readCanvas(page);
    const selOff = pixelsOff(turn(tinted, true), tintedCw);

    // Fold the selection into an adjustment layer and brighten it.
    await clickTab(page, 'Layers');
    await page.locator('button.btn', { hasText: /＋ Adjust from sel\./ }).click();
    await setRange(page, /^Brightness/, 0.45);
    await clickTab(page, 'Rotate');
    await parkPointer(page);
    await settle(page, 800);
    const layered = await readCanvas(page);

    await turnButton(page, '⟲ 90°').click();
    await waitSize(page, 800, 600);
    const layeredCcw = await readCanvas(page);
    const layerOff = pixelsOff(turn(layered, false), layeredCcw);

    // A straighten and back returns the same frame: the masks are remapped
    // from where they were, not resampled once per step.
    await setRange(page, /^Straighten/, 6);
    await settle(page, 800);
    const straight = await canvasSize(page);
    await setRange(page, /^Straighten/, 3);
    await settle(page, 600);
    await setRange(page, /^Straighten/, 0);
    await waitSize(page, 800, 600);
    const roundTrip = await readCanvas(page);
    const tripOff = pixelsOff(layeredCcw, roundTrip, 2);

    // The export uses the turned masks too.
    await clickTab(page, 'Export');
    await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
    const [dl] = await Promise.all([
      page.waitForEvent('download', { timeout: 120_000 }),
      page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
    ]);
    const exported = await decodePng(page, fs.readFileSync((await dl.path())!));
    const exportOff = pixelsOff(roundTrip, exported, 8);

    recordMetrics(testInfo, {
      rotate_selection_px_off: selOff,
      rotate_layer_px_off: layerOff,
      straighten_round_trip_px_off: tripOff,
      rotate_export_px_off: exportOff,
      layer_brightened_px: pixelsOff(base, roundTrip, 30),
      straightened_w: straight.w,
      straightened_h: straight.h,
    });
    // The layer did brighten something, so the checks above compare real edits.
    const brightened = pixelsOff(base, roundTrip, 30);
    expect(brightened, 'the adjustment layer changed nothing').toBeGreaterThan(2000);
    expect(straight.w, 'the straighten did not crop the frame').toBeLessThan(800);
    expect(selOff, 'the selection did not turn with the photo').toBeLessThanOrEqual(200);
    expect(layerOff, 'the adjustment mask did not turn with the photo').toBeLessThanOrEqual(200);
    expect(tripOff, 'straightening and back changed the frame').toBeLessThanOrEqual(50);
    expect(exportOff, 'the export differs from the turned preview').toBeLessThanOrEqual(200);
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
