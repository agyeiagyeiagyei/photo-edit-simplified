import { test, expect, Page } from '@playwright/test';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import {
  openApp, trackErrors, importFiles, fileInput, canvas, canvasSize, clickTab, setRange, thumbs,
  JPEG_A, PNG_SMALL, RING, rawFixtures,
} from './helpers';

// Issue #46 — view zoom: a viewport magnifier separate from crop. Covers the
// cluster and keys, the crop/rotate/export reset rules, pointer mapping through
// the view window, pan clamping, nearest-neighbour above native density, the
// close-up brush range (R1) and export identity with the view zoomed.

type Hs = { m: number; r: number; p: [number, number][] };
type Crop = { x: number; y: number; w: number; h: number };
type Recipe = { raw: boolean; fineAngle: number | null; crop: Crop | null; hs: Hs[]; text: string };

const pct = (page: Page) => page.locator('.zoom-cluster .zoom-pct');
const pctNum = async (page: Page) => Number((await pct(page).textContent())!.replace('%', ''));

/** The filmstrip sorts by capture time then name, not import order. */
const thumbNamed = (page: Page, name: string) => page.locator(`.filmstrip .thumb[title="${name}"]`);

async function zoomIn(page: Page, n = 1) {
  for (let i = 0; i < n; i++) {
    await page.locator('.zoom-cluster button[title="Zoom in (+)"]').click();
    await page.waitForTimeout(120);
  }
}

async function canvasBox(page: Page) {
  const b = await canvas(page).boundingBox();
  if (!b) throw new Error('canvas has no box');
  return b;
}

async function middleDrag(page: Page, dx: number, dy = 0) {
  const b = await canvasBox(page);
  const x = b.x + b.width / 2;
  const y = b.y + b.height / 2;
  await page.mouse.move(x, y);
  await page.mouse.down({ button: 'middle' });
  for (let i = 1; i <= 8; i++) {
    await page.mouse.move(x + (dx * i) / 8, y + (dy * i) / 8);
    await page.waitForTimeout(16);
  }
  await page.mouse.up({ button: 'middle' });
  await page.waitForTimeout(250);
}

/** First item's stored recipe, parsed, plus the raw JSON text. */
async function itemRecipe(page: Page): Promise<Recipe> {
  const r = await page.evaluate(async () => {
    const db = await new Promise<IDBDatabase>((res, rej) => {
      const q = indexedDB.open('pes-session');
      q.onsuccess = () => res(q.result);
      q.onerror = () => rej(q.error);
    });
    try {
      const vals = await new Promise<any[]>((res) => {
        const q = db.transaction('items', 'readonly').objectStore('items').getAll();
        q.onsuccess = () => res(q.result ?? []);
        q.onerror = () => res([]);
      });
      vals.sort((a, b) => (a.ord ?? 0) - (b.ord ?? 0));
      const rec = vals[0];
      return { raw: rec?.raw === true, text: rec?.recipe ?? '' };
    } finally {
      db.close();
    }
  });
  let j: any = {};
  try { j = JSON.parse(r.text); } catch {}
  return {
    raw: r.raw, text: r.text, fineAngle: j.fine_angle ?? null,
    crop: j.crop && typeof j.crop === 'object' ? j.crop : null,
    hs: Array.isArray(j.hs) ? j.hs : [],
  };
}

async function waitHs(page: Page, n: number) {
  await expect.poll(async () => (await itemRecipe(page)).hs.length, { timeout: 15_000 }).toBeGreaterThanOrEqual(n);
}

/** Click a canvas-relative point, optionally with a specific pointer button. */
async function canvasClick(page: Page, nx: number, ny: number) {
  const b = await canvasBox(page);
  await page.mouse.click(b.x + b.width * nx, b.y + b.height * ny);
  await page.waitForTimeout(220);
}

test.describe('view zoom (#46)', () => {
  test('cluster, keys, focus guard and reset rules', async ({ page }) => {
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [RING, PNG_SMALL]);
    await clickTab(page, 'Color');
    await expect(page.locator('.zoom-cluster')).toBeVisible();

    const fitPct = await pctNum(page);
    // ring.jpg is 1536 px wide over a ~1155 px canvas: Fit sits below native.
    expect(fitPct).toBeGreaterThan(60);
    expect(fitPct).toBeLessThan(95);

    await zoomIn(page);
    await expect.poll(() => pctNum(page)).toBeGreaterThan(fitPct * 1.3);

    // Keyboard needs the pointer over the canvas: focus alone must not win.
    const b = await canvasBox(page);
    await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
    await page.keyboard.press('0');
    await expect.poll(() => pctNum(page)).toBeCloseTo(fitPct, 0);
    await page.keyboard.press('1');
    await expect.poll(() => pctNum(page)).toBeCloseTo(100, 0);
    await page.keyboard.press('+');
    await expect.poll(() => pctNum(page)).toBeCloseTo(140, 0);
    await page.keyboard.press('-');
    await expect.poll(() => pctNum(page)).toBeCloseTo(100, 0);

    // R4: a focused slider keeps its arrows, and + does not zoom until the
    // pointer is over the canvas.
    await page.mouse.move(b.x + b.width + 240, b.y + 100);
    const slider = page.locator('label').filter({ hasText: /^Brightness/ }).first().locator('input[type=range]');
    await slider.focus();
    const before = Number(await slider.inputValue());
    await page.keyboard.press('ArrowRight');
    await expect.poll(async () => Number(await slider.inputValue())).toBeGreaterThan(before);
    const pctBefore = await pctNum(page);
    await page.keyboard.press('+');
    await page.waitForTimeout(150);
    expect(await pctNum(page)).toBeCloseTo(pctBefore, 0);

    // Space-pan must swallow the release so a focused button is not pressed.
    await zoomIn(page);
    await page.locator('.zoom-cluster button.zoom-fit').focus();
    await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
    await page.keyboard.press('Space');
    await page.waitForTimeout(150);
    expect(await pctNum(page), 'Space over the canvas pressed the focused Fit button').toBeGreaterThan(fitPct * 1.3);

    // Entering Crop or Rotate resets to Fit; Export keeps the view.
    await clickTab(page, 'Crop');
    await expect(page.locator('.zoom-cluster')).toHaveCount(0);
    await clickTab(page, 'Color');
    await expect.poll(() => pctNum(page)).toBeCloseTo(fitPct, 0);

    await zoomIn(page);
    await clickTab(page, 'Rotate');
    await clickTab(page, 'Color');
    await expect.poll(() => pctNum(page)).toBeCloseTo(fitPct, 0);

    await zoomIn(page);
    const keep = await pctNum(page);
    await clickTab(page, 'Export');
    await expect.poll(() => pctNum(page)).toBeCloseTo(keep, 0);
    await clickTab(page, 'Color');

    // Photo switch resets in both directions, and so does a reload.
    await thumbNamed(page, 'small.png').click();
    await page.waitForTimeout(500);
    const pct2Fit = await pctNum(page);
    await thumbNamed(page, 'ring.jpg').click();
    await page.waitForTimeout(500);
    const pct1Fit = await pctNum(page);
    expect(pct1Fit).toBeCloseTo(fitPct, 0);
    await zoomIn(page);
    await thumbNamed(page, 'small.png').click();
    await page.waitForTimeout(500);
    expect(await pctNum(page)).toBeCloseTo(pct2Fit, 0);
    await thumbNamed(page, 'ring.jpg').click();
    await page.waitForTimeout(500);
    expect(await pctNum(page)).toBeCloseTo(pct1Fit, 0);

    await zoomIn(page);
    await page.reload();
    await expect(thumbs(page)).toHaveCount(2, { timeout: 60_000 });
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await thumbNamed(page, 'ring.jpg').click();
    await clickTab(page, 'Color'); // the app always boots on Crop
    await page.waitForTimeout(500);
    await expect.poll(() => pctNum(page)).toBeCloseTo(pct1Fit, 0);

    expect(errors).toEqual([]);
  });

  test('pointer mapping through the view window, pan and clamping', async ({ page }) => {
    const raws = rawFixtures();
    test.skip(raws.length === 0, 'no RAW fixture in dl-raw');
    const raw = raws.find((p) => /synthetic/i.test(p)) || raws[0];
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [raw]);
    await clickTab(page, 'Heal');
    await page.locator('.heal-panel .chip', { hasText: /^Smooth fill$/ }).click();

    // R1: the smallest brush is 2 image px, labelled in pixels.
    const label = page.locator('label').filter({ hasText: /^Brush size:/ }).first();
    const slider = label.locator('input[type=range]');
    await expect(label).toContainText(/px/);
    await expect(slider).toHaveAttribute('data-min-px', '2');
    await slider.evaluate((el: HTMLInputElement) => {
      el.value = '0';
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await expect(label).toContainText(/2\.00 px/);

    // Fit: a dab at the canvas centre is the crop centre.
    await canvasClick(page, 0.5, 0.5);
    await waitHs(page, 1);
    let rec = await itemRecipe(page);
    expect(rec.raw).toBe(true);
    const c0 = rec.hs[0].p[0];
    expect(c0[0]).toBeCloseTo(0.5, 2);
    expect(c0[1]).toBeCloseTo(0.5, 2);
    // 2 px diameter on a radius slider = 1 px radius, stored undivided.
    expect(rec.hs[0].r).toBeGreaterThan(0);

    // Zoom keeps the crop centre under the canvas centre.
    await zoomIn(page); // 1.4× Fit
    await expect.poll(() => pctNum(page)).toBeCloseTo(140, 0);
    await canvasClick(page, 0.5, 0.5);
    await waitHs(page, 2);
    rec = await itemRecipe(page);
    const c1 = rec.hs[1].p[0];
    expect(c1[0]).toBeCloseTo(0.5, 2);
    expect(c1[1]).toBeCloseTo(0.5, 2);

    // Pan right by 10% of the canvas: the point under the centre moves left by
    // 0.1 / view.zoom in crop units (view.zoom = 1.4 relative to Fit).
    const b = await canvasBox(page);
    await middleDrag(page, b.width * 0.1);
    await canvasClick(page, 0.5, 0.5);
    await waitHs(page, 3);
    rec = await itemRecipe(page);
    const c2 = rec.hs[2].p[0];
    expect(c2[0]).toBeCloseTo(0.5 - 0.1 / 1.4, 2);
    expect(c2[1]).toBeCloseTo(0.5, 2);

    // Pan far past the edge: the window clamps at half its width (1/2.8).
    await middleDrag(page, b.width * 3);
    await middleDrag(page, b.width * 3);
    await canvasClick(page, 0.5, 0.5);
    await waitHs(page, 4);
    rec = await itemRecipe(page);
    const c3 = rec.hs[3].p[0];
    expect(c3[0]).toBeCloseTo(0.5 / 1.4, 2);
    expect(c3[1]).toBeGreaterThanOrEqual(0.5 / 1.4 - 0.02);
    expect(c3[1]).toBeLessThanOrEqual(1 - 0.5 / 1.4 + 0.02);

    // Back to Fit: the centre maps to the crop centre again.
    await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
    await page.keyboard.press('0');
    await canvasClick(page, 0.5, 0.5);
    await waitHs(page, 5);
    rec = await itemRecipe(page);
    expect(rec.hs[4].p[0][0]).toBeCloseTo(0.5, 2);
    expect(rec.hs[4].p[0][1]).toBeCloseTo(0.5, 2);

    // R1 precision: the tiny radius survives a reload instead of rounding to 0.
    const rBefore = rec.hs[0].r;
    await page.reload();
    await expect(thumbs(page)).toHaveCount(1, { timeout: 60_000 });
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await expect.poll(async () => (await itemRecipe(page)).hs.length, { timeout: 30_000 }).toBe(5);
    const rAfter = (await itemRecipe(page)).hs[0].r;
    expect(rAfter).toBeGreaterThan(0);
    expect(Math.abs(rAfter - rBefore)).toBeLessThan(1e-6);

    // With a non-square crop set, the same mapping composes crop -> view.
    await clickTab(page, 'Crop');
    await page.locator('.chip', { hasText: /^1:1$/ }).click();
    await expect.poll(async () => (await itemRecipe(page)).crop).not.toBeNull();
    await clickTab(page, 'Heal');
    const crop = (await itemRecipe(page)).crop!;
    expect(crop.w).toBeGreaterThan(0.3);
    await canvasClick(page, 0.25, 0.25);
    await waitHs(page, 6);
    rec = await itemRecipe(page);
    const p5 = rec.hs[5].p[0];
    expect(p5[0]).toBeCloseTo(crop.x + 0.25 * crop.w, 2);
    expect(p5[1]).toBeCloseTo(crop.y + 0.25 * crop.h, 2);

    expect(errors).toEqual([]);
  });

  test('nearest-neighbour above native density; checkerboard stays hard', async ({ page }) => {
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [JPEG_A]);
    await clickTab(page, 'Color');
    const rendering = () => canvas(page).evaluate((c: HTMLCanvasElement) => getComputedStyle(c).imageRendering);
    // At Fit the hi frame is ~1.25× the device need, so the browser smooths.
    await expect.poll(rendering).toBe('auto');
    const fit = await canvasSize(page);
    expect(fit.w).toBeGreaterThanOrEqual(fit.cw * 1.5);

    await zoomIn(page, 5); // 1.4^5 ≈ 5.37 > fit zoom for 6000 px over 1155 CSS
    await expect.poll(() => pctNum(page)).toBeGreaterThan(100);
    await expect.poll(rendering).toBe('pixelated');
    const zoom = await canvasSize(page);
    expect(zoom.w, 'a window past native must not be upsampled').toBeLessThan(fit.w);

    // A synthetic checkerboard: at zoom the backing stays at the native window
    // resolution and CSS stretches it; smoothing would fill mid-luma.
    const dataUrl = await page.evaluate(() => {
      const c = document.createElement('canvas');
      c.width = 400;
      c.height = 300;
      const x = c.getContext('2d')!;
      const s = 40;
      for (let yy = 0; yy < 300; yy += s) {
        for (let xx = 0; xx < 400; xx += s) {
          x.fillStyle = (xx / s + yy / s) % 2 ? '#000' : '#fff';
          x.fillRect(xx, yy, s, s);
        }
      }
      return c.toDataURL('image/png');
    });
    const file = path.join(os.tmpdir(), `pes-checker-${Date.now()}.png`);
    fs.writeFileSync(file, Buffer.from(dataUrl.split(',')[1], 'base64'));
    // Not importFiles: it waits for a thumb count of 1, which is already true.
    await fileInput(page).setInputFiles([file]);
    await expect(thumbs(page)).toHaveCount(2);
    await thumbNamed(page, path.basename(file)).click();
    await clickTab(page, 'Color');
    await zoomIn(page);
    await expect.poll(rendering).toBe('pixelated');
    const mid = await canvas(page).evaluate((c: HTMLCanvasElement) => {
      const d = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
      let n = 0;
      let mids = 0;
      for (let i = 0; i < d.length; i += 4) {
        const l = 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
        if (l > 70 && l < 185) mids++;
        n++;
      }
      return { frac: mids / n, w: c.width, css: c.clientWidth };
    });
    expect(mid.frac, 'smoothed checkerboard has too many mid-luma pixels').toBeLessThan(0.2);
    expect(mid.w, 'checkerboard window must not be upsampled to the CSS box').toBeLessThan(mid.css);
    fs.rmSync(file, { force: true });

    expect(errors).toEqual([]);
  });

  test('export is byte-identical with the view zoomed; the crop is untouched', async ({ page }) => {
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await clickTab(page, 'Crop');
    await page.locator('.chip', { hasText: /^1:1$/ }).click();
    await clickTab(page, 'Color');
    await page.locator('label').filter({ hasText: /^Brightness/ }).first().locator('input[type=range]').evaluate((el: HTMLInputElement) => {
      el.value = '0.25';
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await page.waitForTimeout(1200);
    const recipeFit = (await itemRecipe(page)).text;

    await clickTab(page, 'Export');
    await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
    const download = async () => {
      const [dl] = await Promise.all([
        page.waitForEvent('download', { timeout: 60_000 }),
        page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
      ]);
      return fs.readFileSync((await dl.path())!);
    };
    const fitBytes = await download();

    await clickTab(page, 'Color');
    await zoomIn(page, 3);
    const b = await canvasBox(page);
    await middleDrag(page, b.width * 0.25);
    const recipeZoom = (await itemRecipe(page)).text;
    expect(recipeZoom, 'zoom/pan changed the recipe').toBe(recipeFit);

    await clickTab(page, 'Export');
    await expect.poll(() => pctNum(page)).toBeGreaterThan(140); // Export keeps the view
    const zoomBytes = await download();
    expect(zoomBytes.equals(fitBytes), 'export differs with the view zoomed').toBe(true);

    const rec = await itemRecipe(page);
    expect(rec.text, 'export changed the recipe').toBe(recipeFit);
    expect(rec.text, 'a 1:1 crop must be serialised').toContain('"crop"');
  });

  test('a plain drag pans the zoomed view on tabs without a drawing tool', async ({ page }) => {
    const raws = rawFixtures();
    test.skip(raws.length === 0, 'no RAW fixture in dl-raw');
    const raw = raws.find((p) => /synthetic/i.test(p)) || raws[0];
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [raw]);
    const leftDrag = async (dx: number) => {
      const b = await canvasBox(page);
      const [x, y] = [b.x + b.width / 2, b.y + b.height / 2];
      await page.mouse.move(x, y);
      await page.mouse.down();
      for (let i = 1; i <= 8; i++) {
        await page.mouse.move(x + (dx * i) / 8, y);
        await page.waitForTimeout(16);
      }
      await page.mouse.up();
      await page.waitForTimeout(250);
    };
    const wrap = page.locator('.canvas-wrap');

    // At Fit there is nothing to pan, and the photo shows no hand.
    await clickTab(page, 'Color');
    await expect(wrap).not.toHaveClass(/pannable/);
    await zoomIn(page); // 1.4x Fit
    await expect.poll(() => pctNum(page)).toBeCloseTo(140, 0);
    await expect(wrap).toHaveClass(/pannable/);

    // Drag right by 10% of the canvas on Color: the point under the centre
    // moves left by 0.1 / 1.4 in crop units. A heal dab reads it back.
    const b = await canvasBox(page);
    await leftDrag(b.width * 0.1);
    await clickTab(page, 'Heal');
    await expect(wrap, 'Heal keeps the drag for its brush').not.toHaveClass(/pannable/);
    await page.locator('.heal-panel .chip', { hasText: /^Smooth fill$/ }).click();
    await canvasClick(page, 0.5, 0.5);
    await waitHs(page, 1);
    let rec = await itemRecipe(page);
    expect(rec.hs[0].p[0][0]).toBeCloseTo(0.5 - 0.1 / 1.4, 2);
    expect(rec.hs[0].p[0][1]).toBeCloseTo(0.5, 2);

    // The same drag on Heal paints a stroke and leaves the view where it is.
    await leftDrag(b.width * 0.1);
    await waitHs(page, 2);
    await canvasClick(page, 0.5, 0.5);
    await waitHs(page, 3);
    rec = await itemRecipe(page);
    expect(rec.hs[1].p.length, 'the drag on Heal should be a stroke').toBeGreaterThan(1);
    expect(rec.hs[2].p[0][0], 'a drag on Heal must not pan').toBeCloseTo(0.5 - 0.1 / 1.4, 2);

    // The zoom buttons sit over the photo and still take their clicks.
    await clickTab(page, 'RAW');
    await page.locator('.zoom-cluster button[title="Fit (0)"]').click();
    await expect.poll(() => pctNum(page)).toBeCloseTo(100, 0);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  // QA B1: digits, +, - and Space are text in a text field, even with the
  // pointer resting on the photo.
  test('a text field keeps its keys while the pointer is over the canvas', async ({ page }) => {
    await openApp(page);
    await importFiles(page, [PNG_SMALL]);
    await clickTab(page, 'Layers');
    await page.locator('button.btn', { hasText: '＋ Text' }).click();
    const field = page.locator('input[type=text]').first();
    await field.click();
    await field.fill('');
    const b = await canvasBox(page);
    await page.mouse.move(b.x + b.width * 0.5, b.y + b.height * 0.5);
    await page.keyboard.type('Hi 10-1+2 ok', { delay: 20 });
    await expect(field).toHaveValue('Hi 10-1+2 ok');
    expect(await pctNum(page), 'typing in a text field zoomed the view').toBeLessThan(101);
  });

  /** A detailed random-block PNG: blur parity shows on it where bokeh hides it. */
  async function blocksPng(page: Page, w: number, h: number): Promise<string> {
    const dataUrl = await page.evaluate(([w, h]) => {
      const c = document.createElement('canvas');
      c.width = w;
      c.height = h;
      const x = c.getContext('2d')!;
      let seed = 12345;
      const rnd = () => ((seed = (seed * 1664525 + 1013904223) >>> 0) / 4294967296);
      for (let yy = 0; yy < h; yy += 8) {
        for (let xx = 0; xx < w; xx += 8) {
          x.fillStyle = `rgb(${(rnd() * 256) | 0},${(rnd() * 256) | 0},${(rnd() * 256) | 0})`;
          x.fillRect(xx, yy, 8, 8);
        }
      }
      return c.toDataURL('image/png');
    }, [w, h]);
    const file = path.join(os.tmpdir(), `pes-blocks-${w}x${h}-${Date.now()}.png`);
    fs.writeFileSync(file, Buffer.from(dataUrl.split(',')[1], 'base64'));
    return file;
  }

  // QA B2: at dpr 1 the canvas is about as wide as the 1024 px preview, and
  // the sharp-frame decision used to compare against the whole preview.
  test('a zoomed view sharpens on a dpr-1 screen', async ({ browser }) => {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
    const page = await ctx.newPage();
    await openApp(page);
    const file = await blocksPng(page, 2400, 1800);
    await importFiles(page, [file]);
    await clickTab(page, 'Color');
    await page.locator('.zoom-cluster .zoom-native').click();
    await expect.poll(() => pctNum(page)).toBe(100);
    // At 100% one image pixel is one CSS pixel: the backing must reach it.
    await expect
      .poll(async () => { const s = await canvasSize(page); return s.w / s.cw; }, { timeout: 20_000 })
      .toBeGreaterThan(0.95);
    fs.rmSync(file, { force: true });
    await ctx.close();
  });

  // 21 Nov: a RAW shows from a display proxy (half the sensor's size on a
  // 24 MP camera) until something needs its sensor pixels, and a zoomed view
  // never asked for them: "100%" was the proxy enlarged two times, so denoise
  // and sharpening could not be judged there. The synthetic DNG's proxy holds
  // every sensor pixel, so this needs a camera RAW.
  test('a RAW zoomed past its display proxy loads the sensor pixels', async ({ browser }) => {
    const raw = rawFixtures().find((p) => !/synthetic/i.test(p));
    test.skip(!raw, 'no camera RAW in dl-raw');
    test.setTimeout(240_000);
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
    const page = await ctx.newPage();
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [raw!]);
    await clickTab(page, 'RAW');
    await page.locator('.zoom-cluster .zoom-native').click();
    await expect.poll(() => pctNum(page)).toBe(100);
    // At 100% one sensor pixel is one CSS pixel: the backing must reach it.
    // The proxy alone gives half of that on a 24 MP camera.
    await expect
      .poll(async () => { const s = await canvasSize(page); return s.w / s.cw; }, { timeout: 180_000 })
      .toBeGreaterThan(0.95);
    // Back at Fit the view is whole again, and the zoom did not disturb it.
    await page.locator('.zoom-cluster .zoom-fit').click();
    await expect.poll(async () => (await canvasSize(page)).w, { timeout: 60_000 }).toBeGreaterThanOrEqual(1024);
    expect(errors).toEqual([]);
    await ctx.close();
  });

  // QA B3 (R7): with Blur on, the zoomed window's outer pixels must match the
  // export too. Without an apron the outer 2 px were 7.7 levels off.
  test('the edge of a zoomed, blurred view matches the export', async ({ page }) => {
    test.setTimeout(180_000);
    await openApp(page);
    const file = await blocksPng(page, 2400, 1800);
    await importFiles(page, [file]);
    await clickTab(page, 'Color');
    await setRange(page, /^Blur/, 60);
    await page.locator('.zoom-cluster .zoom-native').click();
    await expect.poll(() => pctNum(page)).toBe(100);
    await page.waitForTimeout(2500);
    const view = await canvas(page).evaluate((c: HTMLCanvasElement) => {
      const d = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data;
      let s = '';
      for (let i = 0; i < d.length; i += 0x8000) s += String.fromCharCode.apply(null, Array.from(d.subarray(i, i + 0x8000)) as number[]);
      return { w: c.width, h: c.height, b64: btoa(s) };
    });
    await clickTab(page, 'Export');
    await page.locator('label', { hasText: 'Format' }).locator('select').selectOption('png');
    const [dl] = await Promise.all([
      page.waitForEvent('download', { timeout: 120_000 }),
      page.locator('button.btn.primary', { hasText: 'Download this' }).click(),
    ]);
    const png = fs.readFileSync((await dl.path())!).toString('base64');
    const band = await page.evaluate(async ({ view, png }) => {
      const bin = atob(png);
      const u8 = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
      const bmp = await createImageBitmap(new Blob([u8], { type: 'image/png' }));
      const c = document.createElement('canvas');
      c.width = bmp.width;
      c.height = bmp.height;
      const x = c.getContext('2d')!;
      x.drawImage(bmp, 0, 0);
      const e = x.getImageData(0, 0, c.width, c.height).data;
      const vb = atob(view.b64);
      const v = (i: number) => vb.charCodeAt(i);
      const cx = Math.round((c.width - view.w) / 2);
      const cy = Math.round((c.height - view.h) / 2);
      // Mean |Δ| over a rectangle of the view, with the window at (ox, oy).
      const diff = (ox: number, oy: number, x0: number, y0: number, x1: number, y1: number) => {
        let s = 0, n = 0;
        for (let yy = y0; yy < y1; yy++) for (let xx = x0; xx < x1; xx++) {
          const a = (yy * view.w + xx) * 4, b = ((yy + oy) * c.width + xx + ox) * 4;
          for (let k = 0; k < 3; k++) { s += Math.abs(v(a + k) - e[b + k]); n++; }
        }
        return s / n;
      };
      // The centred window can sit a pixel either way: take the best fit.
      let best = { d: 1e9, ox: cx, oy: cy };
      for (let dy = -2; dy <= 2; dy++) for (let dx = -2; dx <= 2; dx++) {
        const d = diff(cx + dx, cy + dy, 100, 100, 260, 220);
        if (d < best.d) best = { d, ox: cx + dx, oy: cy + dy };
      }
      const strips = [
        diff(best.ox, best.oy, 0, 0, view.w, 2),
        diff(best.ox, best.oy, 0, view.h - 2, view.w, view.h),
        diff(best.ox, best.oy, 0, 0, 2, view.h),
        diff(best.ox, best.oy, view.w - 2, 0, view.w, view.h),
      ];
      return { interior: best.d, edge: Math.max(...strips), vw: view.w, ew: c.width };
    }, { view, png });
    expect(band.vw, 'the view must be a window of the export').toBeLessThan(band.ew);
    expect(band.interior, 'zoomed interior differs from the export').toBeLessThanOrEqual(0.5);
    expect(band.edge, 'outer 2 px of the zoomed view differ from the export').toBeLessThanOrEqual(1.5);
    fs.rmSync(file, { force: true });
  });
});
