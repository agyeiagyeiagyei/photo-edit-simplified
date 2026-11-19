// One-off QA probe for issue #46 R5: cost of a pixel-worker window render
// miss (zoom window, 26 MP RAW, fine rotation, 9 heal strokes) against the
// 500 ms budget. Run with the local dist server on 8787:
//   node qa-window-miss.mjs
import { chromium } from 'playwright';
import fs from 'fs';
import os from 'os';
import path from 'path';

const BASE = process.env.PES_BASE_URL || 'http://127.0.0.1:8787/?gpu=0';
const RAW = path.resolve(process.env.RAW_FILE || 'dl-raw/DSC05290.ARW');

const clickTab = (page, label) =>
  page.locator('.tabs button, .tabbar button, button.tab')
    .filter({ hasText: new RegExp(`^${label}$`) }).first().click();

const setRange = async (page, labelRe, value) => {
  const input = page.locator('label').filter({ hasText: labelRe }).first()
    .locator('input[type=range]').first();
  await input.evaluate((el, v) => {
    el.value = String(v);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }, value);
};

const canvas = (page) => page.locator('.canvas-wrap canvas').first();

const itemRec = (page) => page.evaluate(async () => {
  const db = await new Promise((res, rej) => {
    const rq = indexedDB.open('pes-session');
    rq.onsuccess = () => res(rq.result);
    rq.onerror = () => rej(rq.error);
  });
  try {
    const vals = await new Promise((res) => {
      const rq = db.transaction('items', 'readonly').objectStore('items').getAll();
      rq.onsuccess = () => res(rq.result ?? []);
      rq.onerror = () => res([]);
    });
    vals.sort((a, b) => (a.ord ?? 0) - (b.ord ?? 0));
    const rec = vals[0];
    if (!rec) return null;
    let recipe = null;
    try { recipe = rec.recipe ? JSON.parse(rec.recipe) : null; } catch {}
    return {
      raw: rec.raw === true,
      fine: recipe?.fine_angle ?? recipe?.fa ?? null,
      hs: Array.isArray(recipe?.hs) ? recipe.hs.length : 0,
    };
  } finally { db.close(); }
});

const waitMark = async (page, name, since, timeout = 45_000) => {
  const t0 = Date.now();
  while (Date.now() - t0 < timeout) {
    const ok = await page.evaluate(({ name, since }) =>
      (window.__pes?.perf?.() || []).some((e) => e.name === name && e.at >= since), { name, since });
    if (ok) return true;
    await page.waitForTimeout(100);
  }
  return false;
};

const marksSince = (page, since) => page.evaluate((since) => {
  const p = (window.__pes?.perf?.() || []).filter((e) => e.at >= since);
  const n = (re) => p.filter((e) => re.test(e.name))
    .map((e) => ({ name: e.name, ms: Math.round(e.dur_ms * 10) / 10, at: Math.round(e.at), detail: e.detail || undefined }));
  return {
    renders: n(/^worker\.render_window$/),
    stages: n(/^worker\.render_window\./),
    hiReply: n(/^hi\.worker_reply$/),
    hiRequest: n(/^hi\.request$/),
  };
}, since);

const viewState = (page) => page.evaluate(() => {
  const c = document.querySelector('.canvas-wrap canvas');
  const pct = document.querySelector('.zoom-pct');
  return { canvas: c ? { w: c.width, h: c.height, cw: c.clientWidth, ch: c.clientHeight } : null, pct: pct?.textContent ?? null };
});

const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'pes-qa-'));
const ctx = await chromium.launchPersistentContext(profile, {
  channel: 'chromium',
  headless: true,
  viewport: { width: 1600, height: 1000 },
  deviceScaleFactor: 2,
  args: ['--disable-dev-shm-usage'],
});
const page = ctx.pages()[0] || (await ctx.newPage());
const errors = [];
page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
page.on('console', (m) => { if (m.type() === 'error') errors.push(`console: ${m.text()}`); });

const out = { raw: path.basename(RAW) };
try {
  await page.goto(BASE);
  await page.locator('header h1').waitFor({ timeout: 30_000 });
  await page.setInputFiles('input[type=file]', RAW);
  await page.waitForFunction(() => document.querySelectorAll('.filmstrip .thumb').length >= 1, null, { timeout: 120_000 });
  await page.waitForFunction(() => {
    const c = document.querySelector('.canvas-wrap canvas');
    return !!c && c.width > 2000 && !document.querySelector('.overlay') && !document.querySelector('.developing-badge');
  }, null, { timeout: 120_000 });
  await page.waitForTimeout(1500);

  // Fine rotation: the worst case for the window miss (whole-frame rotate).
  await clickTab(page, 'Rotate');
  await setRange(page, /Straighten/, 3.2);
  await page.waitForTimeout(2500);
  out.straighten = await page.locator('label').filter({ hasText: /Straighten/ }).first()
    .locator('input[type=range]').first().inputValue();
  out.afterRotate = await viewState(page);

  // Nine heal dabs across the frame.
  await clickTab(page, 'Heal');
  await page.locator('.heal-panel .chip', { hasText: /^Smooth fill$/ }).click();
  const bb = await canvas(page).boundingBox();
  const pts = [[0.25, 0.25], [0.5, 0.25], [0.75, 0.25], [0.25, 0.5], [0.5, 0.5], [0.75, 0.5], [0.25, 0.75], [0.5, 0.75], [0.75, 0.75]];
  for (const [nx, ny] of pts) {
    await canvas(page).click({ position: { x: bb.width * nx, y: bb.height * ny } });
    await page.waitForTimeout(250);
  }
  await page.waitForTimeout(2000);
  out.rec = await itemRec(page);

  // Zoom 1.4^3 above Fit on a zoomable tab.
  await clickTab(page, 'Color');
  await page.mouse.click(bb.x + bb.width / 2, bb.y + bb.height / 2);
  const tZoom = await page.evaluate(() => performance.now());
  for (let i = 0; i < 3; i++) {
    await page.locator('.zoom-cluster button[title="Zoom in (+)"]').click();
    await page.waitForTimeout(350);
  }
  out.zoomMark = await waitMark(page, 'worker.render_window', tZoom);
  await page.waitForTimeout(1200);
  out.zoomView = await viewState(page);
  out.zoomMarks = await marksSince(page, tZoom);

  // Pan: a new crop is a cache miss whatever was cached before.
  const tPan = await page.evaluate(() => performance.now());
  await page.mouse.move(bb.x + bb.width / 2, bb.y + bb.height / 2);
  await page.mouse.down({ button: 'middle' });
  for (let i = 1; i <= 10; i++) {
    await page.mouse.move(bb.x + bb.width / 2 + i * 18, bb.y + bb.height / 2 + i * 10);
    await page.waitForTimeout(25);
  }
  await page.mouse.up({ button: 'middle' });
  out.panMark = await waitMark(page, 'worker.render_window', tPan);
  await page.waitForTimeout(1200);
  out.panView = await viewState(page);
  out.panMarks = await marksSince(page, tPan);

  // Colour tick at the same crop/target: the window cache must hit.
  const tCache = await page.evaluate(() => performance.now());
  await setRange(page, /^Brightness/, 0.25);
  out.cacheMark = await waitMark(page, 'worker.render_window', tCache);
  await page.waitForTimeout(1200);
  out.cacheMarks = await marksSince(page, tCache);

  out.errors = errors;
} catch (e) {
  out.fatal = String(e && e.stack || e);
  out.errors = errors;
} finally {
  await ctx.close().catch(() => {});
  fs.rmSync(profile, { recursive: true, force: true });
}
console.log(JSON.stringify(out, null, 2));
