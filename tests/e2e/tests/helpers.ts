import { Page, expect, TestInfo } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';

export const FIX = path.resolve(__dirname, '..', 'fixtures');
export const JPEG_A = path.join(FIX, 'bird-a-24mp.jpg');
export const JPEG_B = path.join(FIX, 'bird-b-24mp.jpg');
export const PNG_SMALL = path.join(FIX, 'small.png');
export const RAW_DIR = path.resolve(__dirname, '..', '..', '..', 'dl-raw');
export function rawFixtures(): string[] {
  if (!fs.existsSync(RAW_DIR)) return [];
  return fs.readdirSync(RAW_DIR)
    .filter((n) => /\.(arw|dng|nef|cr2|cr3|raf|orf|rw2)$/i.test(n))
    .sort()
    .map((n) => path.join(RAW_DIR, n));
}

export type Metrics = Record<string, number | string | boolean>;
const METRICS_FILE = path.resolve(__dirname, '..', 'results', 'metrics.json');
export function recordMetrics(testInfo: TestInfo, m: Metrics) {
  fs.mkdirSync(path.dirname(METRICS_FILE), { recursive: true });
  let all: Record<string, Metrics> = {};
  try { all = JSON.parse(fs.readFileSync(METRICS_FILE, 'utf8')); } catch {}
  all[testInfo.title] = { ...(all[testInfo.title] || {}), ...m, target: process.env.PES_BASE_URL || 'local', at: new Date().toISOString() };
  fs.writeFileSync(METRICS_FILE, JSON.stringify(all, null, 2));
  for (const [k, v] of Object.entries(m)) testInfo.annotations.push({ type: 'metric', description: `${k}=${v}` });
}

/** Collect console errors + page errors for the test's lifetime. */
export function trackErrors(page: Page) {
  const errors: string[] = [];
  page.on('console', (msg) => { if (msg.type() === 'error') errors.push(`console: ${msg.text()}`); });
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
  return errors;
}

/** Wait for the app shell (Leptos mounted) and return load timings. */
export async function openApp(page: Page) {
  const t0 = Date.now();
  await page.goto('./');
  await expect(page.locator('header h1')).toHaveText('photo-edit-simplified');
  const shellMs = Date.now() - t0;
  const res = await page.evaluate(() => {
    const out: Record<string, number> = {};
    for (const e of performance.getEntriesByType('resource') as PerformanceResourceTiming[]) {
      const n = e.name.split('/').pop() || e.name;
      if (/\.wasm$/.test(n)) {
        out[`wasm_${n.includes('worker') ? 'worker' : 'main'}_transfer_bytes`] = e.transferSize;
        out[`wasm_${n.includes('worker') ? 'worker' : 'main'}_decoded_bytes`] = e.decodedBodySize;
        out[`wasm_${n.includes('worker') ? 'worker' : 'main'}_fetch_ms`] = Math.round(e.duration);
      }
    }
    const nav = performance.getEntriesByType('navigation')[0] as PerformanceNavigationTiming | undefined;
    if (nav) { out.dom_content_loaded_ms = Math.round(nav.domContentLoadedEventEnd); out.load_event_ms = Math.round(nav.loadEventEnd); }
    return out;
  });
  return { shellMs, ...res };
}

export const fileInput = (page: Page) => page.locator('input[type=file]').first();
export const thumbs = (page: Page) => page.locator('.filmstrip .thumb');
export const canvas = (page: Page) => page.locator('.canvas-wrap canvas').first();

export async function importFiles(page: Page, files: string[]) {
  const t0 = Date.now();
  await fileInput(page).setInputFiles(files);
  await expect(thumbs(page)).toHaveCount(files.length, { timeout: 90_000 });
  const thumbsMs = Date.now() - t0;
  await expect(canvas(page)).toBeVisible();
  await expect.poll(() => canvas(page).evaluate((c: HTMLCanvasElement) => c.width), { timeout: 60_000 }).toBeGreaterThan(0);
  await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 90_000 });
  const canvasMs = Date.now() - t0;
  return { thumbsMs, canvasMs };
}

export async function canvasSize(page: Page) {
  return canvas(page).evaluate((c: HTMLCanvasElement) => ({ w: c.width, h: c.height, cw: c.clientWidth, ch: c.clientHeight }));
}

/** Mean RGB of the canvas centre region (reads back the actual backing store). */
export async function canvasMean(page: Page) {
  return canvas(page).evaluate((c: HTMLCanvasElement) => {
    const ctx = c.getContext('2d')!;
    const w = Math.max(1, Math.floor(c.width * 0.2)), h = Math.max(1, Math.floor(c.height * 0.2));
    const d = ctx.getImageData(Math.floor(c.width * 0.4), Math.floor(c.height * 0.4), w, h).data;
    let r = 0, g = 0, b = 0; const n = d.length / 4;
    for (let i = 0; i < d.length; i += 4) { r += d[i]; g += d[i + 1]; b += d[i + 2]; }
    return { r: r / n, g: g / n, b: b / n, luma: (0.299 * r + 0.587 * g + 0.114 * b) / n };
  });
}

export async function clickTab(page: Page, label: string) {
  await page.locator('.tabs button, .tabbar button, button.tab').filter({ hasText: new RegExp(`^${label}$`) }).first().click();
}

/** Set a range input by its visible label text, dispatching input+change like a real drag. */
export async function setRange(page: Page, labelRe: RegExp, value: number) {
  const label = page.locator('label').filter({ hasText: labelRe }).first();
  const input = label.locator('input[type=range]').first();
  await expect(input).toBeVisible();
  await input.evaluate((el: HTMLInputElement, v) => {
    el.value = String(v);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }, value);
}

export async function imageDims(page: Page, bytes: Buffer, mime: string) {
  return page.evaluate(async ({ b64, mime }) => {
    const bin = atob(b64); const u8 = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
    const bmp = await createImageBitmap(new Blob([u8], { type: mime }));
    return { w: bmp.width, h: bmp.height };
  }, { b64: bytes.toString('base64'), mime });
}

export function zipEntryCount(buf: Buffer): number {
  // Count local file headers (PK\x03\x04).
  let n = 0;
  for (let i = 0; i + 4 <= buf.length; i++) {
    if (buf[i] === 0x50 && buf[i + 1] === 0x4b && buf[i + 2] === 0x03 && buf[i + 3] === 0x04) n++;
  }
  return n;
}

/** Walk JPEG marker segments up to SOS. */
export function jpegSegments(buf: Buffer): { marker: number; data: Buffer }[] {
  const out: { marker: number; data: Buffer }[] = [];
  let i = 2;
  while (i + 4 <= buf.length && buf[i] === 0xff) {
    const marker = buf[i + 1]; const len = buf.readUInt16BE(i + 2);
    out.push({ marker, data: buf.subarray(i + 4, i + 2 + len) });
    if (marker === 0xda) break;
    i += 2 + len;
  }
  return out;
}

/** Frame-granular wait for the canvas backing width to exceed `min` (rAF polling, not expect.poll intervals). */
export async function waitCanvasWidthAbove(page: Page, min: number, timeout = 20_000) {
  await page.waitForFunction((m) => { const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement | null; return !!c && c.width > m; }, min, { timeout, polling: 'raf' });
}

/** Perf marks from the app (WS1+ builds); {} on older builds. Last occurrence of each name. */
export async function perfMarks(page: Page, re = /^(hi\.|worker\.(load|render)|preview\.render|ingest\.|export\.)/) {
  return page.evaluate((src) => {
    const w = window as any; if (!w.__pes || !w.__pes.perf) return {} as Record<string, number>;
    const r = new RegExp(src); const out: Record<string, number> = {};
    for (const e of w.__pes.perf()) if (r.test(e.name)) out['mark_' + e.name.replace(/\./g, '_')] = Math.round(e.dur_ms * 10) / 10;
    return out;
  }, re.source);
}
