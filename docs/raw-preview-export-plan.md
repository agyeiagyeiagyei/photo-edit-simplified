# RAW preview sharpness and export — current state, plan, handover

Status: plan only. Written 2026-10-30 against `c2aee95`. Nothing in this
document is approved for implementation, commit or deployment.

## TL;DR

Two problems were reported on 2026-10-30 against a 26MP Sony ARW:

1. The crop view is soft, and a tight crop is close to unusable.
2. Download does not produce a file for an edited RAW.

Both come from the same design gap. Edits are already a parametric recipe, and
the app already keeps a JPEG working image, but:

- the working image is capped at 3200px on its **long edge for the whole
  frame**, so a tight crop only has a few hundred to ~1100 source pixels;
- export decodes the full sensor **synchronously on the UI thread**, keeps
  several full-frame copies, and fails silently.

The recommendation is to keep the existing "recipe over a JPEG working image"
architecture — it is the right one — but make the working image crop-aware and
move the full-sensor work off the UI thread. The camera's own JPEG is useful as
an instant fallback and for the loupe, but should not become the primary colour
source: the camera's rendering and the app's rawcore develop differ by about
3/255 mean, and preview/export parity is the property that matters most.

## What ships today (`c2aee95`)

- **Lazy RAW import**: the file's embedded camera preview decodes immediately;
  the original RAW bytes and a 1024px preview are stored in IndexedDB. The
  sensor develop is deferred (`prepare_raw`, `ensure_proxy`).
- **Display proxy**: when a RAW is selected and stays selected for 2s, the
  pixel worker `rawcore::decode_raw`s it and returns a 3200px proxy
  (`PROXY_EDGE`). It is persisted as a q0.9 JPEG so reloads skip the decode.
- **Edits are parametric**: `MediaItem.edit` (`EditParams`: geometry, raw
  develop, levels/curves, colour, blur, grain, selection, crop, layers) is
  stored with the item. The display pipeline re-renders pixels from the
  recipe; edits never bake into the source unless the user runs a destructive
  operation (heal/cut/fill/delete).
- **Hi-res display render**: `pworker::render` applies the recipe to the
  loaded source (sensor buffer when present, otherwise the display proxy).
  Canvas backing size is `max(viewport need) * 1.25`, capped at
  `HI_MAX_EDGE = 3200` for the **whole working frame**.
- **Crop view**: when a crop is set and the Crop tab is closed, the canvas
  shows the crop window cut out of that whole-frame render.
- **Loupe**: 1:1 loupe reads the camera's full-size embedded JPEG (6192×4128
  on the A6700) for lazy RAWs (`c2aee95`).
- **Export**: `ensure_full_async` → full sensor decode on the main thread →
  geometry/raw/colour → layer composite → native-pixel crop → PNG or JPEG
  canvas encode → EXIF injection → anchor download.
- **Live**: https://agyeiagyeiagyei.github.io/photo-edit-simplified/, deploy
  run 40998455304, live wasm `5cbf6a45a4f3e9a9`.

## Measurements

Fixture: `DSC05232.ARW`, 6192×4128 (26MP), 29MB. Browser: 1100×690 CSS,
devicePixelRatio 2.

| View | Canvas backing | Device pixels | Effective scale |
|---|---|---|---|
| Whole frame | 1725×1150 | 1380×920 | 0.80× (sharp) |
| After a 35% crop | 1127×756 | 1372×920 | **1.22× upscale** (soft) |

The 35% crop is cut from a 3200px whole-frame render, so the crop window has
`3200 * 0.352 ≈ 1127` pixels no matter how large the window is. At a 1600px CSS
window on the same display the upscale is ~2.8×; before the 3200px proxy lands
the crop comes from the 1024px preview and the window is only ~358px wide.

The same crop exported natively would be ~2180×1453 pixels: the preview shows
52% of the export's linear resolution.

Export behaviour:

- 6000×4000 JPEG, 35% crop, PNG: **works** — 2113×1417 PNG, 1.5s, no errors.
  So the download mechanism, crop math and PNG encoder are sound.
- 6192×4128 RAW: **page crash** in the 3.9GB test container during the
  deferred full develop. Previous probes measured the full develop freezing
  the main thread for ~45s on 30–43MB ARWs. On a larger machine this may
  "work" after a long freeze, but `render_photo_export` returns `None` on any
  failure and `export_photo` shows nothing — every failure mode is a dead
  button.

## Root causes

1. **The cap is on the whole frame, not the crop.** `hi_target` limits the
   long edge of the entire working image to 3200. A crop keeps only its
   fraction of those pixels. Resampling the canvas cannot add detail; the crop
   needs a render of the crop window at a resolution the display actually
   uses, from a source that has those pixels (the 3200 proxy does not; the
   camera JPEG and the full RAW do).
2. **The proxy is below sensor resolution.** 3200 vs 6192 is fine for a
   full-frame view on a 1100–1600px window but leaves a tight crop with
   nothing to magnify.
3. **Export does the expensive step on the UI thread.** The full develop is a
   ~100MB+ operation per copy, and the export path holds the sensor buffer,
   a full-size canvas, the crop canvas, and blob copies for EXIF at once. It
   has no progress reporting and no error reporting. A failure is
   indistinguishable from a button that does nothing.
4. **One edge case in the failure path**: when RAW bytes were not persisted
   (`raw_fallback` holds them in memory after a storage failure), `ensure_full`
   looks only in IndexedDB and never consults the fallback, so export fails.

## Direction

Keep the recipe architecture. It already delivers "apply edits to a working
image, use the RAW only for the final render". The missing pieces are
resolution where it matters and a non-blocking, observable export.

The one product decision that changes the design: **what should the exported
image look like?** Today the app renders a neutral rawcore develop and export
matches it (measured Δ1.18/255 mean, p95 3). The camera's own JPEG differs
from that develop by ~2.9–3.7/255 mean. Using the camera JPEG as the working
image would make preview↔preview consistent and instantly sharp, but export
from the RAW would then look different unless we also match the RAW develop to
the camera look (a per-image colour/tone profile). Recommendation: keep the
rawcore develop as the source of truth; use the camera JPEG for the loupe and
as an instant stand-in, not as the primary colour source.

## Plan

### Phase 1 — make export trustworthy (highest priority)

- Move the full-sensor develop and the recipe render into the pixel worker.
  Return either the crop-window pixels or the final encoded bytes; do not
  bring a 100MB RGBA buffer back to the main thread for a second copy.
- Show real progress (reading RAW / developing / encoding) and a real error on
  failure. Every `None` in `render_photo_export` must become a user-visible
  message.
- Reduce peak memory: one full-frame buffer, crop before compositing where
  layer coordinates allow, and avoid the extra blob copies for EXIF when the
  budget is tight (PNG can be downloaded without EXIF in the worst case).
- Keep exports 8-bit sRGB PNG ("lossless" = lossless compression of the
  edited pixels; the app's pipeline is 8-bit throughout) and state that in the
  UI if there is any ambiguity.
- Consult `raw_fallback` before declaring an item undecodable.
- Acceptance: 26MP ARW exports on Agyei's machine; the PNG opens at native
  crop dimensions; preview vs export MAE ≤2/255; a forced failure shows a
  message; no page crash and no multi-minute freeze.

### Phase 2 — crop view at native density

- Add a crop-window render: the worker decodes the RAW for the selected item
  (or uses the sensor buffer already loaded), applies the recipe, and returns
  only the displayed crop rect at up to its native pixels, capped at a sane
  long edge (proposal: 4096).
- Debounce it like the existing hi-res render; keep the 3200 proxy for live
  dragging, then swap in the native crop render when it lands.
- Instant fallback while the develop runs: use the camera JPEG crop (already
  used by the loupe, ~0.6s for 6192×4128) with a `camera` tag, replaced by
  the developed window. This is the only place the camera JPEG would be
  primary, and the tag makes the difference honest.
- Acceptance: at a 35% crop the canvas backing is at least the device-pixel
  width; the tag reports the real source; preview vs export MAE ≤2/255.

### Phase 3 — camera-look profile (only if Agyei prefers the camera rendering)

- Fit a per-image transform (white-balance gains + 3×3 matrix and/or a
  per-channel tone curve, optionally a small 3D LUT) from the developed
  preview to the camera JPEG at import.
- Apply the same transform in preview and in export, so "as on the preview"
  holds while using the RAW develop.
- Measure ΔE on 5 ARWs; guard highlight clipping and shadow noise.

### Phase 4 — durable high-res working layer (if crop-window latency is not enough)

- Store a developed 6192px JPEG per item (q0.92, ~8–12MB) instead of only the
  3200px proxy; decode it on selection, keep it in the worker budget, and use
  it for crop views. RAW is read only for export and for the first develop.
- One image at a time; do not move the import path back to eager develops.
- Migration: existing proxy records are either replaced in the background on
  first selection or accepted as-is.

## Risks and constraints

- **Memory**: a 6192×4128 RGBA frame is 102MB; the export path currently
  makes several copies. The wasm heap is ~4GB in Chrome and lower on iOS;
  the test container is 3.9GB and already OOMs on a 26MP full develop.
- **Time**: rawcore full develop measured ~5.7s on the 26MP frame in this
  container, and ~45s on larger ARWs in older probes. Moving it to the worker
  keeps the UI responsive but does not make it faster.
- **Worker transfers**: postMessage of a 100MB buffer copies unless it is
  transferred; results should be transferred, and the worker should own the
  full sensor buffer while it exists.
- **Canvas limits**: native crop exports of 6000px images are fine in Chrome;
  old iOS Safari has a much smaller canvas area limit. Decide whether iPad is
  in scope.
- **PNG size**: a 25MP lossless PNG can be tens of MB; download time and
  browser memory for the blob matter. JPEG max quality is the lighter option.
- **RAW controls on a camera JPEG**: exposure/highlight recovery assume
  linear developed data. Applying them to the camera JPEG (8-bit, display
  referred) clips differently. Another reason to keep the developed source.
- **Layers**: compositing happens in full-frame coordinates; a crop-window
  render must offset layer draws, or composite before cropping.
- **Service worker staleness**: after a deploy, verify the live asset hash
  before re-debugging any "still broken" report.
- **Drive**: a Drive RAW import downloads ~29MB and stores `raw_src`; export
  behaves identically. The same worker/export changes apply.

## Verification

Fixtures:

- 26MP A6700 ARW (`DSC05232.ARW`, 6192×4128). `/tmp` is wiped between turns;
  re-fetch it into `/tmp` when needed.
- A 12MP DNG for full-decode tests that fit the container.
- A synthetic 6000×4000 JPEG for export/UI tests (no RAW memory).

Probes (agent workspace `pdf-edit-test/`):

- `export-crop-probe.mjs` — imports a fixture, crops, applies brightness,
  selects PNG, clicks Download this, waits for the download event, reports
  canvas backing vs device pixels, export dimensions and errors. Used for the
  numbers above.
- `camera-loupe-test.mjs` — the camera-JPEG loupe check.
- Existing crop/export probes: `probe-canvas-fidelity.mjs`,
  `png-export-dims-probe.mjs`.

Every change should be checked locally against the release bundle
(`photo-edit/dist`) and, after an approved deploy, against Pages. The useful
metrics are: crop backing / device px ratio, export pixel dimensions vs the
native crop, PNG validity and EXIF presence, preview vs export MAE, develop
time and peak memory.

## Key code map

| Concern | Location |
|---|---|
| Lazy RAW ingest / preview | `src/main.rs::prepare_raw`, `ensure_proxy` |
| 3200px proxy develop | `src/main.rs::pump_upgrade`, `src/pworker.rs::raw_proxy` |
| Whole-frame display render | `src/main.rs::render_hi`, `render_source` |
| Crop view cut-out | `src/main.rs` ~5844–5962 |
| Hi-res cap | `pixel/src/geo.rs::hi_target`, `HI_MAX_EDGE` |
| Export pipeline | `src/main.rs::render_photo_export`, `export_photo` |
| Full decode | `src/main.rs::ensure_full` |
| Camera JPEG helpers | `src/raw.rs::largest_embedded_jpeg`, `camera_jpeg_rgba` |
| Worker develop | `worker/src/lib.rs::raw_proxy`, `camera_proxy`, `render` |
| Download | `src/web.rs::download_blob` |

Build: `CARGO_HOME=/workspace/agent/.cargo-home trunk build --release`
(package `pes-pixel`; deploy workflow builds the worker first).

## Open decisions

1. Output look: keep the neutral rawcore develop (recommended) or match the
   camera's JPEG rendering?
2. Order: export reliability first, or crop sharpness first?
3. Acceptable export time for a 26MP RAW (10s / 30s / a minute)?
4. If the full develop fails for memory, should export fall back to the camera
   JPEG with a warning, or fail with an explanation?
5. Device scope: desktop only, or iPad/iPhone too?

## Agent instructions

This section is the live handover channel. A scheduled watcher polls this file
on GitHub every 15 minutes, extracts everything between this heading and the
next `##`, hashes it, and wakes the agent only when it changes. The agent reads
this document first and treats this section as the current instruction. Edits
here are **instructions, not publish approval** — pushes, deploys and releases
still need an explicit yes in chat.

Current instruction (2026-10-30): Plan only. Do not implement, commit or push
beyond this document until Agyei says so. When he answers the open decisions,
update the plan and start with Phase 1 unless he says otherwise.
