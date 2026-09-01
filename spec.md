# photo-edit-simplified — Spec v0.3 (2026-08-28)

Owner: Agyei. Drafted by Atlas. Hand to a builder agent once evolved.

## One-liner
A mobile-first web app to upload photos **and videos**, crop/rotate/straighten,
adjust basic color, apply the same edits across a batch, and export
ready-to-post sizes for social media.

## Platform decisions
- **PWA, single page, fully client-side.** No backend, no accounts, no server
  storage. Media never leaves the device. Home-screen install, offline-capable.
- **Open source** on GitHub (public repo), **Apache-2.0 license**.
  README with screenshots + self-host instructions.
- **Name:** `photo-edit-simplified` (repo, Pages URL, app title).
- **Repo:** new repo under `github.com/agyeiagyeiagyei` — i.e.
  `github.com/agyeiagyeiagyei/photo-edit-simplified`.
- **Deploy:** GitHub Pages (static, from the repo's gh-pages or /docs via
  GitHub Actions build). Custom domain optional later.
- **Stack: Rust + WebAssembly.**
  - UI: Leptos or Yew (builder's call; Leptos default if no preference) —
    compiles to wasm, no JS framework.
  - Photo pixel ops: Rust `image` crate in wasm; canvas via web-sys.
  - Video: still **ffmpeg.wasm** (it's the only sane in-browser video
    transcode path; called from Rust via wasm-bindgen/js-sys interop).
  - HEIC: `heic2any` JS lib via interop — pure-Rust HEIC decode in wasm is
    not practical today. Wrap behind one import boundary so it can be swapped.

## Core features (v1)

### Photos
1. **Upload** — file picker (multi-select) + drag/drop + paste.
   Formats: JPEG, PNG, **HEIC** (via heic2any conversion on import).
2. **Crop** — free crop + aspect presets:
   - 9:16 (Reels/TikTok/Shorts, 1080×1920)
   - 1:1 (Instagram feed, 1080×1080)
   - 4:5 (Instagram portrait, 1080×1350)
   - 16:9 (YouTube/landscape, 1920×1080)
   - Original
3. **Rotate / straighten** — 90° buttons + fine-angle slider (±10°),
   auto-crop to hide edges.
4. **Color** — brightness, contrast, saturation, warmth. Live preview.
   Optional "Auto" button (histogram stretch) as stretch goal.
5. **Export** — JPEG q0.9 at exact preset dimensions.
   Filename: `edited-<preset>-<orig>.jpg`.

### Video
6. Same edit set for video: crop to the same aspect presets, rotate/straighten,
   brightness/contrast/saturation/warmth, plus **trim** (in/out points —
   needed for social-length clips; included in v1 scope).
7. **Engine: ffmpeg.wasm** (client-side transcode). Consequences to accept:
   - First load pulls ~30MB of wasm (cache it; show progress).
   - Mobile Safari memory ceiling ~2–4GB — cap input at, say, 500MB / 5 min
     and show a clear error beyond that. (Exact cap to be tuned in testing.)
   - Transcodes are minutes-long on phone hardware for long clips; show a
     progress bar and keep the page alive (wake lock).
   - Export: MP4 (H.264 + AAC) at preset dimensions, CRF ~22 equivalent.
8. Video edits preview on a poster frame / short scrub; full render only at
   export (real-time filtered video preview is a stretch goal via WebGL/CSS
   filters if cheap).

### Batch editing
9. Multi-select upload; edit one item, then **"Apply to all"** (or pick a
   subset) — copies crop-aspect + rotation + color values. Crops re-anchor
   to center on each item; per-item fine-tune stays possible.
10. Batch export produces individual files (zip download if many).

## Explicit non-goals (v1)
- No filters/presets library, no collages. (Text/drawing are v1
  non-goals only — see v2 below.)
- No accounts, no cloud save, no cross-session history.
- No audio editing on video (keep/strip toggle only — default keep).

## v2 candidates

### Duplicate / near-duplicate culling (validated 2026-09-01)
Client-side ML to cluster redundant shots after multi-select upload and
suggest a keeper per cluster. Validated offline on a real 14-photo burst
(one scene, varying poses) using CLIP `ViT-B-32` (laion2b_s34b_b79k)
embeddings + cosine similarity + union-find clustering, keeper picked by
Laplacian-variance sharpness:

- Threshold is scene-dependent and must be user-facing: 0.94 merged the
  whole burst into one cluster (everything is "same scene"); 0.97 culled
  only true dupes (14 → 11); 0.96 over-merged distinct poses (14 → 3).
  Ship a "strict ↔ aggressive" slider, default ~0.97.
- Sharpness-only keeper selection picks the technically sharpest frame,
  not necessarily the best composition — keeper must be manually
  swappable in the UI. Pose/face-aware scoring is a later refinement.
- Never auto-delete: clusters are suggestions; user confirms culls.

Implementation notes for the app: transformers.js + quantized CLIP
(~100MB, Cache API after first load, same pattern as ffmpeg.wasm).
Clusters render as stacks in the filmstrip with the keeper on top;
tap to expand, swap keeper, or ungroup.

### Creative editing suite (requested 2026-09-01, BUILT 2026-09-01)
Four interdependent features. **Layers is the substrate — build it
first**; drawing, text, and isolated subjects all live as layers.

Status: all four shipped — layers+text (f65492c), vector pen+brush
(52bc7d7), marquee+subject isolation with MediaPipe vendored for
offline (c8a26bb), text-on-video via PNG-overlay burn-in (2867cef,
verified end-to-end on a 27.6s clip with keep/strip audio).

1. **Layers** — ordered stack above the base photo. Per layer: type
   (raster/drawing/text), visibility toggle, opacity, reorder,
   delete, drag-to-position. Composite via canvas at preview and
   export; export flattens to JPEG.
2. **Marquee selection + background isolation** — rect and free-form
   (lasso) marquee for manual regions. Plus one-tap **subject
   isolation**: client-side person/subject segmentation (MediaPipe
   Selfie Segmentation or a quantized MODNet via ONNX Runtime Web —
   both small enough for mobile) to lift the subject onto its own
   layer. Once isolated: blur/darken/replace background, or move the
   subject. Feathered edges by default; marquee ops (cut/copy to
   layer, delete, adjust-within-selection) apply to both manual and
   ML selections.
3. **Pen tool (vector) + drawing** — the pen is a *vector* pen in
   the Illustrator/Fontra sense: click to place anchor points, drag
   out Bézier handles for curves, close the path, then fill and/or
   stroke it. Paths stay editable after placement (move points,
   adjust handles, add/delete anchors) and live as vector path
   layers — rasterized at export resolution, so they stay crisp at
   any output size. Plus a simple freehand brush mode for casual
   markup. Path editing must be touch-friendly (big handles,
   double-tap to convert smooth ↔ corner). Undo = per-path
   operation, not global. Not a full vector editor: no pathfinder
   boolean ops, no blend modes in the first cut.
4. **Font upload + text** — user uploads .ttf/.otf/.woff2, loaded
   via the FontFace API (stays on-device). Text layers: drag
   placement, size, color, weight where the font provides it,
   stroke/outline + shadow for legibility over photos, alignment.
   Ship a few bundled open-license defaults (e.g. Inter, Oswald) so
   it works without an upload.
   **Text layers also work on video.** Preview = CSS/canvas overlay
   on the video element (no re-render). Export: render each text
   layer to a transparent PNG at export resolution and burn it in
   during the ffmpeg.wasm transcode with the `overlay` filter —
   avoids needing a libfreetype-enabled ffmpeg.wasm build for
   `drawtext`. Static overlay for the whole clip in the first cut;
   timed in/out per text layer is a later refinement.
   (Marquee/isolation and pen drawing stay photo-only for now —
   per-frame segmentation and stroke rasterization on video are a
   bigger lift; revisit after text-on-video ships.)

## UX sketch
Grid of uploaded items → tap to edit one → bottom tool tabs
(Crop | Rotate | Color | Trim [video] | Export) → "Apply to all" bar when
multiple items loaded. Press-and-hold on canvas shows original.

## Decisions log
- 2026-08-28 (Agyei): batch editing is important — apply same fix to many.
- 2026-08-28 (Agyei): video support required, same edit functions.
- 2026-08-28 (Agyei): support both HEIC and JPEG.
- 2026-08-28 (Agyei): name = photo-edit-simplified.
- 2026-08-28 (Agyei): open-source on GitHub, hosted on GitHub Pages.
- 2026-08-28 (Agyei): build with Rust + WASM.
- 2026-08-28 (Agyei): Apache-2.0 license; new repo under github.com/agyeiagyeiagyei.

## Open questions
1–7 resolved (see decisions log). Spec is handoff-ready.

## Handoff notes for builder agent
- This file is the source of truth; update in place as decisions land.
- Verify on a real phone viewport (iOS Safari especially) before calling
  done — ffmpeg.wasm + HEIC conversion are the two likeliest mobile breakage
  points.
- Keep everything client-side; no server components without asking Agyei.
