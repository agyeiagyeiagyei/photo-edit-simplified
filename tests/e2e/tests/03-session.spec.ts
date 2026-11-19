import { test, expect } from '@playwright/test';
import * as fs from 'fs';
import * as path from 'path';
import { openApp, trackErrors, recordMetrics, importFiles, thumbs, canvasSize, installRestoreProbe, readRestoreProbe, fileInput, clickTab, setRange, rawFixtures, JPEG_A, JPEG_B, PNG_SMALL } from './helpers';

test.describe('session persistence (IndexedDB)', () => {
  test('reload restores items and the editor, and measures restore time', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [JPEG_A, JPEG_B, PNG_SMALL]);
    await page.waitForTimeout(1500); // debounced persist (800 ms) + blob writes
    await installRestoreProbe(page, 3);
    const t0 = Date.now();
    await page.reload();
    await expect(page.locator('header h1')).toBeVisible();
    await expect(thumbs(page)).toHaveCount(3, { timeout: 60_000 });
    const wallMs = Date.now() - t0;
    const probe = await readRestoreProbe(page);
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await expect.poll(async () => (await canvasSize(page)).w, { timeout: 30_000 }).toBeGreaterThan(0);
    const canvasMs = Date.now() - t0;
    recordMetrics(testInfo, {
      restore3_to_thumbs_ms: probe.thumbsMs,
      restore3_to_thumbs_wall_ms: wallMs,
      restore3_overlay_gone_ms: probe.overlayGoneMs,
      restore3_to_canvas_ms: canvasMs,
    });
    expect(probe.thumbsMs).toBeGreaterThan(0);
    expect(probe.thumbsMs).toBeLessThanOrEqual(300);
    expect(probe.overlayGoneMs).toBeLessThanOrEqual(300);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  // The preview pump bumped photo.gen whenever a decoded preview landed,
  // which happens on every restore. Persistence reads gen > 0 on a RAW as
  // "destructively healed", found no worker frame to bake, and skipped the
  // item record, so every edit to a RAW after a reload was lost at the next
  // reload. JPEGs were unaffected.
  test('edits to a RAW after a reload survive the next reload', async ({ page }) => {
    const raws = rawFixtures();
    test.skip(raws.length === 0, 'no RAW fixture in dl-raw; run gen_dng.py to create one');
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    const slider = (re: RegExp) =>
      page.locator('label.slider').filter({ hasText: re }).first().locator('input[type=range]');
    const ready = async () => {
      await expect(thumbs(page)).toHaveCount(1, { timeout: 120_000 });
      await expect(page.locator('.developing-badge')).toHaveCount(0, { timeout: 120_000 });
      await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
      // The restored preview decode is what bumped gen; let it land.
      await page.waitForTimeout(3000);
    };
    await openApp(page);
    await importFiles(page, [raws[0]]);
    await ready();
    await clickTab(page, 'Color');
    await setRange(page, /^Brightness/, 0.2);
    await page.waitForTimeout(4000); // debounced persist + the RAW's blob write

    await page.reload();
    await ready();
    await clickTab(page, 'Color');
    await expect(slider(/^Brightness/)).toHaveValue('0.2');
    await setRange(page, /^Contrast/, 0.3);
    await page.waitForTimeout(4000);

    await page.reload();
    await ready();
    await clickTab(page, 'Color');
    await expect(slider(/^Contrast/), 'an edit made after a reload was not saved').toHaveValue('0.3');
    await expect(slider(/^Brightness/)).toHaveValue('0.2');
    await expect(page.locator('.tabs button', { hasText: /^RAW$/ })).toHaveCount(1);
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('reload restores a 50-item session from thumbnails', async ({ page }, testInfo) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    await openApp(page);
    // Playwright caps in-memory file payloads at 50 MB; hardlink the two
    // fixtures under 50 distinct names instead.
    const dir = fs.mkdtempSync(path.join(__dirname, '..', 'results', 'in-50-'));
    const pad = (i: number) => String(i + 1).padStart(2, '0');
    const paths = [
      ...Array.from({ length: 25 }, (_, i) => {
        const f = `${dir}/bird-a-${pad(i)}.jpg`;
        fs.linkSync(JPEG_A, f);
        return f;
      }),
      ...Array.from({ length: 25 }, (_, i) => {
        const f = `${dir}/bird-b-${pad(i)}.jpg`;
        fs.linkSync(JPEG_B, f);
        return f;
      }),
    ];
    await fileInput(page).setInputFiles(paths);
    await expect(thumbs(page)).toHaveCount(50, { timeout: 240_000 });
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 120_000 });
    // The debounced persist writes records one at a time; wait for a
    // completed pass that saw all 50 items rather than a fixed delay.
    await page.waitForFunction(() => {
      const p = (window as any).__pes?.perf?.() || [];
      return p.some((e: any) => e.name === 'session.persist' && e.detail === '50 items');
    }, undefined, { timeout: 120_000, polling: 250 });

    await installRestoreProbe(page, 50);
    const t0 = Date.now();
    await page.reload();
    await expect(thumbs(page)).toHaveCount(50, { timeout: 60_000 });
    const wallMs = Date.now() - t0;
    const probe = await readRestoreProbe(page);
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 30_000 });
    await expect.poll(async () => (await canvasSize(page)).w, { timeout: 30_000 }).toBeGreaterThan(0);
    const canvasMs = Date.now() - t0;
    recordMetrics(testInfo, {
      restore50_to_thumbs_ms: probe.thumbsMs,
      restore50_thumbs_wall_ms: wallMs,
      restore50_overlay_gone_ms: probe.overlayGoneMs,
      restore50_to_canvas_ms: canvasMs,
    });
    expect(probe.thumbsMs).toBeGreaterThan(0);
    expect(probe.thumbsMs).toBeLessThanOrEqual(1000);
    expect(probe.overlayGoneMs).toBeLessThanOrEqual(1000);
    expect(errors, errors.join('\n')).toEqual([]);
    // File blobs are backed by these paths until the session is done with them.
    fs.rmSync(dir, { recursive: true, force: true });
  });
});
