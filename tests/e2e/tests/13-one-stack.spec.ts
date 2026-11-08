import { test, expect, Page, Locator } from '@playwright/test';
import * as fs from 'fs';
import {
  openApp, trackErrors, recordMetrics, importFiles, canvas, canvasSize, clickTab, setRange,
  thumbs, PNG_SMALL,
} from './helpers';

// Issue #36 Phase C: one stack. An adjustment applies to whatever sits below
// it, overlays included, in the preview and the export. A stack saved before
// this change renders the same after restore. QA Bug 1: two masked overlays
// under a dragged adjustment must not rebuild their masks every frame.

type Frame = { w: number; h: number; bytes: Buffer };
type Mark = { name: string; dur_ms: number; detail?: string };

const W = 800;
const H = 600;

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

/** Milliseconds from now until the canvas stops changing for `stableMs`. */
async function settleTimed(page: Page, stableMs = 300, timeout = 8000): Promise<number> {
  const t0 = Date.now();
  let prev = '';
  let since = t0;
  while (Date.now() - t0 < timeout) {
    const cur = await canvasHash(page);
    const now = Date.now();
    if (cur !== prev) {
      prev = cur;
      since = now;
    } else if (now - since >= stableMs) {
      return now - t0;
    }
    await page.waitForTimeout(50);
  }
  return Date.now() - t0;
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

async function selectRect(page: Page) {
  await clickTab(page, 'Select');
  await page.locator('.select-tab .chip', { hasText: /^Rect$/ }).click();
}

async function addText(page: Page, text: string, color: string, size: number) {
  await page.getByRole('button', { name: '＋ Text' }).click();
  await page.waitForTimeout(300);
  await page.locator('.text-editor input[type=text]').fill(text);
  await page.locator('.text-editor input[type=range]').nth(0).fill(String(size));
  await page.locator('.text-editor input[type=color]').nth(0).fill(color);
  await page.waitForTimeout(300);
}

const addAdjust = (page: Page) => page.locator('button.btn', { hasText: /＋ Adjust\s*$/ });
const maskThumb = (page: Page, row = 0) => page.locator('.layer-row').nth(row).locator('.mask-thumb');
const brightnessSlider = (page: Page) =>
  page.locator('.adjust-editor label.slider', { hasText: /^Brightness/ }).locator('input[type=range]');

/** Drag a range input with the real mouse, from one track fraction to another. */
async function dragRange(page: Page, input: Locator, fromFrac: number, toFrac: number, steps = 28) {
  const b = await input.boundingBox();
  if (!b) throw new Error('no range box');
  const y = b.y + b.height / 2;
  const x0 = b.x + b.width * fromFrac;
  const x1 = b.x + b.width * toFrac;
  await page.mouse.move(x0, y);
  await page.mouse.down();
  for (let i = 1; i <= steps; i++) {
    await page.mouse.move(x0 + ((x1 - x0) * i) / steps, y);
  }
  await page.mouse.up();
}

async function exportPng(page: Page): Promise<Buffer> {
  await clickTab(page, 'Export');
  await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
  const [dl] = await Promise.all([
    page.waitForEvent('download', { timeout: 120_000 }),
    page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
  ]);
  return fs.readFileSync((await dl.path())!);
}

const marks = (page: Page): Promise<Mark[]> =>
  page.evaluate(() => (((window as any).__pes?.perf?.() ?? []) as Mark[]));

/** Strip the stack-semantics version from every stored record; returns the
 * saved stack shape so the test can assert it is a pre-Phase-C one. */
async function stripStackVersion(page: Page): Promise<{ layers: number; last: string; hadLv: boolean }> {
  return page.evaluate(() => new Promise((resolve, reject) => {
    const req = indexedDB.open('pes-session');
    req.onerror = () => reject(req.error);
    req.onsuccess = () => {
      const db = req.result;
      const tx = db.transaction('items', 'readwrite');
      const store = tx.objectStore('items');
      const out = { layers: 0, last: '', hadLv: false };
      const cur = store.openCursor();
      cur.onerror = () => reject(cur.error);
      cur.onsuccess = () => {
        const c = cur.result;
        if (!c) return;
        const v = c.value as any;
        if (typeof v.layers === 'string') {
          const ls = JSON.parse(v.layers) as { t: string }[];
          out.layers = ls.length;
          out.last = ls.length ? ls[ls.length - 1].t : '';
        }
        out.hadLv = 'lv' in v;
        delete v.lv;
        c.update(v);
        c.continue();
      };
      tx.oncomplete = () => { db.close(); resolve(out); };
      tx.onerror = () => reject(tx.error);
    };
  }));
}

test.describe('one stack, adjustments over everything below (issue #36 Phase C)', () => {
  test('moving an adjustment above a text layer changes the text in preview and export', async ({ page }, testInfo) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);

    // Adjustment first, then text: the text lands above it, so the brightness
    // must reach the photo but not the glyphs.
    await clickTab(page, 'Layers');
    await addAdjust(page).click();
    await setRange(page, /^Brightness/, 0.8);
    await settle(page, 800);
    await addText(page, 'PHASE', '#0000ff', 0.12);
    await settle(page, 800);
    const below = await readCanvas(page);

    await clickTab(page, 'Export');
    await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
    const [dlBelow] = await Promise.all([
      page.waitForEvent('download', { timeout: 120_000 }),
      page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
    ]);
    const pngBelow = fs.readFileSync((await dlBelow.path())!);

    // ↑ on the adjustment row (the panel lists topmost first, so it sits at
    // the bottom) puts it above the text in the stack.
    await clickTab(page, 'Layers');
    await expect(page.locator('.layer-row')).toHaveCount(2);
    await page.locator('.layer-row').nth(1).locator('button.icon-btn', { hasText: '↑' }).click();
    await settle(page, 800);
    const above = await readCanvas(page);
    const pngAbove = await exportPng(page);

    const exportedBelow = await decodePng(page, pngBelow);
    const exportedAbove = await decodePng(page, pngAbove);
    expect({ w: exportedBelow.w, h: exportedBelow.h }).toEqual({ w: W, h: H });
    expect({ w: exportedAbove.w, h: exportedAbove.h }).toEqual({ w: W, h: H });

    // 'PHASE' at size 0.12 is a centred band; the left edge never holds a glyph.
    const TEXT = [0.3, 0.42, 0.7, 0.58] as const;
    const EDGE = [0.02, 0.42, 0.2, 0.58] as const;
    const textPreview = regionDiff(below.bytes, above.bytes, W, H, ...TEXT);
    const edgePreview = regionDiff(below.bytes, above.bytes, W, H, ...EDGE);
    const textExport = regionDiff(exportedBelow.bytes, exportedAbove.bytes, W, H, ...TEXT);
    const edgeExport = regionDiff(exportedBelow.bytes, exportedAbove.bytes, W, H, ...EDGE);
    const dbBelow = psnr(below.bytes, exportedBelow.bytes);
    const dbAbove = psnr(above.bytes, exportedAbove.bytes);

    recordMetrics(testInfo, {
      one_stack_text_preview_delta: Math.round(textPreview * 100) / 100,
      one_stack_photo_preview_delta: Math.round(edgePreview * 100) / 100,
      one_stack_text_export_delta: Math.round(textExport * 100) / 100,
      one_stack_photo_export_delta: Math.round(edgeExport * 100) / 100,
      one_stack_psnr_below_db: Math.round(dbBelow * 100) / 100,
      one_stack_psnr_above_db: Math.round(dbAbove * 100) / 100,
    });

    expect(textPreview, 'the adjustment above the text did not change the text in the preview').toBeGreaterThan(3);
    expect(edgePreview, 'the adjustment changed photo-only pixels between the two orders').toBeLessThanOrEqual(1);
    expect(textExport, 'the adjustment above the text did not change the text in the export').toBeGreaterThan(3);
    expect(edgeExport, 'the export differs on photo-only pixels between the two orders').toBeLessThanOrEqual(1);
    expect(dbBelow, `adjustment-below preview vs export PSNR ${dbBelow.toFixed(2)} dB < 33 dB`).toBeGreaterThanOrEqual(33);
    expect(dbAbove, `adjustment-above preview vs export PSNR ${dbAbove.toFixed(2)} dB < 33 dB`).toBeGreaterThanOrEqual(33);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('a stack saved before one-stack semantics renders the same after restore', async ({ page }, testInfo) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);

    await clickTab(page, 'Layers');
    await addAdjust(page).click();
    await setRange(page, /^Brightness/, 0.8);
    await settle(page, 800);
    await addText(page, 'LEGACY', '#0000ff', 0.12);
    await settle(page, 800);
    // Phase B rendering of a [text, adjust] stack: photo brightened, text not.
    const legacy = await readCanvas(page);

    // New semantics: the same stored order now brightens the text too.
    await clickTab(page, 'Layers');
    await page.locator('.layer-row').nth(1).locator('button.icon-btn', { hasText: '↑' }).click();
    await settle(page, 800);
    const reordered = await readCanvas(page);
    const TEXT = [0.28, 0.42, 0.72, 0.58] as const;
    expect(regionDiff(legacy.bytes, reordered.bytes, W, H, ...TEXT),
      'moving the adjustment above the text did not change the text').toBeGreaterThan(3);

    // Debounced persist (800 ms), then make the record look pre-Phase-C: the
    // stack stays in its [text, adjust] order and the version marker is gone.
    await page.waitForTimeout(2200);
    const stored = await stripStackVersion(page);
    expect(stored, 'the session record did not carry the saved layer stack').toMatchObject({
      layers: 2,
      last: 'adjust',
      hadLv: true,
    });

    await page.reload();
    await expect(thumbs(page)).toHaveCount(1, { timeout: 60_000 });
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await expect.poll(() => canvasSize(page).then((s) => ({ w: s.w, h: s.h })), { timeout: 30_000 })
      .toEqual({ w: W, h: H });
    await settle(page, 800);

    await clickTab(page, 'Layers');
    await expect(page.locator('.layer-row')).toHaveCount(2);
    // Normalised: the adjustment was moved to the bottom of the stack, so the
    // text is listed first again.
    await expect(page.locator('.layer-row').nth(0).locator('.layer-label')).toContainText('LEGACY');
    await expect(page.locator('.layer-row').nth(1).locator('.layer-label')).toHaveText('Adjust');

    const restored = await readCanvas(page);
    const restoreDelta = meanDiff(legacy.bytes, restored.bytes);
    const restoreBand = regionDiff(legacy.bytes, restored.bytes, W, H, ...TEXT);
    const newSemanticsDelta = regionDiff(restored.bytes, reordered.bytes, W, H, ...TEXT);
    recordMetrics(testInfo, {
      one_stack_legacy_restore_delta: Math.round(restoreDelta * 100) / 100,
      one_stack_legacy_restore_band_delta: Math.round(restoreBand * 100) / 100,
      one_stack_legacy_new_semantics_delta: Math.round(newSemanticsDelta * 100) / 100,
    });
    expect(restoreDelta, 'the restored legacy stack did not render its old pixels').toBeLessThanOrEqual(1);
    expect(restoreBand, 'the text region changed across the legacy restore').toBeLessThanOrEqual(1);
    expect(newSemanticsDelta, 'the restored stack adopted one-stack semantics instead of legacy pixels').toBeGreaterThan(3);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('dragging an adjustment over two masked overlays rebuilds no mask per frame', async ({ page }, testInfo) => {
    test.setTimeout(240_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await waitSmall(page);

    // Layer A: red text masked to the left half.
    await clickTab(page, 'Layers');
    await selectRect(page);
    await rectDrag(page, 0.05, 0.2, 0.48, 0.8);
    await clickTab(page, 'Layers');
    await addText(page, 'LEFT', '#ff0000', 0.2);
    await maskThumb(page).click();
    await expect(page.locator('.layer-row .mask-thumb.has-mask')).toHaveCount(1);
    await settle(page, 600);

    // Layer B: blue text masked to the right half.
    await selectRect(page);
    await rectDrag(page, 0.52, 0.2, 0.95, 0.8);
    await clickTab(page, 'Layers');
    await addText(page, 'RIGHT', '#0000ff', 0.2);
    await maskThumb(page).click();
    await expect(page.locator('.layer-row .mask-thumb.has-mask')).toHaveCount(2);
    await settle(page, 800);

    // Adjustment above both masked overlays: the preview and every worker
    // reply composite the overlay run, then apply the adjustment.
    await addAdjust(page).click();
    await setRange(page, /^Brightness/, 0.0);
    await settle(page, 800);
    await page.waitForTimeout(600);
    await expect(page.locator('.layer-row')).toHaveCount(3);

    await page.evaluate(() => (window as any).__pes.reset());
    await dragRange(page, brightnessSlider(page), 0.5, 0.82);
    const releaseAt = Date.now();
    const settleMs = await settleTimed(page);
    const spend = await marks(page);
    const count = (n: string) => spend.filter((m) => m.name === n).length;
    const maskBuilds = count('overlay.mask.build');
    const segmentBuilds = count('overlay.segment.build');
    const adjustApplies = count('stack.adjust.apply');
    const previews = count('preview.render');
    const value = Number(await brightnessSlider(page).inputValue());
    void releaseAt;

    recordMetrics(testInfo, {
      one_stack_drag_previews: previews,
      one_stack_drag_adjust_applies: adjustApplies,
      one_stack_drag_mask_builds: maskBuilds,
      one_stack_drag_segment_builds: segmentBuilds,
      one_stack_drag_sharp_settle_ms: Math.round(settleMs),
      one_stack_drag_brightness: Math.round(value * 100) / 100,
    });

    expect(value, 'the real-mouse drag did not move the brightness slider').toBeGreaterThan(0.05);
    expect(previews, 'the drag did not render multiple preview frames').toBeGreaterThanOrEqual(5);
    expect(adjustApplies, 'the post-stack adjustment was never applied').toBeGreaterThanOrEqual(5);
    expect(maskBuilds, `mask rebuilt ${maskBuilds} times during the drag`).toBeLessThanOrEqual(2);
    expect(segmentBuilds, `overlay run rebuilt ${segmentBuilds} times during the drag`).toBeLessThanOrEqual(1);
    expect(settleMs, `final frame settled ${settleMs} ms after release (> 700 ms)`).toBeLessThanOrEqual(700);
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
