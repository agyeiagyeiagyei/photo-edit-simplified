import { test, expect, Page, BrowserContext } from '@playwright/test';
import * as fs from 'fs';
import { openApp, importFiles, thumbs, clickTab, setRange, trackErrors, recordMetrics, JPEG_A } from './helpers';

// Issue #47: Blur's radius and Grain's particle size were in output pixels, so
// a 6000 px export blurred far less, and showed far finer and weaker grain,
// than the fit preview the owner tuned them on (85% vs 36% of detail removed
// at Blur 60 on a 1024 px canvas; grain 7x stronger on screen). Both are now
// defined in image units, so the export downscaled to the preview's size has
// the same detail as the preview.

// A laptop-sized window at dpr 1 gives the smallest preview, the worst case.
test.use({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });

/** Mean |Laplacian| of luma after drawing `src` at `w` px wide: how much fine
 *  detail survives at that size. Runs in the page. */
const DETAIL_FN = `(src, W) => {
  const h = Math.round(W * src.height / src.width);
  const c = document.createElement('canvas'); c.width = W; c.height = h;
  const x = c.getContext('2d'); x.imageSmoothingEnabled = true; x.imageSmoothingQuality = 'high';
  x.drawImage(src, 0, 0, W, h);
  const d = x.getImageData(0, 0, W, h).data;
  const L = (i) => 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
  let s = 0, k = 0;
  for (let y = 1; y < h - 1; y++) for (let xx = 1; xx < W - 1; xx++) {
    const i = (y * W + xx) * 4;
    s += Math.abs(4 * L(i) - L(i - 4) - L(i + 4) - L(i - W * 4) - L(i + W * 4)); k++;
  }
  return s / k;
}`;

async function previewDetail(page: Page): Promise<{ detail: number; w: number }> {
  return page.evaluate((fn) => {
    const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement;
    const detail = (0, eval)(fn);
    return { detail: detail(c, c.width), w: c.width };
  }, DETAIL_FN);
}

/** Settle: the canvas stops changing for `ms`. */
async function settle(page: Page, ms = 1500, timeout = 90_000) {
  const hash = () =>
    page.evaluate(() => {
      const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement;
      const s = document.createElement('canvas'); s.width = 64; s.height = 40;
      s.getContext('2d')!.drawImage(c, 0, 0, 64, 40);
      const d = s.getContext('2d')!.getImageData(0, 0, 64, 40).data;
      let h = 2166136261 >>> 0;
      for (let i = 0; i < d.length; i += 4) { h ^= d[i]; h = Math.imul(h, 16777619) >>> 0; }
      return `${c.width}:${h}`;
    });
  let prev = '', since = Date.now();
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const h = await hash();
    if (h !== prev) { prev = h; since = Date.now(); } else if (Date.now() - since >= ms) return;
    await page.waitForTimeout(100);
  }
  throw new Error('preview never settled');
}

async function exportPng(page: Page): Promise<Buffer> {
  await clickTab(page, 'Export');
  await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
  const [dl] = await Promise.all([
    page.waitForEvent('download', { timeout: 240_000 }),
    page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
  ]);
  return fs.readFileSync((await dl.path())!);
}

/** Decode a full-size PNG in a fresh, empty page (a 24 MP noise PNG is too
 *  big to decode next to the app's own buffers) and measure it at `w` px. */
async function exportDetail(context: BrowserContext, png: Buffer, w: number) {
  const p = await context.newPage();
  await p.route('http://qa.test/**', (r) =>
    r.request().url().endsWith('.png')
      ? r.fulfill({ body: png, contentType: 'image/png' })
      : r.fulfill({ body: '<!doctype html><title>qa</title>', contentType: 'text/html' }),
  );
  await p.goto('http://qa.test/');
  const out = await p.evaluate(
    async ({ fn, w }) => {
      const bmp = await createImageBitmap(await (await fetch('/export.png')).blob());
      const detail = (0, eval)(fn);
      const r = { detail: detail(bmp, w), dims: [bmp.width, bmp.height] };
      bmp.close();
      return r;
    },
    { fn: DETAIL_FN, w },
  );
  await p.close();
  return out;
}

for (const fx of [
  { name: 'Blur', re: /^Blur/, value: 60, lo: 0.8, hi: 1.25 },
  { name: 'Grain', re: /^Grain/, value: 50, lo: 0.75, hi: 1.33 },
]) {
  test(`${fx.name} looks the same in the preview and the full-size export (#47)`, async ({ page, context }, testInfo) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [JPEG_A]);
    await expect(thumbs(page)).toHaveCount(1);
    await settle(page);
    const base = await previewDetail(page);
    await clickTab(page, 'Color');
    await setRange(page, fx.re, fx.value);
    await settle(page);
    const shown = await previewDetail(page);
    const png = await exportPng(page);
    const exported = await exportDetail(context, png, shown.w);
    expect(exported.dims).toEqual([6000, 4000]);
    const ratio = exported.detail / shown.detail;
    recordMetrics(testInfo, {
      [`${fx.name.toLowerCase()}_preview_px`]: shown.w,
      [`${fx.name.toLowerCase()}_detail_base`]: Math.round(base.detail * 1000) / 1000,
      [`${fx.name.toLowerCase()}_detail_preview`]: Math.round(shown.detail * 1000) / 1000,
      [`${fx.name.toLowerCase()}_detail_export`]: Math.round(exported.detail * 1000) / 1000,
      [`${fx.name.toLowerCase()}_export_over_preview`]: Math.round(ratio * 1000) / 1000,
    });
    expect(ratio, `export/preview detail ${ratio.toFixed(3)} at ${shown.w} px`).toBeGreaterThanOrEqual(fx.lo);
    expect(ratio, `export/preview detail ${ratio.toFixed(3)} at ${shown.w} px`).toBeLessThanOrEqual(fx.hi);
    expect(errors).toEqual([]);
  });
}
