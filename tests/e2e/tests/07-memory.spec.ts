import { test, expect } from '@playwright/test';
import * as fs from 'fs';
import * as http from 'http';
import * as path from 'path';
import type { AddressInfo } from 'net';
import { trackErrors, fileInput, thumbs, recordMetrics, JPEG_A, JPEG_B } from './helpers';

// Baseline from the WS6-B reference run at ce8e8e2 (the double-copy build):
// 1086 MiB of JS heap + wasm after loading the same five 24 MP fixtures.
const BASELINE_MB = 1086;
const TARGET_RATIO = 0.55;

const DIST = process.env.PES_DIST || path.resolve(__dirname, '..', '..', '..', 'dist');

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.wasm': 'application/wasm',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.map': 'application/json',
  '.webmanifest': 'application/manifest+json',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8',
};

/**
 * Serve the built app with COOP/COEP: `measureUserAgentSpecificMemory` only
 * exists for cross-origin-isolated documents, and the regular test server
 * (a plain static server) cannot set those headers. The optional
 * `/photo-edit-simplified/` prefix keeps builds made with the deploy URL and
 * builds made with `--public-url /` both working.
 */
function serveDist(): Promise<{ url: string; close: () => Promise<void> }> {
  const server = http.createServer((req, res) => {
    let rel = decodeURIComponent(new URL(req.url || '/', 'http://x').pathname);
    rel = rel.replace(/^\/photo-edit-simplified(?=\/)/, '');
    if (rel === '/' || rel === '') rel = '/index.html';
    const file = path.join(DIST, rel);
    if (!file.startsWith(DIST + path.sep)) {
      res.writeHead(403).end();
      return;
    }
    fs.readFile(file, (err, data) => {
      if (err) {
        res.writeHead(404).end('not found');
        return;
      }
      res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
      res.setHeader('Cross-Origin-Embedder-Policy', 'credentialless');
      res.setHeader('Content-Type', MIME[path.extname(file)] || 'application/octet-stream');
      res.end(data);
    });
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address() as AddressInfo;
      resolve({
        url: `http://127.0.0.1:${port}/photo-edit-simplified/`,
        close: () => new Promise((done) => server.close(() => done())),
      });
    });
  });
}

// performance.measureUserAgentSpecificMemory() (recorded for reference) exists
// only in full Chromium; Playwright's default headless shell throws
// SecurityError for it even when the page is cross-origin isolated. Run this
// file on the full build in its new headless mode, which
// `npx playwright install chromium` installs next to the shell. A channel can
// only be set at file level.
test.use({ channel: 'chromium' });

test.describe('memory (issue #10)', () => {
  test('five 24 MP loads keep one full-res copy per item', async ({ page }, testInfo) => {
    test.setTimeout(300_000);
    const errors = trackErrors(page);
    const srv = await serveDist();
    try {
      await page.goto(srv.url);
      await expect(page.locator('header h1')).toHaveText('photo-edit-simplified');
      expect(await page.evaluate(() => (window as any).crossOriginIsolated), 'COOP/COEP not in effect').toBe(true);

      // Playwright caps in-memory payloads at 50 MB, so hardlink the 24 MP
      // fixtures under five names instead.
      const dir = fs.mkdtempSync(path.join(__dirname, '..', 'results', 'mem-'));
      const paths = [JPEG_A, JPEG_B, JPEG_A, JPEG_B, JPEG_A].map((src, i) => {
        const f = `${dir}/bird-${i + 1}.jpg`;
        fs.linkSync(src, f);
        return f;
      });
      await fileInput(page).setInputFiles(paths);
      await expect(thumbs(page)).toHaveCount(5, { timeout: 120_000 });
      await expect(page.locator('.overlay')).toHaveCount(0, { timeout: 120_000 });

      // Each selection materialises that item's full pixels in the worker;
      // worker.load_photo fires exactly once per item.
      const loads = () =>
        page.evaluate(() => ((window as any).__pes?.perf?.() || []).filter((e: any) => e.name === 'worker.load_photo').length);
      for (let i = 1; i < 5; i++) {
        const before = await loads();
        await page.locator('.filmstrip .thumb').nth(i).click();
        await expect.poll(loads, { timeout: 120_000 }).toBeGreaterThan(before);
      }
      await expect.poll(loads, { timeout: 120_000 }).toBe(5);

      // Let the last blit settle, then collect garbage so the measurement
      // reflects retained memory rather than dead temporaries.
      const cdp = await page.context().newCDPSession(page);
      await cdp.send('HeapProfiler.collectGarbage');
      await page.waitForTimeout(1500);

      const m = await page.evaluate(async () => {
        const pes = (window as any).__pes?.mem?.() || null;
        const heap = (performance as any).memory?.usedJSHeapSize ?? null;
        let ua: number | null = null;
        try {
          ua = (await (performance as any).measureUserAgentSpecificMemory()).bytes as number;
        } catch {
          // Not in every Chromium build (absent from the headless shell).
        }
        return { pes, heap, ua };
      });
      expect(m.pes?.pixel_worker, 'pixel worker memory telemetry missing').toBeGreaterThan(0);
      expect(m.heap, 'performance.memory missing').not.toBeNull();

      // Gate on the quantity the baseline was measured in: wasm linear memory
      // (high-water) of the app and each worker, plus the JS heap.
      // measureUserAgentSpecificMemory is recorded for reference only: it
      // attributes ~1 MB to the pixel worker while that worker's wasm memory
      // holds ~500 MB of pixels, so it cannot see the copies this test is about.
      const MiB = 1048576;
      const mb = (b: number | null | undefined) => Math.round((b || 0) / MiB);
      const wasmBytes = (m.pes.main || 0) + (m.pes.pixel_worker || 0) + (m.pes.raw_worker || 0) + (m.pes.raw_pool || 0);
      const totalMb = mb(wasmBytes + (m.heap as number));
      const ratioPct = Math.round((1000 * totalMb) / BASELINE_MB) / 10;
      recordMetrics(testInfo, {
        mem_total_mb: totalMb,
        mem_baseline_mb: BASELINE_MB,
        mem_ratio_pct: ratioPct,
        mem_pixel_worker_mb: mb(m.pes.pixel_worker),
        mem_main_mb: mb(m.pes.main),
        mem_raw_workers_mb: mb((m.pes.raw_worker || 0) + (m.pes.raw_pool || 0)),
        mem_js_heap_mb: mb(m.heap),
        mem_ua_specific_mb: m.ua === null ? -1 : mb(m.ua),
      });

      expect(totalMb, `peak ${totalMb} MB exceeds ${Math.floor(BASELINE_MB * TARGET_RATIO)} MB (55% of ${BASELINE_MB} MB baseline)`).toBeLessThanOrEqual(
        Math.floor(BASELINE_MB * TARGET_RATIO),
      );
      expect(errors, errors.join('\n')).toEqual([]);
      fs.rmSync(dir, { recursive: true, force: true });
    } finally {
      await srv.close();
    }
  });
});
