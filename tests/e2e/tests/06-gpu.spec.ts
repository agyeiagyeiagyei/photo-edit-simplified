import { test, expect, Page } from '@playwright/test';
import {
  importFiles,
  clickTab,
  setRange,
  recordMetrics,
  trackErrors,
  waitCanvasWidthAbove,
  JPEG_A,
} from './helpers';

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
    return { w: W, h, b64: btoa(s) };
  }, width);
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
async function waitStableSharpCanvas(page: Page, timeout = 90_000) {
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
  while (Date.now() < deadline) {
    const cur = await sample();
    if (cur !== 'soft' && cur === prev) return;
    prev = cur;
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

test.describe('WebGL2 parametric preview (docs/GPU.md)', () => {
  test('gpu render matches the CPU pipeline within 45 dB', async ({ page }, testInfo) => {
    const gpuPage = page;
    const gpuErrors = trackErrors(gpuPage);
    await openMode(gpuPage, '1');
    await importFiles(gpuPage, [JPEG_A]);
    await waitCanvasWidthAbove(gpuPage, 1200);
    await waitMark(gpuPage, 'worker.render_base');
    await waitMark(gpuPage, 'gpu.render');
    await applyColorEdit(gpuPage);
    await waitStableSharpCanvas(gpuPage);

    const cpuPage = await gpuPage.context().newPage();
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

    await cpuPage.close();
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
