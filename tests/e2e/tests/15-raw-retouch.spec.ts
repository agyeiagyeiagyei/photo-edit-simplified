import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import {
  openApp, importFiles, thumbs, canvas, canvasSize, clickTab, setRange,
  trackErrors, recordMetrics, imageDims, rawFixtures,
} from './helpers';

// Issue #42: a Heal/Clone on a RAW is recorded in the recipe (mode, radius,
// original-image path, clone source, seed) and replayed by the worker after
// develop, so the item stays a RAW across a reload and the stroke rides later
// edits (exposure) and the export. The old behaviour baked the stroke into an
// 8-bit raster (gen > 0), which dropped RAW develop on reload.
const RAWS = rawFixtures();
const MiB = 1048576;

/** Sensor develop has landed: canvas past the camera-JPEG tier, the develop
 *  mark recorded, and the interim "Developing RAW…" badge gone. */
async function waitDeveloped(page: Page) {
  await expect.poll(async () => (await canvasSize(page)).w, { timeout: 60_000 }).toBeGreaterThan(1200);
  await expect.poll(async () => (await canvasSize(page)).w, { timeout: 60_000 }).toBeGreaterThan(2000);
  await expect
    .poll(
      () =>
        page.evaluate(() =>
          ((window as any).__pes?.perf?.() || []).some(
            (e: any) => e.name === 'proxy.raw_develop' || e.name === 'pool.develop',
          ),
        ),
      { timeout: 60_000 },
    )
    .toBe(true);
  await expect(page.locator('.developing-badge')).toHaveCount(0, { timeout: 30_000 });
  // Let any hi render that the develop triggered settle.
  await page.waitForTimeout(1000);
}

/**
 * Luma of the normalized box at (x, y) ± half, resampled to an n×n grid so a
 * comparison survives the canvas changing resolution between renders (the
 * proxy tier and the device-pixel hi tier differ).
 */
async function boxLuma(page: Page, x: number, y: number, half: number, n = 40): Promise<number[]> {
  return page.evaluate(
    ({ x, y, half, n }) => {
      const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement | null;
      if (!c || !c.width || !c.height) return new Array(n * n).fill(0);
      const ctx = c.getContext('2d')!;
      const x0 = Math.max(0, Math.floor((x - half) * c.width));
      const y0 = Math.max(0, Math.floor((y - half) * c.height));
      const w = Math.max(1, Math.min(c.width - x0, Math.round(half * 2 * c.width)));
      const h = Math.max(1, Math.min(c.height - y0, Math.round(half * 2 * c.height)));
      const img = ctx.getImageData(x0, y0, w, h);
      const mid = document.createElement('canvas');
      mid.width = w;
      mid.height = h;
      mid.getContext('2d')!.putImageData(img, 0, 0);
      const out = document.createElement('canvas');
      out.width = n;
      out.height = n;
      const octx = out.getContext('2d')!;
      octx.drawImage(mid, 0, 0, w, h, 0, 0, n, n);
      const d = octx.getImageData(0, 0, n, n).data;
      const v: number[] = [];
      for (let i = 0; i < d.length; i += 4) {
        v.push(0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2]);
      }
      return v;
    },
    { x, y, half, n },
  );
}

const mean = (v: number[]) => v.reduce((s, x) => s + x, 0) / v.length;
const std = (v: number[]) => {
  const m = mean(v);
  return Math.sqrt(v.reduce((s, x) => s + (x - m) * (x - m), 0) / v.length);
};
const mad = (a: number[], b: number[]) => {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += Math.abs(a[i] - b[i]);
  return s / a.length;
};

/**
 * PSNR (RGB) between the preview canvas and the export inside one normalized
 * box, with both surfaces drawn down to 1024 px wide first. The two renders
 * come from different source buffers (proxy vs sensor), so equality has to be
 * judged at a common resolution.
 */
async function exportBoxPsnr(
  page: Page,
  bytes: Buffer,
  mime: string,
  at: { x: number; y: number },
  half: number,
): Promise<number> {
  return page.evaluate(
    async ({ b64, mime, at, half }) => {
      const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement | null;
      if (!c || !c.width || !c.height) return -1;
      const bin = atob(b64);
      const u8 = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
      const bmp = await createImageBitmap(new Blob([u8], { type: mime }));
      const at1024 = (src: CanvasImageSource, sw: number, sh: number) => {
        const w = 1024;
        const h = Math.max(1, Math.round(sh * (w / sw)));
        const o = document.createElement('canvas');
        o.width = w;
        o.height = h;
        const ctx = o.getContext('2d')!;
        ctx.imageSmoothingQuality = 'high';
        ctx.drawImage(src, 0, 0, w, h);
        return { ctx, w, h };
      };
      const a = at1024(c, c.width, c.height);
      const b = at1024(bmp, bmp.width, bmp.height);
      bmp.close();
      const x0 = Math.max(0, Math.floor((at.x - half) * a.w));
      const y0 = Math.max(0, Math.floor((at.y - half) * a.h));
      const bw = Math.max(1, Math.min(a.w - x0, Math.round(half * 2 * a.w)));
      const bh = Math.max(1, Math.min(a.h - y0, Math.round(half * 2 * a.h)));
      const da = a.ctx.getImageData(x0, y0, bw, bh).data;
      const db = b.ctx.getImageData(x0, y0, bw, bh).data;
      let mse = 0;
      let n = 0;
      for (let i = 0; i < da.length; i += 4) {
        for (let k = 0; k < 3; k++) {
          const d = da[i + k] - db[i + k];
          mse += d * d;
          n++;
        }
      }
      mse /= n;
      return mse > 0 ? 10 * Math.log10((255 * 255) / mse) : 99;
    },
    { b64: bytes.toString('base64'), mime, at, half },
  );
}

type Box = { x: number; y: number; m: number; sd: number };

/** Grid of 3%-wide boxes over the middle of the canvas, with mean luma + std. */
async function gridBoxes(page: Page): Promise<Box[] | null> {
  return page.evaluate(() => {
    const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement | null;
    if (!c || !c.width || !c.height) return null;
    const ctx = c.getContext('2d')!;
    const W = c.width;
    const H = c.height;
    const bw = Math.max(8, Math.floor(W * 0.03));
    const bh = Math.max(8, Math.floor(H * 0.03));
    const out: { x: number; y: number; m: number; sd: number }[] = [];
    for (let gy = 0; gy < 7; gy++) {
      for (let gx = 0; gx < 7; gx++) {
        const nx = 0.12 + gx * (0.76 / 6);
        const ny = 0.12 + gy * (0.76 / 6);
        const x0 = Math.min(W - bw, Math.max(0, Math.round(nx * W - bw / 2)));
        const y0 = Math.min(H - bh, Math.max(0, Math.round(ny * H - bh / 2)));
        const d = ctx.getImageData(x0, y0, bw, bh).data;
        let s = 0;
        let s2 = 0;
        for (let i = 0; i < d.length; i += 4) {
          const y = 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
          s += y;
          s2 += y * y;
        }
        const cnt = d.length / 4;
        const m = s / cnt;
        out.push({ x: nx, y: ny, m, sd: Math.sqrt(Math.max(0, s2 / cnt - m * m)) });
      }
    }
    return out;
  });
}

const dist = (a: { x: number; y: number }, b: { x: number; y: number }) =>
  Math.hypot(a.x - b.x, a.y - b.y);

/** Textured box to heal, plus a mid-tone untouched control box far from it. */
async function pickHealAndControl(page: Page) {
  const boxes = await gridBoxes(page);
  if (!boxes) return null;
  const heal = boxes.reduce((a, b) => (b.sd > a.sd ? b : a));
  const med = [...boxes].sort((a, b) => a.m - b.m)[Math.floor(boxes.length / 2)].m;
  let control: Box | null = null;
  for (const b of boxes) {
    if (dist(b, heal) < 0.24) continue;
    if (!control || Math.abs(b.m - med) < Math.abs(control.m - med)) control = b;
  }
  return { heal, control: control ?? heal, median: med };
}

/** Highest-contrast pair at least 0.3 apart: clone the brighter box onto the
 *  darker one. Real fixtures run dark (DSC05290 tops out near delta 18), so the
 *  picker maximises the measurable difference instead of assuming a mid-tone. */
async function pickCloneBoxes(page: Page) {
  const boxes = await gridBoxes(page);
  if (!boxes) return null;
  let src: Box | null = null;
  let dest: Box | null = null;
  let best = 0;
  for (const a of boxes) {
    for (const b of boxes) {
      if (dist(a, b) < 0.3) continue;
      const d = Math.abs(a.m - b.m);
      if (d > best) {
        best = d;
        if (a.m >= b.m) {
          src = a;
          dest = b;
        } else {
          src = b;
          dest = a;
        }
      }
    }
  }
  return src && dest ? { src, dest } : null;
}

async function clickNorm(page: Page, x: number, y: number) {
  const b = await canvas(page).boundingBox();
  await canvas(page).click({ position: { x: b!.width * x, y: b!.height * y } });
}

type SessionRec = {
  raw: boolean;
  raster: boolean;
  hs: any[] | null;
};

/** First item record: raw/raster flags and the parsed `hs` stroke list. */
async function itemRecord(page: Page): Promise<SessionRec | null> {
  return page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((res, rej) => {
      const req = indexedDB.open('pes-session');
      req.onsuccess = () => res(req.result);
      req.onerror = () => rej(req.error);
    });
    try {
      const vals = await new Promise<any[]>((res) => {
        const rq = db.transaction('items', 'readonly').objectStore('items').getAll();
        rq.onsuccess = () => res(rq.result ?? []);
        rq.onerror = () => res([]);
      });
      vals.sort((a, b) => (a.ord ?? 0) - (b.ord ?? 0));
      const rec = vals[0];
      if (!rec) return null;
      let recipe: any = null;
      try {
        recipe = rec.recipe ? JSON.parse(rec.recipe) : null;
      } catch {}
      return {
        raw: rec.raw === true,
        raster: rec.raster === true,
        hs: Array.isArray(recipe?.hs) ? recipe.hs : null,
      };
    } finally {
      db.close();
    }
  });
}

type BoxPair = { src: number[]; dest: number[] };

/** Sample two normalized boxes out of an encoded image (export bytes). */
async function imageBoxPair(
  page: Page,
  bytes: Buffer,
  mime: string,
  srcAt: { x: number; y: number },
  destAt: { x: number; y: number },
  half: number,
  n = 40,
): Promise<BoxPair> {
  return page.evaluate(
    async ({ b64, mime, srcAt, destAt, half, n }) => {
      const bin = atob(b64);
      const u8 = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
      const bmp = await createImageBitmap(new Blob([u8], { type: mime }));
      const full = document.createElement('canvas');
      full.width = bmp.width;
      full.height = bmp.height;
      full.getContext('2d')!.drawImage(bmp, 0, 0);
      const ctx = full.getContext('2d')!;
      const sample = (p: { x: number; y: number }) => {
        const x0 = Math.max(0, Math.floor((p.x - half) * full.width));
        const y0 = Math.max(0, Math.floor((p.y - half) * full.height));
        const w = Math.max(1, Math.min(full.width - x0, Math.round(half * 2 * full.width)));
        const h = Math.max(1, Math.min(full.height - y0, Math.round(half * 2 * full.height)));
        const img = ctx.getImageData(x0, y0, w, h);
        const mid = document.createElement('canvas');
        mid.width = w;
        mid.height = h;
        mid.getContext('2d')!.putImageData(img, 0, 0);
        const o = document.createElement('canvas');
        o.width = n;
        o.height = n;
        const octx = o.getContext('2d')!;
        octx.drawImage(mid, 0, 0, w, h, 0, 0, n, n);
        const d = octx.getImageData(0, 0, n, n).data;
        const v: number[] = [];
        for (let i = 0; i < d.length; i += 4) v.push(0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2]);
        return v;
      };
      const out = { src: sample(srcAt), dest: sample(destAt) };
      bmp.close();
      return out;
    },
    { b64: bytes.toString('base64'), mime, srcAt, destAt, half, n },
  );
}

/** PSNR between the visible canvas and the export drawn down to its size. */
async function psnrCanvasVsExport(page: Page, bytes: Buffer, mime: string): Promise<number> {
  return page.evaluate(
    async ({ b64, mime }) => {
      const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement | null;
      if (!c || !c.width || !c.height) return -1;
      const bin = atob(b64);
      const u8 = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
      const bmp = await createImageBitmap(new Blob([u8], { type: mime }));
      const exp = document.createElement('canvas');
      exp.width = c.width;
      exp.height = c.height;
      exp.getContext('2d')!.drawImage(bmp, 0, 0, c.width, c.height);
      const a = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
      const b = exp.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
      let mse = 0;
      let n = 0;
      for (let i = 0; i < a.length; i += 4) {
        for (let k = 0; k < 3; k++) {
          const d = a[i + k] - b[i + k];
          mse += d * d;
          n++;
        }
      }
      bmp.close();
      mse /= n;
      return mse > 0 ? 10 * Math.log10((255 * 255) / mse) : 99;
    },
    { b64: bytes.toString('base64'), mime },
  );
}

test.describe('RAW retouch (#42): non-destructive Heal/Clone', () => {
  test.skip(RAWS.length === 0, 'no RAW fixture in dl-raw; run gen_dng.py to create one');

  for (const RAW of RAWS) {
    const name = RAW.split('/').pop()!;

    test(`RAW ${name} keeps a healed area and RAW develop after reload`, async ({ page }, testInfo) => {
      test.setTimeout(300_000);
      const errors = trackErrors(page);

      await openApp(page);
      await importFiles(page, [RAW]);
      await waitDeveloped(page);

      await clickTab(page, 'Heal');
      await page.locator('.heal-panel .chip', { hasText: /^Smooth fill$/ }).click();
      await setRange(page, /^Brush size:/, 0.06);
      await page.waitForTimeout(300);

      const pick = await pickHealAndControl(page);
      expect(pick, 'canvas had no pixels to sample').not.toBeNull();
      const { heal, control } = pick!;
      const half = 0.02;
      const pre = await boxLuma(page, heal.x, heal.y, half);
      const preSd = std(pre);
      const preControl = await boxLuma(page, control.x, control.y, half);
      expect(preSd, 'picked heal spot has no texture to flatten').toBeGreaterThan(4);

      await clickNorm(page, heal.x, heal.y);
      await expect
        .poll(async () => std(await boxLuma(page, heal.x, heal.y, half)) / preSd, { timeout: 120_000 })
        .toBeLessThan(0.8);
      await page.waitForTimeout(400);
      const post = await boxLuma(page, heal.x, heal.y, half);
      const postMad = mad(pre, post);

      // The session record must stay a RAW whose recipe carries the stroke.
      await expect
        .poll(async () => (await itemRecord(page))?.hs?.length ?? 0, { timeout: 30_000 })
        .toBeGreaterThan(0);
      const rec = await itemRecord(page);
      expect(rec?.raw, 'raw flag dropped after a heal').toBe(true);
      expect(rec?.raster, 'heal was baked into a raster').toBe(false);
      const hs = rec!.hs!;
      expect(hs[0].m, 'mode').toBe(1); // Smooth fill
      expect(Math.abs(hs[0].r - 0.06)).toBeLessThan(0.001);
      expect(Math.abs(hs[0].p[0][0] - heal.x)).toBeLessThan(0.02);
      expect(Math.abs(hs[0].p[0][1] - heal.y)).toBeLessThan(0.02);

      // Reload: the RAW develop panel returns and the stroke replays. The
      // discriminator is texture, not colour — the restored proxy may differ
      // from the developed frame until the hi render lands, but a lost stroke
      // would bring the original texture back.
      await page.reload();
      await expect(thumbs(page)).toHaveCount(1, { timeout: 120_000 });
      await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 120_000 });
      await expect
        .poll(async () => std(await boxLuma(page, heal.x, heal.y, half)) / preSd, {
          timeout: 120_000,
          intervals: [500],
        })
        .toBeLessThan(0.85);
      await page.waitForTimeout(500);
      const reload = await boxLuma(page, heal.x, heal.y, half);
      const reloadMad = mad(pre, reload);

      await clickTab(page, 'RAW');
      await expect(page.locator('.raw-panel .panel-title', { hasText: 'RAW develop' })).toBeVisible();

      // A later RAW edit still applies on top of the recorded stroke, and the
      // heal stays smooth at the new exposure.
      const controlBefore = mean(await boxLuma(page, control.x, control.y, half));
      await page.locator('label.slider', { hasText: /^Exposure/ }).locator('input').evaluate((el: HTMLInputElement) => {
        el.value = '1'; el.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await expect
        .poll(async () => mean(await boxLuma(page, control.x, control.y, half)) - controlBefore, {
          timeout: 60_000,
          intervals: [500],
        })
        .toBeGreaterThan(5);
      await expect
        .poll(async () => std(await boxLuma(page, heal.x, heal.y, half)) / preSd, { timeout: 60_000 })
        .toBeLessThan(0.85);
      const exposedSd = std(await boxLuma(page, heal.x, heal.y, half));

      recordMetrics(testInfo, {
        raw_fixture: name,
        heal_pre_sd: Math.round(preSd * 10) / 10,
        heal_post_sd: Math.round(std(post) * 10) / 10,
        heal_post_mad: Math.round(postMad * 10) / 10,
        heal_reload_sd: Math.round(std(reload) * 10) / 10,
        heal_reload_mad: Math.round(reloadMad * 10) / 10,
        heal_exposed_sd: Math.round(exposedSd * 10) / 10,
        heal_control_mean_delta: Math.round((mean(await boxLuma(page, control.x, control.y, half)) - controlBefore) * 10) / 10,
        heal_record_raw: rec!.raw,
        heal_record_raster: rec!.raster,
      });

      expect(errors, errors.join('\n')).toEqual([]);
    });

    test(`RAW ${name} replays a clone through exposure and the export`, async ({ page }, testInfo) => {
      test.setTimeout(360_000);
      const errors = trackErrors(page);

      await openApp(page);
      await importFiles(page, [RAW]);
      await waitDeveloped(page);

      const pick = await pickCloneBoxes(page);
      expect(pick, 'canvas had no pixels to sample').not.toBeNull();
      const { src, dest } = pick!;
      const half = 0.02;
      const srcPre = await boxLuma(page, src.x, src.y, half);
      const destPre = await boxLuma(page, dest.x, dest.y, half);
      const preDelta = Math.abs(mean(srcPre) - mean(destPre));
      // DSC05290 is a dark frame whose largest grid-pair delta is ~18.5; that
      // is still 3x the 6-level tolerance the clone is checked against below.
      expect(preDelta, 'source and destination look alike; clone would prove nothing').toBeGreaterThan(12);

      await clickTab(page, 'Clone');
      await setRange(page, /^Brush size:/, 0.07);
      await page.locator('.clone-panel .chip', { hasText: /^Set source$/ }).click();
      await clickNorm(page, src.x, src.y);
      await clickNorm(page, dest.x, dest.y);

      // Sample off the Clone tab: its persistent source crosshair is about the
      // size of the box and would skew the source mean. The RAW tab holds the
      // Exposure slider used below.
      await clickTab(page, 'RAW');

      // Preview: the destination now renders the source content.
      await expect
        .poll(
          async () =>
            Math.abs(
              mean(await boxLuma(page, dest.x, dest.y, half)) - mean(await boxLuma(page, src.x, src.y, half)),
            ),
          { timeout: 120_000, intervals: [500] },
        )
        .toBeLessThan(6);
      await page.waitForTimeout(300);
      const srcCloned = await boxLuma(page, src.x, src.y, half);
      const destCloned = await boxLuma(page, dest.x, dest.y, half);
      const cloneDelta = Math.abs(mean(srcCloned) - mean(destCloned));

      await expect
        .poll(async () => (await itemRecord(page))?.hs?.length ?? 0, { timeout: 30_000 })
        .toBeGreaterThan(0);
      const rec = await itemRecord(page);
      expect(rec?.raw, 'raw flag dropped after a clone').toBe(true);
      expect(rec?.raster, 'clone was baked into a raster').toBe(false);
      const stroke = rec!.hs![0];
      expect(stroke.m, 'mode').toBe(3); // Clone
      expect(stroke.src, 'clone source missing from the recipe').toBeTruthy();
      expect(Math.abs(stroke.src[0] - src.x)).toBeLessThan(0.02);
      expect(Math.abs(stroke.src[1] - src.y)).toBeLessThan(0.02);

      // Exposure +1 must apply without losing the clone.
      const srcBeforeExp = mean(await boxLuma(page, src.x, src.y, half));
      await page.locator('label.slider', { hasText: /^Exposure/ }).locator('input').evaluate((el: HTMLInputElement) => {
        el.value = '1'; el.dispatchEvent(new Event('input', { bubbles: true }));
      });
      await expect
        .poll(async () => mean(await boxLuma(page, src.x, src.y, half)) - srcBeforeExp, {
          timeout: 120_000,
          intervals: [500],
        })
        .toBeGreaterThan(5);
      await expect
        .poll(
          async () =>
            Math.abs(
              mean(await boxLuma(page, dest.x, dest.y, half)) - mean(await boxLuma(page, src.x, src.y, half)),
            ),
          { timeout: 60_000 },
        )
        .toBeLessThan(8);
      const srcExposed = await boxLuma(page, src.x, src.y, half);
      const destExposed = await boxLuma(page, dest.x, dest.y, half);
      const deltaExposed = Math.abs(mean(srcExposed) - mean(destExposed));

      // Export: sensor-size decode, then the same boxes must still match —
      // proof the export replayed the stroke after develop.
      await clickTab(page, 'Export');
      const [dl] = await Promise.all([
        page.waitForEvent('download', { timeout: 240_000 }),
        page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
      ]);
      const bytes = fs.readFileSync((await dl.path())!);
      const dims = await imageDims(page, bytes, 'image/jpeg');
      expect(dims.w * dims.h, 'export is not at sensor resolution').toBeGreaterThan(4_000_000);
      const pair = await imageBoxPair(page, bytes, 'image/jpeg', src, dest, half);
      const exportDelta = Math.abs(mean(pair.src) - mean(pair.dest));
      const psnr = await psnrCanvasVsExport(page, bytes, 'image/jpeg');

      recordMetrics(testInfo, {
        raw_fixture: name,
        clone_src_luma: Math.round(mean(srcPre)),
        clone_dest_luma: Math.round(mean(destPre)),
        clone_pre_delta: Math.round(preDelta * 10) / 10,
        clone_preview_delta: Math.round(cloneDelta * 10) / 10,
        clone_exposed_delta: Math.round(deltaExposed * 10) / 10,
        clone_export_delta: Math.round(exportDelta * 10) / 10,
        clone_export_psnr_db: Math.round(psnr * 10) / 10,
        clone_export_w: dims.w,
        clone_export_h: dims.h,
        clone_record_raw: rec!.raw,
        clone_record_raster: rec!.raster,
      });

      expect(exportDelta, 'export did not replay the clone stroke').toBeLessThan(8);
      expect(psnr, 'preview-vs-export PSNR').toBeGreaterThan(22);
      expect(errors, errors.join('\n')).toEqual([]);
    });

    test(`RAW ${name} replays a content-aware heal identically in preview and export`, async ({ page }, testInfo) => {
      test.setTimeout(420_000);
      const errors = trackErrors(page);

      await openApp(page);
      await importFiles(page, [RAW]);
      await waitDeveloped(page);

      await clickTab(page, 'Heal');
      await page.locator('.heal-panel .chip', { hasText: /^Content-aware$/ }).click();
      await setRange(page, /^Brush size:/, 0.06);
      await page.waitForTimeout(300);

      const pick = await pickHealAndControl(page);
      expect(pick, 'canvas had no pixels to sample').not.toBeNull();
      const { heal } = pick!;
      const half = 0.02;
      const pre = await boxLuma(page, heal.x, heal.y, half);
      const preSd = std(pre);
      expect(preSd, 'picked heal spot has no texture to flatten').toBeGreaterThan(4);

      await clickNorm(page, heal.x, heal.y);
      await expect
        .poll(async () => std(await boxLuma(page, heal.x, heal.y, half)) / preSd, { timeout: 120_000 })
        .toBeLessThan(0.8);

      // The chosen patch must be pinned in the recipe at stroke time: a
      // replay that searches again would find different pixels at export.
      await expect
        .poll(async () => (await itemRecord(page))?.hs?.[0]?.src?.length ?? 0, { timeout: 30_000 })
        .toBe(2);
      const rec = await itemRecord(page);
      expect(rec?.raw, 'raw flag dropped after a heal').toBe(true);
      expect(rec?.raster, 'heal was baked into a raster').toBe(false);
      const stroke = rec!.hs![0];
      expect(stroke.m, 'mode').toBe(0); // Content-aware

      // Off the Heal tab so no brush cursor sits over the sampled box.
      await clickTab(page, 'RAW');
      await page.waitForTimeout(400);
      const previewSd = std(await boxLuma(page, heal.x, heal.y, half));

      // Export: sensor-size decode. Inside the healed box the export must
      // match the preview despite the 2888 px vs 6192 px replay resolution.
      await clickTab(page, 'Export');
      const [dl] = await Promise.all([
        page.waitForEvent('download', { timeout: 240_000 }),
        page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
      ]);
      const bytes = fs.readFileSync((await dl.path())!);
      const dims = await imageDims(page, bytes, 'image/jpeg');
      expect(dims.w * dims.h, 'export is not at sensor resolution').toBeGreaterThan(4_000_000);
      const boxPsnr = await exportBoxPsnr(page, bytes, 'image/jpeg', heal, half);
      const psnr = await psnrCanvasVsExport(page, bytes, 'image/jpeg');

      recordMetrics(testInfo, {
        raw_fixture: name,
        heal_ca_pre_sd: Math.round(preSd * 10) / 10,
        heal_ca_preview_sd: Math.round(previewSd * 10) / 10,
        heal_ca_anchor: `${stroke.src[0].toFixed(4)},${stroke.src[1].toFixed(4)}`,
        heal_ca_box_psnr_db: Math.round(boxPsnr * 10) / 10,
        heal_ca_export_psnr_db: Math.round(psnr * 10) / 10,
        heal_ca_export_w: dims.w,
        heal_ca_export_h: dims.h,
      });

      expect(boxPsnr, 'preview-vs-export PSNR inside the healed box').toBeGreaterThan(30);
      expect(errors, errors.join('\n')).toEqual([]);
    });
  }
});
