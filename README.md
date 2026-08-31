# photo-edit-simplified

A mobile-first, fully client-side photo and video editor for social media.
Upload, crop to platform presets, rotate/straighten, adjust basic color, apply
the same edits across a batch, and export ready-to-post files. No accounts, no
server — media never leaves your device.

## Features

- **Photos** (JPEG, PNG, HEIC): crop presets (9:16, 1:1, 4:5, 16:9, original),
  90° rotation + fine straighten (±10°), brightness/contrast/saturation/warmth,
  JPEG export at exact social dimensions (e.g. 1080×1920).
- **Video**: same crop/rotate/color edits plus trim, transcoded in-browser to
  MP4 (H.264 + AAC) via ffmpeg.wasm.
- **Batch**: multi-select upload, "Apply edits to all", download all.
- **PWA**: installable, works offline after first load.

## Tech

- Rust + WebAssembly UI ([Leptos](https://leptos.dev)), pixel ops in Rust.
- [ffmpeg.wasm](https://github.com/ffmpegwasm/ffmpeg.wasm) for video.
- [heic2any](https://github.com/alexcorvi/heic2any) for HEIC import.

## Development

Requires [Rust](https://rustup.rs) with the wasm target and
[trunk](https://trunkrs.dev):

```sh
rustup target add wasm32-unknown-unknown
trunk serve        # dev server on :8080
trunk build --release --public-url /photo-edit-simplified/
```

## Deployment

GitHub Actions builds and deploys to GitHub Pages on every push to `main`
(`.github/workflows/deploy.yml`). Enable Pages (source: GitHub Actions) in the
repo settings.

## License

Apache-2.0 — see [LICENSE](LICENSE).
