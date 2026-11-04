# Architecture

photo-edit-simplified is a static single-page app. There is no backend: the
GitHub Pages host serves wasm, JS and the service worker, and every decode,
edit and encode happens in the browser. This document describes the runtime
shape of that app — the workers, where pixels live, the caches and their
budgets, the IndexedDB schema, the edit-recipe wire format, and the order of
the render pipeline. It is the map QA should use to reason about tests.

## Process model

```
main thread (Leptos UI + canvas)
 ├─ interactive pixel worker      (wasm, singleton)
 ├─ RAW worker                    (wasm, singleton)
 └─ RAW batch pool                (wasm, 0–3 temporary workers)
```

All pixel work lives in Web Workers compiled from the same `worker/` crate to
wasm; the main thread never holds a full-resolution image buffer. Large
payloads (RGBA frames, encoded files, RAW bytes) cross the boundary as
transferables.

### Main thread

The Leptos app owns UI state, edit parameters, the filmstrip and canvases, and
all persistence orchestration. It stores only *metadata* about pixel buffers
(id, dimensions, flags) and asks a worker for the pixels when it needs to
paint. It also hosts the canvas 2D compositor used for layers at preview and
export time.

### Interactive pixel worker

The singleton worker handles hi-res renders, preview renders, destructive
edits (heal, clone, cut, fill), loupe windows, JPEG/PNG encode, and RAW
develops that the user is waiting on. `index.html` creates it at parse time as
`window.__pesWorker` so its bundle downloads in parallel with the app wasm;
the app adopts it on start. Its URL is content-hashed:

```
<meta name="pes-worker"> → run.<hash>.js → pixel_worker.<hash>.js →
pixel_worker_bg.<hash>.wasm
```

The chain is built by `./build.sh`; a new app wasm can therefore never talk to
a stale worker, and the service worker can treat all three files as immutable.

### RAW worker

A second worker instance exists because a full RAW demosaic is multi-second.
Running it on the interactive worker would stall the preview the user is
looking at. The RAW worker receives the original bytes and returns a display
proxy or a full-resolution result; the UI stays responsive on camera JPEGs
meanwhile.

### RAW batch pool

During an import, each RAW's display proxy is developed on a pool so a card's
worth of files does not queue behind one worker. Pool size is
`clamp(hardwareConcurrency − 2, 1, 3)`, further capped at roughly one worker
per 2 GB of device memory (each worker's wasm memory is significant). Every
pool slot runs one develop at a time. The pool is torn down when the import
batch drains — wasm memory does not shrink in place, so keeping the workers
alive would hold that high-water mark for the whole session.

### Worker protocol

Requests are `{id, op, args}`; replies are `{id, ok, result?, error?, ms}`.
`ms` is measured inside the worker and recorded as a perf mark on the main
thread. Operations include:

- buffer lifecycle — `load`, `load_neutral`, `load_photo`, `load_photo_view`,
  `unload`
- rendering — `render`, `render_base`, `render_window`, `crop_window`,
  `loupe_window`
- destructive edits — `heal_stroke`, `clone_stamp`, `cut`, `fill_selection`,
  `extract`
- RAW — `raw_proxy`, `camera_proxy`, `raw_meta`, `raw_develop`, `raw_preview`
- output — `export_photo`, `encode_jpeg`, `encode_png`

`pworker::call(op, args, transfer, cb)` wraps the round trip. If the worker
cannot be created (or fails to load), ops return an error and callers fall
back to the main-thread path.

## Pixels, caches and budgets

### Full-resolution buffers (worker-owned)

Decoded sensor pixels never leave the worker. The main thread's `PhotoData`
carries only flags: `full`, `full_dims`, `neutral` (a 16-bit develop is
present), and `dirty` (a destructive edit landed and has not yet been
persisted). The worker holds them in a keyed buffer table.

The budget is computed once from `navigator.deviceMemory`: **1 GB** when the
device reports at least 8 GB, otherwise **512 MB**. The estimate per item is
`width × height × 4` bytes, plus `× 2` when a 16-bit neutral develop is also
resident. `evict_full_lru` drops the least-recently-touched *clean* full
buffers until the total fits and sends `unload` for each. Dirty items are
exempt: the worker copy is the only source of truth until the session save
persists it.

### Display proxies

Browsing does not need full resolution. A **proxy** is a display-size image
at `PROXY_EDGE = 3200` px on the long edge, in one of two forms:

- the camera's embedded JPEG (RAW frames, camera-JPEG-first ingest), or
- a worker-demosaiced JPEG for frames whose embedded JPEG is unusable.

Up to **160 MB** of proxies are held in memory, shared across items and
evicted LRU; each proxy knows whether a *developed* (edit-applied) and/or
*neutral* copy of its pixels exists. Proxies are persisted in IndexedDB so a
reload is editable without re-reading a 25 MB RAW. A batch pool worker may be
developing a proxy that has not landed yet; the pool queue is prioritized
when the selection changes.

### Hi-res render cache and loupes

The hi-res preview cache holds one rendered frame per selected item at up to
`HI_MAX_EDGE = 3200` px. Hover loupes (filmstrip focus loupe, Drive loupe,
canvas loupe, camera loupe) are separate small copies with their own caches,
keyed by item and pointer position. The eviction pass protects any item the
loupes are currently showing and the item under the cursor, so moving the
pointer never evicts the pixels that make the loupe possible.

### Destructive edits

Heal, clone, cut and fill mutate the worker's base pixels for an item and set
`dirty` on the main thread. A destructive op invalidates the item's embedded
camera JPEG, its stored proxy and the hi-res cache; the next session save
writes the new pixels to `layer_pixels`/`proxies` as appropriate. These
operations are not part of the non-destructive recipe — they are baked into
the item's base image.

## IndexedDB: `pes-session` v5

Persistence is split by write frequency and payload size so that an edit-param
change never rewrites megabytes of pixels.

| store | key | contents |
|---|---|---|
| `items` | item id | one small record per item: name, kind, dims, edit params, layers (metadata), flags |
| `meta` | fixed keys | session-level state (ordering/ids, Drive folder, counters) |
| `layer_pixels` | `"<itemId>:<layerId>"` | PNG blobs for raster layer pixels, referenced by layer metadata |
| `raw_src` | item id | original RAW/JPEG bytes (Blob) — read on demand for a lazy develop |
| `previews` | item id | 1024 px preview PNG for RAW items |
| `proxies` | item id | display proxy bytes (camera JPEG or demosaiced JPEG), sized for immediate editing after reload |
| `thumbs` | item id | 256 px JPEG filmstrip thumbnail, written at import so restore decodes no originals |
| `drive_proxies` | Drive file id | camera-JPEG display proxies for Drive files browsed before import |

On restore the app rebuilds the filmstrip from `thumbs`, item records and
recipes, without decoding a single full image. Thumbnails are also why a
reload after the service worker's update-reload is fast.

Browser storage is origin-scoped; private modes and low-quota situations fall
back to keeping RAW bytes in memory (`raw_fallback`) for the session rather
than failing the import.

## Edit recipe wire format

An edit recipe is a compact JSON object shared by the session store, the
Drive shortlist manifest, and the worker protocol. `pixel/src/recipe.rs` owns
both directions.

- Top-level version: `{"v":1, ...}`. Keys with identity values are omitted;
  the all-default recipe is `{"v":1}`.
- Geometry: `aspect` (`"original" | "9:16" | "1:1" | "4:5" | "16:9"`), `crop`
  (`{x,y,w,h}` normalized), `rot90` (0–3), `fine_angle` (degrees).
- Colour: `brightness`, `contrast`, `saturation`, `warmth`, `blur`, `grain`,
  `grain_gaussian`, `grain_mono`.
- Levels: array of four channel rows (R, G, B, composite), each
  `[black, gamma, white, output_black, output_white]`.
- Curves: array of four channel point arrays, `[[[x,y],...], ...]`.
- RAW develop: a nested `raw` object with temperature, tint, exposure,
  highlights, shadows, noise, denoise and sharpen (short keys, e.g. `sp` for
  sharpen).
- Video: `trim: [start_s, end_s]`, `keep_audio`.
- Selection masks, layer pixels and stroke data are **not** serialized — they
  are session-only.

The worker rebuilds `EditParams` from the full JSON form
(`edit_params_full_json`), which always carries explicit values; the stored
manifest form is the compact one. Parsing is forgiving: unknown keys are
ignored and missing keys fall back to defaults, so recipes survive schema
additions. A malformed recipe parses as `None` and the caller treats the item
as untouched.

## Render pipeline

`pes_pixel::pipeline` is the single pipeline used by previews, exports and
the worker; the old duplicated colour pipelines were removed in WS5.

1. **Prepare / geometry** — crop, quarter-turns and fine straighten with
   edge auto-crop produce the source frame the render sees.
2. **Resample** — scale to the render target (preview, proxy, export rect) by
   the shared resampler; RAW paths stay in 16-bit sRGB when the neutral
   pipeline applies.
3. **Develop (RAW only, linear light)** — white balance (temperature/tint),
   exposure as a gain, highlight shoulder, shadow toe, chroma-noise
   suppression. Output is 16-bit sRGB. For the neutral pipeline the camera
   tone curve is applied here, after the develop; otherwise the camera
   rendering is the starting point.
4. **Colour (gamma space)** — levels, curves, brightness/contrast/saturation/
   warmth; blur and grain run at their stage in this pass.
5. **Mask blend** — when `edit.selection` is set, the processed frame is
   blended back against the unprocessed frame through the (feathered) mask.
6. **Quantise** — one conversion to u8 RGBA with a final dither on the
   wide path.

The pipeline has two entry points: the u8 path (`Frame`) and the neutral
16-bit path (`Frame16`). `neutral_eligible` decides whether a RAW edit can
use the latter; stages it cannot represent (denoise, capture sharpen) force
the u8 path so an edit never silently loses a stage.

Destructive operations run outside this pipeline against the worker's base
pixels. Layer compositing runs on a canvas on the main thread: the worker
renders the edited base, the canvas draws raster/text/vector layers over it,
and the result is encoded. When there are no layers, the worker renders the
exact export rectangle and encodes it without ever sending full-res RGBA to
the main thread.

Filenames are `edited-<aspect-slug>-<name>.<ext>` with `orig`, `1x1`, `4x5`,
`9x16`, `16x9` — `:` is not legal in Windows or macOS filenames.

## Service worker

`public/sw.js` owns two caches:

- **`pes-v5`** — the app shell and all same-origin assets.
  - Precached at install: `./`, `index.html`, the webmanifest, both icons.
    Keeping the precache tiny makes updates activate quickly.
  - Navigations are **network-first**, falling back to the cached page
    offline. This is deliberate: deploys reach open clients on the next load
    instead of being pinned behind a stale cache-first HTML.
  - Hashed JS/WASM (including the three-file worker chain) are
    **cache-first**; their names change with every deploy.
  - On activate the worker deletes every other cache, walks the live
    `index.html` to learn the current hashed asset and worker files, and
    prunes anything no longer referenced. It then claims clients and posts
    `pes-reload` so a page loads once more and picks up the new bundle.
- **`pes-drive-thumbs-v1`** — Drive grid/loupe thumbnails
  (`drive.google.com/thumbnail?…`). These are cross-origin `<img>` loads, so
  responses are opaque and have no usable Content-Length; the cap is therefore
  a count, **600 entries** (roughly 200 MB at Drive's usual sizes). It is
  separated from the shell cache so the LRU trim can never delete app files,
  and it is exempt from the activate sweep. A cache hit re-inserts the entry
  to approximate LRU.

The cache name must change whenever the strategy changes: activate deletes
all other caches, which is how clients stuck on an old strategy get unpinned.

## Google Drive integration

`src/drive.rs` holds config and REST calls; the OAuth token client and Picker
dance live in `index.html` as JS shims. The app never sees an API secret — it
uses a public browser API key plus OAuth with `drive.readonly` and
`drive.file` scopes. It reads only the folder the user grants via the Picker
and writes only files it creates.

- Drive grid thumbnails come from `drive.google.com/thumbnail` and are cached
  by the service worker as above.
- Browsing a RAW in Drive decodes its embedded camera JPEG once and stores it
  in the `drive_proxies` IDB store keyed by file id; revisiting reuses it
  instead of downloading the original again.
- The shortlist is `photo-edit-shortlist.json` (manifest version 2) in the
  granted folder: starred file ids plus their compact edit recipes. It is
  written debounced; reading it on load re-applies stars and recipes.
- Imports can be filtered to unstarred files, or run as "Import starred" to
  pull in a culling pass from another machine.

See README's privacy/security section for the Google Cloud key restrictions
that must be configured for a fork.

## Build and test surface

- `./build.sh` builds the worker chain and writes the fingerprinted names;
  `trunk build --release --public-url /photo-edit-simplified/` builds the app
  and injects the worker meta tag.
- `cargo test --workspace` covers the pixel kernels, pipeline parity, the
  recipe round trip and rawmeta.
- `cargo clippy --workspace -- -D warnings` is the lint gate (vendored rawler
  opts out; see `vendor/rawler/PATCHES.md`).
- `scripts/size-budget.sh` enforces the gzip budgets in `budgets.json`
  (currently 700,000 B for the main wasm, 980,000 B for the worker wasm).
- `tests/e2e` runs Playwright against the built `dist/`; memory tests need a
  dedicated COOP/COEP server.
