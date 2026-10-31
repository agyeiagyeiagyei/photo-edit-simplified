// Pixel worker bootstrap: loads the wasm-bindgen bundle beside this file and
// dispatches {id, op, args} messages. ArrayBuffers in results are transferred
// back (zero-copy). Errors come back as {id, ok: false, error}.
// build.sh rewrites the glue import below to the content-hashed filename.
import init, * as api from './pixel_worker.js';

const ready = init();

function collectTransfers(v, out) {
  if (v instanceof ArrayBuffer) {
    out.push(v);
  } else if (v && typeof v === 'object') {
    for (const k of Object.keys(v)) collectTransfers(v[k], out);
  }
  return out;
}

// RAW originals cross the thread boundary as Blobs — structured clone keeps
// them by reference, so the app thread never copies the file. The bytes only
// materialise here, inside the worker. ArrayBuffers (legacy proxy records)
// get the same treatment: wasm ops expect a Uint8Array.
async function resolveArg(v) {
  if (typeof Blob !== 'undefined' && v instanceof Blob) {
    return new Uint8Array(await v.arrayBuffer());
  }
  if (v instanceof ArrayBuffer) {
    return new Uint8Array(v);
  }
  return v;
}

self.onmessage = async (e) => {
  const { id, op, args } = e.data;
  try {
    await ready;
    const fn = api[op];
    if (typeof fn !== 'function') throw new Error(`unknown op: ${op}`);
    const t0 = performance.now();
    const result = fn(...(await Promise.all((args || []).map(resolveArg))));
    const ms = performance.now() - t0;
    self.postMessage({ id, ok: true, result, ms }, collectTransfers(result, []));
  } catch (err) {
    self.postMessage({ id, ok: false, error: String(err && err.message ? err.message : err) });
  }
};
