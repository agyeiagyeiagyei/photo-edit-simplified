# photo-edit-simplified

A desktop-first, fully client-side RAW culling and develop app built for
birding photography. Import a shoot straight from a folder or Google Drive,
flip through frames with hover loupes, star the keepers, develop them in
linear light, and export a batch or save the edits back to Drive. Nothing is
uploaded: every pixel is decoded and edited in the browser.

## What it does

- **Fast RAW ingest** — drag/drop, file picker or a Google Drive folder.
  A RAW frame shows the camera's embedded JPEG immediately; sensor pixels are
  demosaiced only when a frame is developed or exported, so a full card is
  browsable in seconds.
- **Culling** — filmstrip across the shoot, hover loupes for focus checks,
  a ★ shortlist stored in a Drive manifest (`photo-edit-shortlist.json`) that
  survives reloads and machines, an "unstarred" filter for working through a
  shoot, and "Import starred" to bring just the keepers back.
- **RAW develop, in linear light** — temperature (2500–9500 K, 5500 K =
  as-shot), tint, exposure, highlights, shadows, chroma-noise suppression,
  edge-aware denoise and capture sharpening. A neutral 16-bit path preserves
  highlight headroom before the camera tone curve.
- **Editing** — crop with aspect presets or freeform, 90° rotation and fine
  straighten, brightness/contrast/saturation/warmth, blur, film grain
  (Gaussian or uniform, luma or colour), levels, tone curves, and a bracket
  of social output sizes.
- **Retouch and compose** — rect/lasso/per-pixel selections with feathering,
  magic wand, subject masks, heal and clone, plus raster, text and vector-path
  layers with opacity and reordering.
- **Video** — the same crop/rotate/colour controls plus trim and text overlay,
  transcoded locally to MP4 with ffmpeg.wasm.
- **Delivery** — single or batch export to JPEG (with sRGB ICC), ZIP download,
  ASCII-safe filenames (`edited-1x1-<name>.jpg`), and "Save to Drive". Edit
  recipes persist per item, so reopening the app restores the develop state
  without touching the original file.

## Privacy and security

Media never leaves the device; there is no backend and no account database.
Google Drive is optional and uses OAuth (the `drive.readonly` and `drive.file`
scopes only) — the app reads and writes only the files and folders you pick,
and tokens stay in memory for the session.

The Drive API key and OAuth client ID in `src/drive.rs` are public client
identifiers, not secrets — that is how Google's JS APIs work. They must be
restricted in the Google Cloud console: the API key limited to the **Google
Drive API** with **HTTP referrer** restrictions for the Pages origin
(`https://agyeiagyeiagyei.github.io/photo-edit-simplified/*`) and localhost
development, and the OAuth client limited to the same JavaScript origins. If
you fork this and use your own project, apply the same restrictions; an
unrestricted key can be used against other APIs under your quota.

## Build

Requires [Rust](https://rustup.rs) with the `wasm32-unknown-unknown` target and
[trunk](https://trunkrs.dev). The pixel worker is a separate wasm module with
its own build step:

```sh
rustup target add wasm32-unknown-unknown
./build.sh                 # pixel worker wasm + wasm-bindgen glue, content-hashed
trunk build --release --public-url /photo-edit-simplified/
```

For local development, `./build.sh && trunk serve`; the app is then at
`http://localhost:8080`. Re-run `./build.sh` whenever anything under `worker/`,
`pixel/`, `rawcore/` or `rawmeta/` changes — the trunk post-build hook rewrites
the worker filename in `index.html` from `target/pes-worker-meta.txt`.

Serve `dist/` over HTTP (not `file://`): workers, WASM and the service worker
need an origin. A clean full rebuild:

```sh
./build.sh && trunk build --release --public-url /photo-edit-simplified/
python3 -m http.server 8787 --directory dist
```

## Tests

```sh
cargo test --workspace
cargo clippy --workspace -- -D warnings
(cd tests/e2e && npm ci && npx playwright test)   # builds fixtures first
```

CI (`.github/workflows/deploy.yml`) runs the Rust tests, the clippy gate, a
worker size budget, and the Playwright suite; the Pages deploy depends on all
of them.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — workers, caches, storage,
  render pipeline, service worker.
- [`docs/PERF.md`](docs/PERF.md) — performance work and measured budgets.
- [`docs/GPU.md`](docs/GPU.md) — WebGL/GPU status and plans.
- [`docs/raw-preview-export-plan.md`](docs/raw-preview-export-plan.md) — RAW
  preview/export design notes.
- [`spec.md`](spec.md) — product spec and decisions log.
- [`vendor/rawler/PATCHES.md`](vendor/rawler/PATCHES.md) — local patches to the
  vendored RAW decoder.

## License

Apache-2.0 — see [LICENSE](LICENSE). The vendored `rawler` decoder is
LGPL-2.1; see `vendor/rawler/LICENSE`.
