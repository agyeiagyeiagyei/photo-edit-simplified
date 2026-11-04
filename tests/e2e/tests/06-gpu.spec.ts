import { test, expect, Page } from '@playwright/test';
import * as path from 'path';
import {
  importFiles,
  clickTab,
  setRange,
  recordMetrics,
  trackErrors,
  waitCanvasWidthAbove,
  rawFixtures,
  JPEG_A,
} from './helpers';

const RAWS = rawFixtures();

// ?gpu=1 forces the WebGL2 path even on software renderers (test env),
// ?gpu=0 forces the CPU pipeline. Auto (no query) is the shipping default.
async function openMode(page: Page, mode: '0' | '1') {
  await page.goto(`./?gpu=${mode}`);
  await expect(page.locator('header h1')).toHaveText('photo-edit-simplified');
}

type Mark = { name: string; dur_ms: number; detail?: string };
const marks = (page: Page): Promise<Mark[]> =>
  page.evaluate(() => (window as any).__pes.perf() as any[]);
const markNames = async (page: Page) => (await marks(page)).map((m) => m.name);

async function webglRenderer(page: Page): Promise<string> {
  return page.evaluate(() => {
    const c = document.createElement('canvas');
    const gl = c.getContext('webgl2') as WebGL2RenderingContext | null;
    if (!gl) return 'none';
    const ext = gl.getExtension('WEBGL_debug_renderer_info');
    const name = ext
      ? (gl.getParameter((ext as any).UNMASKED_RENDERER_WEBGL) as string)
      : (gl.getParameter(gl.RENDERER) as string);
    return name || '';
  });
}

const SOFTWARE_GL = /swiftshader|llvmpipe|swangle|software|basic render driver/i;

async function waitMark(page: Page, name: string, min = 1) {
  await expect.poll(async () => (await markNames(page)).filter((n) => n === name).length, {
    timeout: 30_000,
    intervals: [100, 200, 400],
  }).toBeGreaterThanOrEqual(min);
}

/**
 * Wait until the developed RAW proxy has landed (#16). `proxy.raw_develop` is
 * recorded when the worker returns; `land_proxy` then installs the develop.
 * What the canvas shows next depends on the path: the CPU path seeds or
 * re-renders its hi-res frame, while the GPU path keeps drawing the previous
 * (camera-JPEG) base until the worker's base for the new source arrives
 * (stale-while-revalidate). Callers therefore follow this with a long
 * stability window rather than a single repeated sample.
 */
async function waitDevelopLanded(page: Page) {
  await expect.poll(async () => {
    const names = await markNames(page);
    const dev = names.lastIndexOf('proxy.raw_develop');
    if (dev < 0) return 'no develop yet';
    return names.indexOf('proxy.land', dev) > dev ? 'landed' : 'develop not landed';
  }, { timeout: 30_000, intervals: [100, 200, 400] }).toBe('landed');
}

/** Downscaled RGBA of the visible canvas, base64-encoded. */
async function downscaled(page: Page, width = 512) {
  return page.evaluate((W) => {
    const src = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement;
    const h = Math.round((W * src.height) / src.width);
    const c = document.createElement('canvas');
    c.width = W;
    c.height = h;
    const ctx = c.getContext('2d')!;
    ctx.drawImage(src, 0, 0, W, h);
    const d = ctx.getImageData(0, 0, W, h).data;
    let s = '';
    for (let i = 0; i < d.length; i += 0x8000) {
      s += String.fromCharCode.apply(null, Array.from(d.subarray(i, i + 0x8000)) as number[]);
    }
    return { w: W, h, b64: btoa(s), srcW: src.width, srcH: src.height };
  }, width);
}

function maxDelta(a: Buffer, b: Buffer) {
  let max = 0;
  for (let i = 0; i < a.length; i += 4) {
    for (let c = 0; c < 3; c++) max = Math.max(max, Math.abs(a[i + c] - b[i + c]));
  }
  return max;
}

function psnr(a: Buffer, b: Buffer) {
  let se = 0;
  let n = 0;
  for (let i = 0; i < a.length; i += 4) {
    for (let c = 0; c < 3; c++) {
      const d = a[i + c] - b[i + c];
      se += d * d;
      n++;
    }
  }
  const mse = se / n;
  return mse === 0 ? 99 : 10 * Math.log10((255 * 255) / mse);
}

/** Wait until the sharp canvas (w>1200) has been unchanged across two samples. */
/**
 * Wait for a sharp canvas whose content has not changed for `stableMs`
 * (sampled every 250 ms).
 */
async function waitStableSharpCanvas(page: Page, timeout = 90_000, stableMs = 250) {
  const sample = () => page.evaluate(() => {
    const c = document.querySelector('.canvas-wrap canvas') as HTMLCanvasElement;
    if (!c || c.width <= 1200) return 'soft';
    const s = document.createElement('canvas');
    s.width = 96;
    s.height = Math.max(1, Math.round((96 * c.height) / c.width));
    const x = s.getContext('2d')!;
    x.drawImage(c, 0, 0, s.width, s.height);
    const d = x.getImageData(0, 0, s.width, s.height).data;
    let h = 2166136261 >>> 0;
    for (let i = 0; i < d.length; i += 16) {
      h ^= d[i];
      h = Math.imul(h, 16777619) >>> 0;
    }
    return String(h);
  });
  const deadline = Date.now() + timeout;
  let prev = '';
  let since = 0;
  while (Date.now() < deadline) {
    const cur = await sample();
    const now = Date.now();
    if (cur === 'soft' || cur !== prev) {
      prev = cur;
      since = now;
    } else if (now - since >= stableMs) {
      return;
    }
    await page.waitForTimeout(250);
  }
  throw new Error('sharp canvas never stabilised');
}

async function applyColorEdit(page: Page) {
  await clickTab(page, 'Color');
  await setRange(page, /^Brightness/, 0.25);
  await setRange(page, /^Contrast/, 0.15);
  await setRange(page, /^Saturation/, -0.2);
  await setRange(page, /^Warmth/, 0.1);
  await setRange(page, /^Gamma/, 1.2);
  await setRange(page, /^Black point/, 6);
}

async function applyRawDevelop(page: Page) {
  await setRange(page, /^Temperature/, 6500);
  await setRange(page, /^Exposure/, 0.35);
  await setRange(page, /^Highlights/, 40);
  await setRange(page, /^Shadows/, 30);
}

/** Wait for a new visible frame on `name`, then for the canvas to settle. */
async function waitRenderTick(page: Page, name: string, before: number, stableMs = 250) {
  await expect.poll(async () => (await markNames(page)).filter((n) => n === name).length, {
    timeout: 60_000,
    intervals: [100, 200, 400],
  }).toBeGreaterThan(before);
  await waitStableSharpCanvas(page, 90_000, stableMs);
}

test.describe('WebGL2 parametric preview (docs/GPU.md)', () => {
  // qa-regression #13: RAW items on the GPU path were 29-32 dB against the
  // CPU/export pipeline because the shader always ran its linear-light tone
  // round trip, even when the develop stage was identity. Untouched RAW must
  // now pass through bit-exactly; a develop slider hands the frame to the
  // neutral CPU pipeline (WS5), which this test pins structurally and by
  // parity.
  test('RAW preview matches the CPU pipeline within 45 dB', async ({ browser }, testInfo) => {
    test.skip(RAWS.length === 0, 'no RAW fixture in dl-raw; run gen_dng.py to create one');
    const raw = RAWS[0];
    // One context at a time: the two full-res pipelines together crash the
    // renderer in the 64 MB-/dev/shm container. A fresh context per mode also
    // keeps both pages from restoring the persisted developed proxy (a JPEG),
    // so both develop from the same raw and the bases are comparable.
    // The wide viewport keeps the hi target at the source cap on both paths:
    // at a narrower size a pre-layout first pass can size the CPU frame from
    // the viewport fallback while the GPU base lands at the measured display
    // target, and the capture then compares two backing resolutions.
    const contextOpts = {
      viewport: { width: 1920, height: 1080 },
      deviceScaleFactor: 2,
      baseURL: process.env.PES_BASE_URL || 'http://127.0.0.1:8787/',
    };
    const gpuRenders = async (page: Page) =>
      (await markNames(page)).filter((n) => n === 'gpu.render').length;
    const capture = async (mode: '0' | '1') => {
      const ctx = await browser.newContext(contextOpts);
      try {
        const page = await ctx.newPage();
        const errors = trackErrors(page);
        await openMode(page, mode);
        await importFiles(page, [raw]);
        await waitCanvasWidthAbove(page, 1200);
        await waitDevelopLanded(page);
        if (mode === '1') {
          // The GPU keeps the camera-JPEG base on screen until the developed
          // proxy lands (stale-while-revalidate). Wait for that re-upload so
          // parity compares the same source as the CPU capture.
          await expect.poll(async () => {
            const names = (await marks(page)).map((m) => m.name);
            const develop = names.lastIndexOf('proxy.raw_develop');
            return develop >= 0 && names.slice(develop).includes('gpu.base');
          }, { timeout: 60_000 }).toBe(true);
        }
        // The CPU path likewise keeps its previous hi-res frame until the
        // render for the new source lands, which can queue behind a ~200 ms
        // proxy encode on the same worker. A frame unchanged for 1.5 s after
        // the land is the settled one; a wrong image that long is a bug.
        await waitStableSharpCanvas(page, 90_000, 1500);
        if (mode === '1') {
          // The untouched RAW is the GPU path's own case and must stay there.
          await expect.poll(() => gpuRenders(page), { timeout: 30_000 }).toBeGreaterThan(0);
        }
        const out = { errors, frames: {} as Record<string, { w: number; h: number; b64: string; srcW: number; srcH: number }> };
        out.frames.untouched = await downscaled(page, 512);

        // Let the tab switch (which may itself redraw) land before taking the
        // baseline, so the tick we wait for is the sliders' own.
        await clickTab(page, 'Color');
        await page.waitForTimeout(250);
        await waitStableSharpCanvas(page);
        // A non-identity RAW develop stays on the GPU through a developed base
        // (docs/GPU.md): the worker renders the neutral develop, camera tone and
        // detail into the base, and the shader applies only colour. Each
        // develop slider requests a new base, so wait for the last one to land
        // and the frame to hold still; a colour tick is a GPU redraw.
        const gpuBefore = mode === '1' ? await gpuRenders(page) : 0;
        const developTick = mode === '1' ? 'gpu.base' : 'hi.blit';
        const colourTick = mode === '1' ? 'gpu.render' : 'hi.blit';
        let before = (await markNames(page)).filter((n) => n === developTick).length;
        await applyRawDevelop(page);
        await waitRenderTick(page, developTick, before, 1500);
        out.frames.develop = await downscaled(page, 512);

        before = (await markNames(page)).filter((n) => n === colourTick).length;
        await setRange(page, /^Saturation/, 0.4);
        await waitRenderTick(page, colourTick, before, 1500);
        out.frames.develop_saturation = await downscaled(page, 512);
        if (mode === '1') {
          expect(await gpuRenders(page), 'GPU did not draw the developed base').toBeGreaterThan(gpuBefore);
        }
        return out;
      } finally {
        await ctx.close();
      }
    };

    const gpu = await capture('1');
    const cpu = await capture('0');

    for (const name of ['untouched', 'develop', 'develop_saturation']) {
      const g = gpu.frames[name];
      const c = cpu.frames[name];
      expect(c.w).toBe(g.w);
      expect(c.h).toBe(g.h);
      const db = psnr(Buffer.from(g.b64, 'base64'), Buffer.from(c.b64, 'base64'));
      const max = maxDelta(Buffer.from(g.b64, 'base64'), Buffer.from(c.b64, 'base64'));
      const sameRes = g.srcW === c.srcW && g.srcH === c.srcH;
      recordMetrics(testInfo, {
        raw_fixture: path.basename(raw),
        raw_canvas: `${g.w}x${g.h}`,
        [`raw_parity_${name}_canvas_gpu`]: `${g.srcW}x${g.srcH}`,
        [`raw_parity_${name}_canvas_cpu`]: `${c.srcW}x${c.srcH}`,
        [`raw_parity_${name}_psnr_db`]: Math.round(db * 100) / 100,
        [`raw_parity_${name}_max_delta`]: max,
      });
      expect(db, `${name} parity PSNR ${db.toFixed(2)} dB < 45 dB`).toBeGreaterThanOrEqual(45);
      // Untouched must be bit-exact when both modes drew at the same backing
      // size (the owner's ARW: 2888 px in both). When they settle at different
      // sizes (the synthetic DNG: the CPU path keeps its 2600 px proxy, the GPU
      // path draws a base resampled to the display), the 512 px comparison sees
      // resampling differences at edges (0.2 % of pixels > 16 levels, mean
      // difference ~0); the PSNR bar above still applies.
      if (name === 'untouched' && sameRes) {
        expect(max, 'untouched RAW preview moved pixels').toBeLessThanOrEqual(1);
      }
    }

    const gpuFrame = (n: string) => Buffer.from(gpu.frames[n].b64, 'base64');
    const staged = maxDelta(gpuFrame('untouched'), gpuFrame('develop'));
    const saturated = maxDelta(gpuFrame('develop'), gpuFrame('develop_saturation'));
    recordMetrics(testInfo, {
      raw_parity_develop_delta_vs_untouched: staged,
      raw_parity_saturation_delta: saturated,
    });
    expect(staged, 'RAW develop sliders did not change the preview').toBeGreaterThan(5);
    expect(saturated, 'Saturation did not change the preview').toBeGreaterThan(2);

    expect(gpu.errors, gpu.errors.join('\n')).toEqual([]);
    expect(cpu.errors, cpu.errors.join('\n')).toEqual([]);
  });

  test('gpu render matches the CPU pipeline within 45 dB', async ({ page, browser }, testInfo) => {
    const gpuPage = page;
    const gpuErrors = trackErrors(gpuPage);
    await openMode(gpuPage, '1');
    await importFiles(gpuPage, [JPEG_A]);
    await waitCanvasWidthAbove(gpuPage, 1200);
    await waitMark(gpuPage, 'worker.render_base');
    await waitMark(gpuPage, 'gpu.render');
    await applyColorEdit(gpuPage);
    await waitStableSharpCanvas(gpuPage);

    // A fresh context keeps its IndexedDB empty: in the shared context the
    // CPU page can restore the GPU page's debounced session and start with a
    // second filmstrip item.
    const cpuCtx = await browser.newContext({
      viewport: { width: 1600, height: 1000 },
      deviceScaleFactor: 2,
      baseURL: process.env.PES_BASE_URL || 'http://127.0.0.1:8787/',
    });
    const cpuPage = await cpuCtx.newPage();
    const cpuErrors = trackErrors(cpuPage);
    await openMode(cpuPage, '0');
    await importFiles(cpuPage, [JPEG_A]);
    await waitCanvasWidthAbove(cpuPage, 1200);
    await applyColorEdit(cpuPage);
    await waitStableSharpCanvas(cpuPage);

    const g = await downscaled(gpuPage, 512);
    const c = await downscaled(cpuPage, 512);
    expect(c.w).toBe(g.w);
    expect(c.h).toBe(g.h);
    const db = psnr(Buffer.from(g.b64, 'base64'), Buffer.from(c.b64, 'base64'));

    const gpuNames = await markNames(gpuPage);
    const cpuNames = await markNames(cpuPage);
    expect(gpuNames).toContain('worker.render_base');
    expect(gpuNames).toContain('gpu.render');
    expect(gpuNames).not.toContain('worker.render');
    expect(cpuNames).toContain('worker.render');
    expect(cpuNames).not.toContain('gpu.render');

    recordMetrics(testInfo, {
      parity_psnr_db: Math.round(db * 100) / 100,
      parity_width: g.w,
      parity_height: g.h,
      gpu_base_requests: gpuNames.filter((n) => n === 'worker.render_base').length,
    });
    expect(gpuErrors, gpuErrors.join('\n')).toEqual([]);
    expect(cpuErrors, cpuErrors.join('\n')).toEqual([]);
    expect(db, `parity PSNR ${db.toFixed(2)} dB < 45 dB`).toBeGreaterThanOrEqual(45);

    await cpuCtx.close();
  });

  test('ten slider ticks run on the GPU with no worker round trip', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    await openMode(page, '1');
    await importFiles(page, [JPEG_A]);
    await waitCanvasWidthAbove(page, 1200);
    await waitMark(page, 'worker.render_base');
    await waitMark(page, 'gpu.render');
    await clickTab(page, 'Color');
    await page.evaluate(() => (window as any).__pes.reset());

    for (let i = 1; i <= 10; i++) {
      await setRange(page, /^Brightness/, i / 40);
      await page.waitForTimeout(40);
    }
    await waitMark(page, 'gpu.render', 10);

    const ms = await marks(page);
    const names = ms.map((m) => m.name);
    const draws = names.filter((n) => n === 'gpu.render').length;
    expect(draws).toBeGreaterThanOrEqual(10);
    for (const banned of ['hi.request', 'worker.render', 'worker.render_base']) {
      expect(names, `slider tick hit the worker: ${banned}`).not.toContain(banned);
    }
    const timed = (name: string) =>
      ms.filter((m) => m.name === name).map((m) => m.dur_ms).sort((a, b) => a - b);
    const pick = (a: number[], f: 'median' | 'max') =>
      a.length ? (f === 'max' ? a[a.length - 1] : a[Math.floor(a.length / 2)]) : 0;
    const renders = timed('preview.render');
    expect(renders.length).toBeGreaterThanOrEqual(10);
    const submit = timed('gpu.render');
    const blits = timed('gpu.blit');
    const renderer = await webglRenderer(page);
    const software = SOFTWARE_GL.test(renderer);

    const tickMedian = pick(renders, 'median');
    const tickMax = pick(renders, 'max');
    recordMetrics(testInfo, {
      gpu_draws: draws,
      gpu_renderer: renderer,
      gpu_software_renderer: software,
      gpu_preview_render_median_ms: Math.round(tickMedian * 100) / 100,
      gpu_preview_render_max_ms: Math.round(tickMax * 100) / 100,
      gpu_submit_median_ms: Math.round(pick(submit, 'median') * 100) / 100,
      gpu_blit_median_ms: Math.round(pick(blits, 'median') * 100) / 100,
      gpu_worker_roundtrips: names.filter((n) => /^worker\.render/.test(n)).length,
    });
    expect(errors, errors.join('\n')).toEqual([]);
    // The 16 ms budget is a hardware claim: on a software rasterizer the
    // WebGL->2D blit is a CPU readback, so only record it there. Auto mode
    // never selects the GPU path on these renderers (docs/GPU.md).
    if (!software) {
      expect(tickMedian, `median tick ${tickMedian.toFixed(2)} ms > 16 ms`).toBeLessThanOrEqual(16);
    }
  });

  test('auto mode follows the renderer (software falls back to CPU)', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    const renderer = await webglRenderer(page);
    const software = SOFTWARE_GL.test(renderer);
    await page.goto('./');
    await expect(page.locator('header h1')).toHaveText('photo-edit-simplified');
    await importFiles(page, [JPEG_A]);
    await waitCanvasWidthAbove(page, 1200);

    if (software) {
      const names = await markNames(page);
      expect(names).toContain('hi.blit');
      expect(names).not.toContain('gpu.render');
      expect(names).not.toContain('worker.render_base');
    } else {
      await waitMark(page, 'gpu.render');
      const names = await markNames(page);
      expect(names).toContain('worker.render_base');
      expect(names).not.toContain('hi.request');
      expect(names).not.toContain('worker.render');
    }

    recordMetrics(testInfo, {
      auto_renderer: renderer,
      auto_software_renderer: software,
      auto_gpu_draws: (await markNames(page)).filter((n) => n === 'gpu.render').length,
      auto_cpu_hi_requests: (await markNames(page)).filter((n) => n === 'hi.request').length,
    });
    expect(errors, errors.join('\n')).toEqual([]);
  });

  test('gpu=0 keeps the CPU hi-res pipeline intact', async ({ page }, testInfo) => {
    const errors = trackErrors(page);
    await openMode(page, '0');
    await importFiles(page, [JPEG_A]);
    await waitCanvasWidthAbove(page, 1200);
    await waitMark(page, 'hi.blit');

    const names = await markNames(page);
    expect(names).toContain('hi.request');
    expect(names).toContain('worker.render');
    expect(names).toContain('hi.blit');
    expect(names).not.toContain('gpu.render');
    expect(names).not.toContain('worker.render_base');

    recordMetrics(testInfo, {
      fallback_hi_blits: names.filter((n) => n === 'hi.blit').length,
      fallback_gpu_draws: names.filter((n) => n === 'gpu.render').length,
    });
    expect(errors, errors.join('\n')).toEqual([]);
  });
});
