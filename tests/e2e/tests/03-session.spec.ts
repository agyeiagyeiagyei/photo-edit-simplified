import { test, expect } from '@playwright/test';
import { openApp, trackErrors, recordMetrics, importFiles, thumbs, canvasSize, JPEG_A, JPEG_B, PNG_SMALL } from './helpers';

test.describe('session persistence (IndexedDB)', () => {
  test('reload restores items and the editor, and measures restore time', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    await openApp(page);
    await importFiles(page, [JPEG_A, JPEG_B, PNG_SMALL]);
    await page.waitForTimeout(1500); // debounced persist (800 ms) + blob writes
    const t0 = Date.now();
    await page.reload();
    await expect(page.locator('header h1')).toBeVisible();
    await expect(thumbs(page)).toHaveCount(3, { timeout: 60_000 });
    const thumbsMs = Date.now() - t0;
    await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 60_000 });
    await expect.poll(async () => (await canvasSize(page)).w, { timeout: 30_000 }).toBeGreaterThan(0);
    const canvasMs = Date.now() - t0;
    recordMetrics(testInfo, { restore3_to_thumbs_ms: thumbsMs, restore3_to_canvas_ms: canvasMs });
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
