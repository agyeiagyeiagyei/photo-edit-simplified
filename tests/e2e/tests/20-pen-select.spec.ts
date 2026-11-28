import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import { openApp, trackErrors, importFiles, canvas, canvasSize, clickTab, setRange, PNG_SMALL } from './helpers';

// Owner request (28 Nov): a Pen selection tool to add to and remove from
// masks. Clicks place anchors, a drag pulls out Bézier handles, and clicking
// the first anchor (or Enter) closes the path into the photo's selection,
// which the mask panel folds into the adjustment's mask. Checked on exports,
// which carry no editor tint, so they show where the adjustment applies.

type Frame = { w: number; h: number; bytes: Buffer };

async function decodePng(page: Page, png: Buffer): Promise<Frame> {
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
  }, png.toString('base64'));
  return { w: r.w, h: r.h, bytes: Buffer.from(r.b64, 'base64') };
}

async function exportPng(page: Page): Promise<Frame> {
  await clickTab(page, 'Export');
  await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
  const [dl] = await Promise.all([
    page.waitForEvent('download', { timeout: 120_000 }),
    page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
  ]);
  return decodePng(page, fs.readFileSync((await dl.path())!));
}

/** Mean |ΔRGB| (0..255) inside a normalized rect. */
function lift(base: Frame, f: Frame, x0: number, y0: number, x1: number, y1: number): number {
  const { w, h } = f;
  let sum = 0;
  let n = 0;
  for (let y = Math.floor(y0 * h); y < Math.ceil(y1 * h); y++) {
    for (let x = Math.floor(x0 * w); x < Math.ceil(x1 * w); x++) {
      const i = (y * w + x) * 4;
      for (let c = 0; c < 3; c++) {
        sum += Math.abs(base.bytes[i + c] - f.bytes[i + c]);
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

async function settle(page: Page, stableMs = 500) {
  let prev = '';
  let since = Date.now();
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const cur = await canvasHash(page);
    if (cur !== prev) {
      prev = cur;
      since = Date.now();
    } else if (Date.now() - since >= stableMs) {
      return;
    }
    await page.waitForTimeout(120);
  }
  throw new Error('canvas never settled');
}

async function openSmall(page: Page) {
  await openApp(page);
  await importFiles(page, [PNG_SMALL]);
  await expect.poll(() => canvasSize(page).then((s) => ({ w: s.w, h: s.h })), { timeout: 30_000 })
    .toEqual({ w: 800, h: 600 });
  await settle(page);
}

async function at(page: Page, x: number, y: number): Promise<[number, number]> {
  const b = await page.locator('.canvas-wrap').boundingBox();
  if (!b) throw new Error('no .canvas-wrap box');
  return [b.x + b.width * x, b.y + b.height * y];
}

async function tap(page: Page, x: number, y: number) {
  await page.mouse.click(...(await at(page, x, y)));
  await page.waitForTimeout(60);
}

async function drag(page: Page, from: [number, number], to: [number, number], opts: { alt?: boolean } = {}) {
  await page.mouse.move(...(await at(page, ...from)));
  if (opts.alt) await page.keyboard.down('Alt');
  await page.mouse.down();
  await page.mouse.move(...(await at(page, ...to)), { steps: 10 });
  await page.mouse.up();
  if (opts.alt) await page.keyboard.up('Alt');
  await page.waitForTimeout(120);
}

async function pickPen(page: Page) {
  await clickTab(page, 'Select');
  await page.locator('.select-tab .chip', { hasText: /^Pen$/ }).click();
}

/** Pixels of the blue anchor dot (#0a84ff) within 1% of a normalized point on the canvas. */
const anchorPixels = (page: Page, x: number, y: number) =>
  canvas(page).evaluate((c: HTMLCanvasElement, [x, y]) => {
    const r = Math.max(4, Math.round(c.width * 0.01));
    const [cx, cy] = [Math.round(x * c.width), Math.round(y * c.height)];
    const d = c.getContext('2d')!.getImageData(cx - r, cy - r, 2 * r + 1, 2 * r + 1).data;
    let n = 0;
    for (let i = 0; i < d.length; i += 4) {
      if (Math.abs(d[i] - 10) < 25 && Math.abs(d[i + 1] - 132) < 30 && d[i + 2] > 230) n++;
    }
    return n;
  }, [x, y]);

const chip = (page: Page, name: string) => page.locator('.select-tab .chip', { hasText: new RegExp(`^${name}$`) });
const copyToLayer = (page: Page) => page.locator('.select-row button', { hasText: /^Copy to layer$/ });

test.describe('Pen selection (owner request 28 Nov)', () => {
  test('a curved pen path is added to an adjustment mask, and a straight one removed', async ({ page }) => {
    test.setTimeout(240_000);
    const errors = trackErrors(page);
    await openSmall(page);
    const base = await exportPng(page);

    // An adjustment confined to a small rect at the top left.
    await clickTab(page, 'Select');
    await chip(page, 'Rect').click();
    await drag(page, [0.05, 0.05], [0.25, 0.25]);
    await clickTab(page, 'Layers');
    await page.locator('button.btn', { hasText: /＋ Adjust from sel\./ }).click();
    await setRange(page, /^Brightness/, 0.5);
    await settle(page, 600);

    await pickPen(page);
    const panel = page.locator('.select-tab .mask-panel');
    await expect(panel.locator('.editing-line')).toContainText('Editing: Adjust mask');
    await expect(chip(page, 'Close path')).toBeDisabled();
    await expect(chip(page, 'Undo point')).toBeDisabled();

    // Two anchors, the second dragged straight down: its handles point up
    // (in) and down (out), so the path is a lens bulging above and below the
    // line between them. Straight edges would enclose nothing at all.
    await tap(page, 0.35, 0.5);
    await expect(chip(page, 'Undo point')).toBeEnabled();
    await expect(chip(page, 'Close path'), 'two straight anchors cannot close').toBeDisabled();
    await drag(page, [0.75, 0.5], [0.75, 0.8]);
    await expect(chip(page, 'Close path'), 'a curve between two anchors can close').toBeEnabled();
    await expect(copyToLayer(page), 'an open path is not a selection yet').toBeDisabled();
    await tap(page, 0.35, 0.5);
    await expect(copyToLayer(page), 'clicking the first anchor should make the selection').toBeEnabled();
    await expect(chip(page, 'Close path')).toBeDisabled();

    await panel.getByRole('button', { name: 'Add selection to mask' }).click();
    await settle(page, 600);
    await expect(copyToLayer(page), 'adding to the mask should use up the selection').toBeDisabled();
    let e = await exportPng(page);
    expect(lift(base, e, 0.08, 0.08, 0.22, 0.22), 'the rect lost the effect').toBeGreaterThan(3);
    expect(lift(base, e, 0.58, 0.42, 0.7, 0.58), 'inside the lens is not in the mask').toBeGreaterThan(3);
    expect(lift(base, e, 0.42, 0.47, 0.5, 0.53), 'the lens near its first anchor is not in the mask').toBeGreaterThan(3);
    expect(lift(base, e, 0.58, 0.22, 0.7, 0.32), 'above the lens leaked into the mask').toBeLessThanOrEqual(1);

    // A straight four-point path over the right of the lens, closed with
    // Enter after Backspace took back a stray point, then removed.
    await pickPen(page);
    await tap(page, 0.55, 0.4);
    await tap(page, 0.75, 0.4);
    await tap(page, 0.9, 0.9);
    await expect(chip(page, 'Close path')).toBeEnabled();
    await page.keyboard.press('Backspace');
    await expect(chip(page, 'Close path'), 'Backspace should take back the third point').toBeDisabled();
    await tap(page, 0.75, 0.6);
    await tap(page, 0.55, 0.6);
    await page.keyboard.press('Enter');
    await expect(copyToLayer(page), 'Enter should close the path').toBeEnabled();
    await panel.getByRole('button', { name: 'Remove selection from mask' }).click();
    await settle(page, 600);
    e = await exportPng(page);
    expect(lift(base, e, 0.58, 0.42, 0.7, 0.58), 'Remove selection from mask left the effect').toBeLessThanOrEqual(1);
    expect(lift(base, e, 0.42, 0.47, 0.5, 0.53), 'the rest of the lens lost the effect').toBeGreaterThan(3);

    // Escape drops an open path; nothing is selected.
    await pickPen(page);
    await tap(page, 0.3, 0.7);
    await tap(page, 0.4, 0.7);
    await page.keyboard.press('Escape');
    await expect(chip(page, 'Undo point')).toBeDisabled();
    await expect(copyToLayer(page)).toBeDisabled();
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('dragging an anchor of a closed path reshapes the selection', async ({ page }) => {
    test.setTimeout(180_000);
    const errors = trackErrors(page);
    await openSmall(page);
    const base = await exportPng(page);

    await pickPen(page);
    await tap(page, 0.2, 0.2);
    await tap(page, 0.5, 0.2);
    await tap(page, 0.2, 0.5);
    // Each anchor shows as it is placed: a pass coalesced into the next frame
    // must still repaint for the path.
    await expect.poll(() => anchorPixels(page, 0.2, 0.5), { timeout: 5_000 }).toBeGreaterThan(20);
    await chip(page, 'Close path').click();
    await expect(copyToLayer(page)).toBeEnabled();
    // Move the (0.5, 0.2) corner to (0.7, 0.6): the triangle now reaches
    // (0.45, 0.45) and no longer covers just under its old top edge.
    await drag(page, [0.5, 0.2], [0.7, 0.6]);
    await expect(copyToLayer(page), 'the reshaped path should still be the selection').toBeEnabled();

    await clickTab(page, 'Layers');
    await page.locator('button.btn', { hasText: /＋ Adjust from sel\./ }).click();
    await setRange(page, /^Brightness/, 0.5);
    await settle(page, 600);
    const e = await exportPng(page);
    expect(lift(base, e, 0.42, 0.43, 0.48, 0.47), 'the moved corner did not take the selection along').toBeGreaterThan(3);
    expect(lift(base, e, 0.27, 0.22, 0.33, 0.25), 'the old outline is still selected').toBeLessThanOrEqual(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
