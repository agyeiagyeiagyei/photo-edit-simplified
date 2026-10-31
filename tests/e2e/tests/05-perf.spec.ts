import { test, expect } from '@playwright/test';
import { openApp, importFiles, canvasSize, recordMetrics, trackErrors, JPEG_A } from './helpers';

test.describe('perf marks (docs/PERF.md)', () => {
  test('window.__pes.perf() records the documented app and worker marks', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    await openApp(page);

    const mount = await page.evaluate(() => (window as any).__pes.perf());
    expect(Array.isArray(mount)).toBe(true);
    expect(mount.some((e: any) => e.name === 'app.mount' && typeof e.dur_ms === 'number' && e.dur_ms >= 0)).toBe(true);

    await importFiles(page, [JPEG_A]);
    await expect.poll(async () => (await canvasSize(page)).w, { timeout: 20_000 }).toBeGreaterThan(1200);

    const marks = await page.evaluate(() => (window as any).__pes.perf());
    const names = marks.map((e: any) => e.name);
    for (const n of [
      'ingest.read', 'ingest.decode', 'ingest.preview', 'ingest.thumb', 'ingest.push',
      'preview.render', 'hi.request', 'hi.worker_reply', 'hi.blit',
      'worker.load', 'worker.render',
      'worker.render.geometry', 'worker.render.resample', 'worker.render.develop', 'worker.render.color',
      'worker.render.cache',
    ]) {
      expect(names, `missing perf mark ${n}`).toContain(n);
    }
    const preview = marks.find((e: any) => e.name === 'preview.render');
    expect(preview.detail).toMatch(/^\d+x\d+$/);
    expect(preview.dur_ms).toBeGreaterThanOrEqual(0);

    // Each span is a real DevTools measure, not just a ring entry.
    const measures = await page.evaluate(() => performance.getEntriesByType('measure').map((e) => e.name));
    expect(measures).toContain('preview.render');
    expect(measures).toContain('hi.blit');

    await page.evaluate(() => (window as any).__pes.reset());
    expect(await page.evaluate(() => (window as any).__pes.perf().length)).toBe(0);

    recordMetrics(testInfo, {
      perf_marks: names.length,
      perf_preview_render_ms: Math.round(preview.dur_ms * 10) / 10,
    });
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
