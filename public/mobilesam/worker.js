// MobileSAM (int8) subject segmentation worker. Loaded lazily on first use of
// the Subject select tool; terminated when the tool closes so the wasm memory
// comes back. Single-threaded ORT Web: GitHub Pages cannot send COOP/COEP.

import * as ort from '../vendor/ort/ort.wasm.min.mjs';

const MODEL_VERSION = 'v1-56663f87';
const MODEL_DIR = new URL(`../models/mobilesam/${MODEL_VERSION}/`, self.location.href).href;
const ORT_DIR = new URL('../vendor/ort/', self.location.href).href;
const MASK_MAX_EDGE = 3200; // approved raster cap; the 1024 preview stores 1:1
const SAM_SIZE = 1024;
// SAM's own mask threshold (logit 0): the approved rule (#3). A higher margin
// trims thin features, and keeping only the tapped connected component cut
// birds in two where a branch or wire crosses them (QA, owner photos).
const MASK_LOGIT_THRESHOLD = 0;
const PIXEL_MEAN = [123.675, 116.28, 103.53];
const PIXEL_STD = [58.395, 57.12, 57.375];

ort.env.wasm.wasmPaths = ORT_DIR;
ort.env.wasm.numThreads = 1;
ort.env.wasm.proxy = false;
ort.env.logLevel = 'error';

let encoder = null;
let decoder = null;
let loadPromise = null;
const states = new Map(); // id -> { region, embedding, points, frameW, frameH }

let wasmPeak = 0;
(function trackWasmMemory() {
  function note(m) {
    if (m && m.buffer && m.buffer.byteLength > wasmPeak) wasmPeak = m.buffer.byteLength;
  }
  try {
    const Orig = WebAssembly.Memory;
    const grow = Orig.prototype.grow;
    Orig.prototype.grow = function (n) {
      const out = grow.call(this, n);
      note(this);
      return out;
    };
    const origInstantiate = WebAssembly.instantiate;
    WebAssembly.instantiate = function (...args) {
      return origInstantiate.apply(this, args).then((r) => {
        try {
          note(r && r.instance && r.instance.exports && r.instance.exports.memory);
        } catch (_) {}
        return r;
      });
    };
  } catch (_) {}
})();

function post(msg, transfer) {
  self.postMessage(msg, transfer || []);
}

async function fetchWithProgress(url, label, onBytes) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${label}: HTTP ${res.status}`);
  const total = Number(res.headers.get('content-length') || 0);
  const reader = res.body.getReader();
  const chunks = [];
  let loaded = 0;
  let reported = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.byteLength;
    if (loaded === total || loaded - reported >= 512 * 1024) {
      reported = loaded;
      onBytes(loaded, total);
    }
  }
  onBytes(loaded, total);
  const out = new Uint8Array(loaded);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.byteLength;
  }
  return out;
}

async function ensureLoaded(report) {
  if (encoder && decoder) return;
  if (loadPromise) return loadPromise;
  loadPromise = (async () => {
    // Warm the HTTP/SW cache for the 14 MB ORT wasm first so the download
    // progress the user sees covers the bulk of the transfer; ORT then
    // re-reads it from cache when the session is created.
    const info = await (await fetch(`${MODEL_DIR}model.json`, { cache: 'no-store' })).json();
    const encoderBytes = info.files['encoder.int8.onnx'].bytes;
    const decoderBytes = info.files['decoder.int8.onnx'].bytes;
    const wasmBytes = 14239897;
    const total = encoderBytes + decoderBytes + wasmBytes;
    let base = 0;
    let phase = 0;
    const onBytes = (loaded, len) => {
      report({ phase: 'download', loaded: base + loaded, total, file: phase });
    };
    report({ phase: 'download', loaded: 0, total, file: 0 });
    await fetchWithProgress(`${ORT_DIR}ort-wasm-simd-threaded.wasm`, 'ort wasm', (l) => onBytes(l, wasmBytes));
    base += wasmBytes;
    phase = 1;
    const enc = await fetchWithProgress(`${MODEL_DIR}encoder.int8.onnx`, 'encoder', (l) => onBytes(l, encoderBytes));
    base += encoderBytes;
    phase = 2;
    const dec = await fetchWithProgress(`${MODEL_DIR}decoder.int8.onnx`, 'decoder', (l) => onBytes(l, decoderBytes));
    report({ phase: 'session', loaded: total, total });
    encoder = await ort.InferenceSession.create(enc, { executionProviders: ['wasm'], graphOptimizationLevel: 'all' });
    decoder = await ort.InferenceSession.create(dec, { executionProviders: ['wasm'], graphOptimizationLevel: 'all' });
  })();
  try {
    await loadPromise;
  } catch (e) {
    loadPromise = null;
    throw e;
  }
}

function regionOf(state) {
  return state.region;
}

function mapPoints(state, points) {
  const { region } = state;
  const scale = SAM_SIZE / Math.max(region.w, region.h);
  return {
    coords: points.flatMap((p) => [(p.x - region.x) * scale, (p.y - region.y) * scale]),
    labels: points.map((p) => p.label),
    scale,
  };
}

function preprocess(bitmap, region) {
  const canvas = new OffscreenCanvas(SAM_SIZE, SAM_SIZE);
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  const scale = SAM_SIZE / Math.max(region.w, region.h);
  const dw = Math.max(1, Math.round(region.w * scale));
  const dh = Math.max(1, Math.round(region.h * scale));
  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, SAM_SIZE, SAM_SIZE);
  ctx.imageSmoothingEnabled = true;
  ctx.imageSmoothingQuality = 'high';
  ctx.drawImage(bitmap, region.x, region.y, region.w, region.h, 0, 0, dw, dh);
  const rgba = ctx.getImageData(0, 0, SAM_SIZE, SAM_SIZE).data;
  const n = SAM_SIZE * SAM_SIZE;
  const out = new Float32Array(3 * n);
  for (let i = 0; i < n; i++) {
    const r = rgba[i * 4], g = rgba[i * 4 + 1], b = rgba[i * 4 + 2];
    out[i] = (r - PIXEL_MEAN[0]) / PIXEL_STD[0];
    out[n + i] = (g - PIXEL_MEAN[1]) / PIXEL_STD[1];
    out[2 * n + i] = (b - PIXEL_MEAN[2]) / PIXEL_STD[2];
  }
  return { data: out, scale };
}

async function encodeRegion(state, bitmap, region, report) {
  const t0 = performance.now();
  const { data } = preprocess(bitmap, region);
  const tensor = new ort.Tensor('float32', data, [1, 3, SAM_SIZE, SAM_SIZE]);
  const out = await encoder.run({ input_image: tensor });
  state.region = region;
  state.embedding = out.image_embeddings;
  state.encodeMs = performance.now() - t0;
  state.encodeCount = (state.encodeCount || 0) + 1;
  if (report) report({ phase: 'encode', loaded: 1, total: 1, ms: state.encodeMs });
}

async function decodePoints(state, points) {
  const t0 = performance.now();
  const { coords, labels } = mapPoints(state, points);
  const n = labels.length;
  // Store the mask in the frame's own pixel space (1:1 with the preview) so
  // preview uses it directly and export upsamples it bilinearly, the same
  // path the wand mask takes.
  const outScale = Math.min(1, MASK_MAX_EDGE / Math.max(state.frameW, state.frameH));
  const outW = Math.max(1, Math.round(state.frameW * outScale));
  const outH = Math.max(1, Math.round(state.frameH * outScale));
  const regionW = Math.max(1, Math.round(state.region.w * outScale));
  const regionH = Math.max(1, Math.round(state.region.h * outScale));

  const feeds = {
    image_embeddings: state.embedding,
    point_coords: new ort.Tensor('float32', new Float32Array(coords), [1, n, 2]),
    point_labels: new ort.Tensor('float32', new Float32Array(labels), [1, n]),
    mask_input: new ort.Tensor('float32', new Float32Array(256 * 256), [1, 1, 256, 256]),
    has_mask_input: new ort.Tensor('float32', new Float32Array([0]), [1]),
    // Region-sized output: the model cannot see past the region it encoded.
    orig_im_size: new ort.Tensor('float32', new Float32Array([regionH, regionW]), [2]),
  };
  const result = await decoder.run(feeds);
  const masks = result.masks;
  const ious = result.iou_predictions.data;
  // Token 0 is the single-mask output; indices 1..3 are the three candidates
  // the interaction spec selects among. Highest predicted IoU wins.
  let best = 1;
  for (let i = 2; i < ious.length; i++) if (ious[i] > ious[best]) best = i;
  const plane = regionW * regionH;
  const logits = masks.data.subarray(best * plane, (best + 1) * plane);

  const binary = new Uint8Array(plane);
  for (let i = 0; i < plane; i++) binary[i] = logits[i] > MASK_LOGIT_THRESHOLD ? 255 : 0;

  const regionMask = new Uint8ClampedArray(plane * 4);
  for (let i = 0; i < plane; i++) {
    const v = binary[i];
    regionMask[i * 4] = v;
    regionMask[i * 4 + 1] = v;
    regionMask[i * 4 + 2] = v;
    regionMask[i * 4 + 3] = 255;
  }
  const regionCanvas = new OffscreenCanvas(regionW, regionH);
  regionCanvas.getContext('2d').putImageData(new ImageData(regionMask, regionW, regionH), 0, 0);

  const full = new OffscreenCanvas(outW, outH);
  const ctx = full.getContext('2d', { willReadFrequently: true });
  ctx.imageSmoothingEnabled = true;
  ctx.imageSmoothingQuality = 'high';
  ctx.drawImage(
    regionCanvas,
    0, 0, regionW, regionH,
    Math.round(state.region.x * outScale),
    Math.round(state.region.y * outScale),
    regionW, regionH
  );
  const rgba = ctx.getImageData(0, 0, outW, outH).data;
  const mask = new Uint8Array(outW * outH);
  for (let i = 0; i < mask.length; i++) mask[i] = rgba[i * 4];

  return {
    mask,
    outW,
    outH,
    region: { ...state.region },
    iou: ious[best],
    candidate: best,
    decodeMs: performance.now() - t0,
  };
}

function maskBBox(mask, outW, outH, frameW, frameH) {
  let x0 = outW, y0 = outH, x1 = -1, y1 = -1;
  for (let y = 0; y < outH; y++) {
    const row = y * outW;
    for (let x = 0; x < outW; x++) {
      if (mask[row + x]) {
        if (x < x0) x0 = x;
        if (x > x1) x1 = x;
        if (y < y0) y0 = y;
        if (y > y1) y1 = y;
      }
    }
  }
  if (x1 < 0) return null;
  const sx = frameW / outW, sy = frameH / outH;
  return { x: x0 * sx, y: y0 * sy, w: (x1 - x0 + 1) * sx, h: (y1 - y0 + 1) * sy };
}

function cropRegion(bbox, frameW, frameH) {
  const cw = Math.min(frameW, Math.max(512, bbox.w * 3));
  const ch = Math.min(frameH, Math.max(512, bbox.h * 3));
  const cx = bbox.x + bbox.w / 2;
  const cy = bbox.y + bbox.h / 2;
  const x = Math.min(Math.max(0, cx - cw / 2), frameW - cw);
  const y = Math.min(Math.max(0, cy - ch / 2), frameH - ch);
  return { x, y, w: cw, h: ch };
}

async function solve(state, bitmap, points, report) {
  // Whole frame first, then a 3x crop when the first mask is small in frame.
  await encodeRegion(state, bitmap, { x: 0, y: 0, w: state.frameW, h: state.frameH }, report);
  let res = await decodePoints(state, points);
  let usedCrop = false;
  const bbox = maskBBox(res.mask, res.outW, res.outH, state.frameW, state.frameH);
  if (bbox) {
    const long = Math.max(bbox.w, bbox.h);
    const frameLong = Math.max(state.frameW, state.frameH);
    if (long < 0.25 * frameLong) {
      const region = cropRegion(bbox, state.frameW, state.frameH);
      await encodeRegion(state, bitmap, region);
      res = await decodePoints(state, points);
      usedCrop = true;
    }
  }
  return { ...res, usedCrop };
}

function stateFor(id, bitmap) {
  let state = states.get(id);
  if (state && (state.frameW !== bitmap.width || state.frameH !== bitmap.height)) {
    state = null; // geometry changed under the same id: old embedding is invalid
  }
  if (!state) {
    state = { frameW: bitmap.width, frameH: bitmap.height, points: [] };
    states.set(id, state);
  }
  return state;
}

async function handleTap(id, bitmap, point, report) {
  const state = stateFor(id, bitmap);
  state.points.push(point);
  let res;
  const inside =
    state.embedding &&
    point.x >= state.region.x && point.x <= state.region.x + state.region.w &&
    point.y >= state.region.y && point.y <= state.region.y + state.region.h;
  if (state.embedding && inside) {
    res = { ...(await decodePoints(state, state.points)), usedCrop: state.region.w < state.frameW || state.region.h < state.frameH };
  } else {
    res = { ...(await solve(state, bitmap, state.points, report)), encodeMs: state.encodeMs || 0 };
  }
  return res;
}

async function handleBox(id, bitmap, box, report) {
  const state = stateFor(id, bitmap);
  state.points = [
    { x: box.x0, y: box.y0, label: 2 },
    { x: box.x1, y: box.y1, label: 3 },
  ];
  return { ...(await solve(state, bitmap, state.points, report)), encodeMs: state.encodeMs || 0 };
}

function reply(req, result, transfer) {
  post({ type: 'reply', reqId: req.reqId, ok: true, result }, transfer);
}

function fail(req, error) {
  post({ type: 'reply', reqId: req.reqId, ok: false, error: String((error && error.message) || error) });
}

self.onmessage = async (e) => {
  const req = e.data;
  try {
    if (req.type === 'load') {
      await ensureLoaded((p) => post({ type: 'progress', reqId: req.reqId, ...p }));
      reply(req, { encoderBytes: 14040628, decoderBytes: 4761904, wasmPeakBytes: wasmPeak });
    } else if (req.type === 'tap') {
      await ensureLoaded((p) => post({ type: 'progress', reqId: req.reqId, ...p }));
      const res = await handleTap(req.id, req.bitmap, { x: req.x, y: req.y, label: req.label }, (p) => post({ type: 'progress', reqId: req.reqId, ...p }));
      req.bitmap.close && req.bitmap.close();
      reply(req, { ...res, wasmPeakBytes: wasmPeak }, [res.mask.buffer]);
    } else if (req.type === 'box') {
      await ensureLoaded((p) => post({ type: 'progress', reqId: req.reqId, ...p }));
      const res = await handleBox(req.id, req.bitmap, req.box, (p) => post({ type: 'progress', reqId: req.reqId, ...p }));
      req.bitmap.close && req.bitmap.close();
      reply(req, { ...res, wasmPeakBytes: wasmPeak }, [res.mask.buffer]);
    } else if (req.type === 'reset') {
      const state = states.get(req.id);
      if (state) state.points = [];
      reply(req, { ok: true });
    } else if (req.type === 'release') {
      states.delete(req.id);
      req.bitmap && req.bitmap.close && req.bitmap.close();
      reply(req, { ok: true });
    } else {
      fail(req, 'unknown request');
    }
  } catch (err) {
    try {
      req.bitmap && req.bitmap.close && req.bitmap.close();
    } catch (_) {}
    fail(req, err);
  }
};
