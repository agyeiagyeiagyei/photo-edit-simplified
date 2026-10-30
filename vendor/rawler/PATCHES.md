# Local patches to vendored rawler

Upstream: dnglab rawler 0.8.0 (see `Cargo.toml.orig`). Keep this list current
when touching vendored code.

## Precompiled camera database (2026-10-31, WS2 #6)

- `data/join.rs` — instead of writing the 306 KB joined `cameras.toml`, writes
  `$OUT_DIR/camera_db.bin` (magic `PESCDB3\n`): each camera base table once,
  its mode tables, alias pairs, and a naked-by-filesize index.
- `src/decoders/camera_db.rs` (new) — decodes one entry per lookup and replays
  the old loader's base-then-mode `Camera::update_from_toml` sequence.
  `CameraDb::all()` materialises the full map for `RawLoader::get_cameras`.
- `src/decoders/mod.rs` — `RawLoader` holds a `CameraDb`; `CAMERAS_TOML` is no
  longer embedded, so `RawLoader::new` no longer parses TOML.
- Parity: `camera_db` tests re-parse a test-only `$OUT_DIR/cameras_ref.toml`
  with the original algorithm and compare every field for every camera,
  alias x mode, plus naked filesize entries.

Effect: first `rawler::get_decoder` in a worker 127 ms → 19 ms (synthetic DNG,
Chromium, this container); worker wasm 1,271,968 → 1,293,780 B gz.

## wasm32 timing calls

At import, `std::time::Instant`-based demosaic timing logs were stripped:
`Instant::now()` panics on `wasm32-unknown-unknown`. Keep timing out of the
decode path; worker perf marks use `js_sys::Date::now()` via `rawcore`.
