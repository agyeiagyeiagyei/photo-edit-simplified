import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import * as zlib from 'zlib';
import { openApp, importFiles, thumbs, clickTab, setRange, trackErrors, recordMetrics, JPEG_A } from './helpers';

// Issue #47: Blur's radius and Grain's particle size were in output pixels, so
// a 6000 px export blurred far less, and showed far finer and weaker grain,
// than the fit preview the owner tuned them on (85% vs 36% of detail removed
// at Blur 60 on a 1024 px canvas; grain 7x stronger on screen). Both are now
// defined in image units, so the export downscaled to the preview's size has
// the same detail as the preview.
//
// Issue #51: grain then still point-sampled its lattice, so a large hi-dpi
// preview (cells wider than its pixels) showed crisper, stronger grain than
// the export box-averaged to it: 0.67 by this measure on the owner's ARW. Each
// pixel now takes the area sample of the cells under it.

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

/** Decode an 8-bit, non-interlaced RGB/RGBA PNG (what Export writes) in Node:
 *  a 24 MP noise PNG is too big to decode in a page next to the app. */
function decodePng(buf: Buffer): { w: number; h: number; ch: number; data: Buffer } {
  let p = 8, w = 0, h = 0, depth = 0, type = 0, interlace = 0;
  const idat: Buffer[] = [];
  while (p < buf.length) {
    const len = buf.readUInt32BE(p);
    const kind = buf.toString('ascii', p + 4, p + 8);
    const body = buf.subarray(p + 8, p + 8 + len);
    if (kind === 'IHDR') {
      w = body.readUInt32BE(0); h = body.readUInt32BE(4);
      depth = body[8]; type = body[9]; interlace = body[12];
    } else if (kind === 'IDAT') idat.push(body);
    else if (kind === 'IEND') break;
    p += 12 + len;
  }
  if (depth !== 8 || interlace !== 0 || (type !== 2 && type !== 6)) {
    throw new Error(`unsupported PNG: depth ${depth} type ${type} interlace ${interlace}`);
  }
  const ch = type === 6 ? 4 : 3, stride = w * ch;
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const out = Buffer.alloc(h * stride);
  for (let y = 0; y < h; y++) {
    const f = raw[y * (stride + 1)], src = y * (stride + 1) + 1, dst = y * stride, up = dst - stride;
    for (let x = 0; x < stride; x++) {
      const a = x >= ch ? out[dst + x - ch] : 0;
      const b = y > 0 ? out[up + x] : 0;
      const c = x >= ch && y > 0 ? out[up + x - ch] : 0;
      let v = raw[src + x];
      if (f === 1) v += a;
      else if (f === 2) v += b;
      else if (f === 3) v += (a + b) >> 1;
      else if (f === 4) {
        const q = a + b - c, pa = Math.abs(q - a), pb = Math.abs(q - b), pc = Math.abs(q - c);
        v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      out[dst + x] = v & 255;
    }
  }
  return { w, h, ch, data: out };
}

/** Area-average the export's luma down to `w` px wide (what averaging its
 *  pixels gives, the model the grain gain follows) and measure detail there,
 *  the same mean |Laplacian| as the preview. */
function exportDetail(png: Buffer, w: number) {
  const img = decodePng(png);
  const { w: sw, h: sh, ch, data: d } = img;
  const W = w, H = Math.round((w * sh) / sw);
  const sum = new Float64Array(W * H), cnt = new Uint32Array(W * H);
  const fx = new Uint32Array(sw);
  for (let i = 0; i < sw; i++) fx[i] = Math.min(W - 1, Math.floor((i * W) / sw));
  for (let y = 0; y < sh; y++) {
    const row = Math.min(H - 1, Math.floor((y * H) / sh)) * W;
    for (let x = 0; x < sw; x++) {
      const i = (y * sw + x) * ch;
      sum[row + fx[x]] += 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
      cnt[row + fx[x]]++;
    }
  }
  const L = (i: number) => sum[i] / cnt[i];
  let s = 0, k = 0;
  for (let y = 1; y < H - 1; y++) for (let x = 1; x < W - 1; x++) {
    const i = y * W + x;
    s += Math.abs(4 * L(i) - L(i - 1) - L(i + 1) - L(i - W) - L(i + W)); k++;
  }
  return { detail: s / k, dims: [sw, sh] };
}

const EFFECTS = [
  // Before #47 these ratios were 4.6 (Blur) and about 0.17 (Grain).
  { name: 'Blur', re: /^Blur/, value: 60, lo: 0.8, hi: 1.25 },
  { name: 'Grain', re: /^Grain/, value: 50, lo: 0.88, hi: 1.14 },
];

const SCREENS = [
  // A laptop-sized window at dpr 1 gives the smallest preview: grain cells
  // smaller than its pixels, and the widest blur kernel relative to it.
  { name: 'laptop', viewport: { width: 1280, height: 800 }, dpr: 1, effects: ['Blur', 'Grain'] },
  // A large hi-dpi preview, about 2900 px: grain cells wider than its pixels.
  // Point-sampled grain measured 0.67 here (#51).
  { name: 'hi-dpi', viewport: { width: 1600, height: 1000 }, dpr: 2, effects: ['Grain'] },
];

for (const screen of SCREENS) {
  test.describe(`${screen.name} screen`, () => {
    test.use({ viewport: screen.viewport, deviceScaleFactor: screen.dpr });
    for (const fx of EFFECTS.filter((e) => screen.effects.includes(e.name))) {
      test(`${fx.name} looks the same in the preview and the full-size export (#47, #51)`, async ({ page }, testInfo) => {
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
        const exported = exportDetail(png, shown.w);
        expect(exported.dims).toEqual([6000, 4000]);
        const ratio = exported.detail / shown.detail;
        const key = `${screen.name}_${fx.name.toLowerCase()}`;
        recordMetrics(testInfo, {
          [`${key}_preview_px`]: shown.w,
          [`${key}_detail_base`]: Math.round(base.detail * 1000) / 1000,
          [`${key}_detail_preview`]: Math.round(shown.detail * 1000) / 1000,
          [`${key}_detail_export`]: Math.round(exported.detail * 1000) / 1000,
          [`${key}_export_over_preview`]: Math.round(ratio * 1000) / 1000,
        });
        expect(ratio, `export/preview detail ${ratio.toFixed(3)} at ${shown.w} px`).toBeGreaterThanOrEqual(fx.lo);
        expect(ratio, `export/preview detail ${ratio.toFixed(3)} at ${shown.w} px`).toBeLessThanOrEqual(fx.hi);
        expect(errors).toEqual([]);
      });
    }
  });
}
