# Perf marks

The app records its own timings on every run — no flag, no build mode. Each
instrumented step emits a real `performance.mark()` / `performance.measure()`
pair (visible in the DevTools performance timeline) and is kept in a 500-entry
in-memory ring buffer.

```js
window.__pes.perf()   // → [{ name, dur_ms, at, detail }, …], oldest first
window.__pes.reset()  // clears the ring (the DevTools timeline is untouched)
window.__pes.mem()    // → { main, raw_worker } wasm linear-memory bytes
```

`__pes.mem()` is the memory high-water probe: wasm memory never shrinks, so
`main` is the app worker's peak, and `raw_worker` is the max `mem` reported by
RAW develop replies (0 until the first one).

`at` is `performance.now()` when the measure closed. `detail` is a free-form
string (dimensions, item names) or `null`. Names are stable; the e2e suite and
QA compare against them.

The mark pair for a measure is `__pes_<id>_start` / `__pes_<id>_end`; both are
cleared right after the measure so the timeline never accumulates marks.

## App marks (main thread)

| Name | Measures | Detail |
|---|---|---|
| `app.mount` | `main()` through the first `mount_to_body` | — |
| `ingest.read` | reading a JPEG/PNG for EXIF (RAW sources skip this: the Blob crosses to the worker) | file name |
| `ingest.decode` | decoding: `prepare_raw`, HEIC conversion, `load_image`, video probe | file name |
| `ingest.preview` | 1024px preview downscale | file name |
| `ingest.thumb` | thumbnail encode + object URL | file name |
| `ingest.push` | inserting the item into the filmstrip | file name |
| `preview.render` | the whole preview effect body for one edit state | canvas backing `WxH` |
| `hi.request` | debounce fired → hi-res render posted to the worker | requested `WxH` |
| `hi.worker_reply` | request posted → worker reply received | — |
| `hi.blit` | worker pixels → canvas → layer composite | — |
| `proxy.camera_decode` | camera-JPEG proxy decode in the raw worker | — |
| `proxy.raw_develop` | sensor develop to a 3200px display proxy | — |
| `proxy.raw_develop_full` | full-size sensor develop in the raw worker | — |
| `proxy.raw_meta` | one raw-worker round-trip: EXIF, capture time, preview, camera JPEG | — |
| `raw.get_decoder` | first `rawler::get_decoder` in this worker (includes the camera-DB build) | — |
| `proxy.raw_preview` | embedded RAW preview (no demosaic) at display size | — |
| `proxy.land` | installing a decoded proxy (cache, worker load, LRU) | item id |
| `export.ensure_full` | waiting for full-res pixels to materialise | — |
| `export.geometry` | geometry stage of the export pipeline | working `WxH` |
| `export.color` | raw develop + colour/levels/curves/grain stage | — |
| `export.encode` | composite → crop → encode → EXIF injection | output `WxH` |
| `export.total` | full export of one item (including `ensure_full`) | item name |
| `session.persist` | one IndexedDB session write pass | item count |
| `restore.session` | whole startup restore | item count |
| `restore.item` | restoring one item | `name #id` |

## RAW display proxy

`rawcore::decode_raw_proxy` chooses the demosaic for the 3200px display proxy:

- Bayer RGB sensors whose binned long edge covers at least 90% of the target
  (`SUPERPIXEL_MIN_COVER`, e.g. 6192/2 = 3096 ≥ 0.9 × 3200) use
  `RawDevelop::superpixel()`: 2×2 binning to a half-size RGB proxy. Small
  sensors, X-Trans and 4-colour CFAs keep the normal demosaic.
- The proxy's tone curve is fitted once against the camera preview and stored
  with it; the export develop (`decode_raw_with_tone`) reuses the same 256-byte
  LUT, so display and export share one tone response.
- `PROXY_DEVELOP_DELAY_MS` (750 ms) debounces selection changes before the
  sensor develop starts; a changed selection drops any in-flight result.

### Zero-copy RAW sources

RAW originals never become a main-thread `Vec<u8>` on the normal path. The
IndexedDB `raw_src` Blob is posted to the RAW worker as-is (Blob structured
clone is by reference); the worker bootstrap calls `blob.arrayBuffer()` and
hands the wasm call a `Uint8Array`. Worker results (proxy and full RGBA)
come back as transferred `ArrayBuffer`s and are kept on the main thread as
`Uint8Array` views (`Pixels::View`) — no `to_vec()`. Only the fallback path
(RAW that could not be persisted to IndexedDB) still reads bytes on the main
thread, because the session record has to embed them; ingest proxies there are
stored as Blobs instead of byte arrays.

Once a full develop has delivered its pixels, the RAW worker is terminated:
its wasm memory never shrinks while it lives, and a 24 MP develop peaks near
1 GB — memory that would otherwise sit idle through composite + encode, the
peak of an export. The next RAW op re-creates the worker lazily. `__pes.mem()`
keeps reporting the develop high-water after termination, so read `raw_worker`
as a peak, not current usage.

## Worker marks

The worker bootstrap (built from `scripts/pixel-worker-run.js` into
`public/pixel-worker/run.<hash>.js`) times each op around the wasm call and
returns `ms` with the reply; the app records it as:

| Name | Measures |
|---|---|
| `worker.<op>` | worker execution of `op` (`load`, `unload`, `render`, `raw_proxy`, `raw_meta`, `raw_develop`, `raw_preview`, `camera_proxy`, `heal_stroke`, `clone_stamp`, `cut`, `fill_selection`, `extract`) |

`render` additionally returns a `timings` object, recorded as:

| Name | Stage |
|---|---|
| `worker.render.geometry` | geometry transform |
| `worker.render.resample` | resample to the hi-res target |
| `worker.render.develop` | raw develop (`rawops::apply`) |
| `worker.render.color` | blur/levels/curves/adjust/grain (includes selection mask) |

## Reading them

- `hi.worker_reply` minus `worker.render` is message transit + queueing.
- `preview.render` is the main-thread cost of one edit tick; the settled
  hi-res path is `hi.request`-to-`hi.blit` plus the 150 ms debounce.
- `worker.*` values are executed off the main thread; they show where a slow
  op spends its time, not UI jank.
- Worker stage timings use `Date.now()` (ms resolution); the op-level `ms`
  uses `performance.now()` in JS.
