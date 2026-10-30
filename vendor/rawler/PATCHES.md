# Local patches to vendored rawler

Upstream: dnglab rawler 0.8.0 (see `Cargo.toml.orig`). Keep this list current
when touching vendored code.

## Precompiled camera database (2026-10-31, WS2 #6)

- `data/join.rs` — instead of writing the 306 KB joined `cameras.toml`, writes
  `$OUT_DIR/camera_db.bin` (magic `PESCDB4\n`): each camera base table once,
  its mode tables, alias pairs, and a naked-by-filesize index. Floats outside
  `[params]` are stored as f32 (every such value is read back as f32 by
  `Camera::update_from_toml`); `params` values stay f64 because `Camera::params`
  keeps the raw `toml::Value`.
- `src/decoders/camera_db.rs` (new) — decodes one entry per lookup and replays
  the old loader's base-then-mode `Camera::update_from_toml` sequence.
  `CameraDb::all()` materialises the full map for `RawLoader::get_cameras`.
- `src/decoders/mod.rs` — `RawLoader` holds a `CameraDb`; `CAMERAS_TOML` is no
  longer embedded, so `RawLoader::new` no longer parses TOML.
- Parity: `camera_db` tests re-parse a test-only `$OUT_DIR/cameras_ref.toml`
  with the original algorithm and compare every field for every camera,
  alias x mode, plus naked filesize entries. The f32 encoding passes this
  bit-for-bit.

Effect: first `rawler::get_decoder` in a worker 127 ms → 19 ms (synthetic DNG,
Chromium, this container).

## Dependency trim for the wasm worker (2026-10-31, WS2 #6)

- `jxl-oxide` is optional behind a new `jpegxl` cargo feature (off by default).
  JPEG-XL-compressed DNG strips/tiles are rare; without the feature they hit
  the existing "Unsupported compression method" error instead of decoding.
- Removed `backtrace` (12 log-only `Backtrace::new()` calls in
  `formats/tiff/value.rs` plus one in `lib.rs`'s `RawlerError` conversion).
- Tried `log/max_level_off`; it made the optimised wasm ~3.5 KB gz *larger*
  (layout noise), so it is not enabled.

Effect: worker wasm 1,293,780 → 809,539 B gz locally (jxl-oxide was ~480 KB gz);
CI runs ~0.6% larger than local. Budget ratcheted to 900,000 B.

## wasm32 timing calls

At import, `std::time::Instant`-based demosaic timing logs were stripped:
`Instant::now()` panics on `wasm32-unknown-unknown`. Keep timing out of the
decode path; worker perf marks use `js_sys::Date::now()` via `rawcore`.
