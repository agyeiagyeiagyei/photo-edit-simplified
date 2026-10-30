#!/usr/bin/env bash
# Build the pixel worker: wasm, wasm-bindgen glue, then wasm-opt -O3.
# Same pipeline locally and in CI; run `trunk build` afterwards to copy
# public/pixel-worker into dist/.
set -euo pipefail
cd "$(dirname "$0")"

WB_VERSION=$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | grep '^version' | head -1 | sed 's/version = "\(.*\)"/\1/')
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/pes-build"
mkdir -p "$CACHE"

ARCH=$(uname -m)
case "$ARCH" in
  x86_64) WB_TRIPLE=x86_64-unknown-linux-musl ;;
  aarch64|arm64) WB_TRIPLE=aarch64-unknown-linux-musl ;;
  *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

WB_BIN="$CACHE/wasm-bindgen-$WB_VERSION-$WB_TRIPLE/wasm-bindgen"
if [ ! -x "$WB_BIN" ]; then
  echo "fetching wasm-bindgen $WB_VERSION ($WB_TRIPLE)"
  curl -fsSL "https://github.com/rustwasm/wasm-bindgen/releases/download/$WB_VERSION/wasm-bindgen-$WB_VERSION-$WB_TRIPLE.tar.gz" | tar xz -C "$CACHE"
fi

if [ -z "${WASM_OPT:-}" ]; then
  if command -v wasm-opt >/dev/null 2>&1; then
    WASM_OPT=$(command -v wasm-opt)
  elif [ -x "$HOME/.cache/trunk/wasm-opt-version_124/bin/wasm-opt" ]; then
    WASM_OPT="$HOME/.cache/trunk/wasm-opt-version_124/bin/wasm-opt"
  else
    case "$ARCH" in
      x86_64) BIN_ARCH=x86_64-linux ;;
      aarch64|arm64) BIN_ARCH=aarch64-linux ;;
    esac
    echo "fetching binaryen version_124 ($BIN_ARCH)"
    curl -fsSL "https://github.com/WebAssembly/binaryen/releases/download/version_124/binaryen-version_124-${BIN_ARCH}.tar.gz" | tar xz -C "$CACHE"
    WASM_OPT="$CACHE/binaryen-version_124/bin/wasm-opt"
  fi
fi

cargo build -p pixel-worker --release --target wasm32-unknown-unknown
"$WB_BIN" --target web --out-dir public/pixel-worker target/wasm32-unknown-unknown/release/pixel_worker.wasm
"$WASM_OPT" -O3 --enable-simd --enable-bulk-memory \
  -o public/pixel-worker/pixel_worker_bg.wasm public/pixel-worker/pixel_worker_bg.wasm

raw=$(wc -c < public/pixel-worker/pixel_worker_bg.wasm)
gz=$(gzip -9c public/pixel-worker/pixel_worker_bg.wasm | wc -c)
echo "worker wasm: $raw bytes raw / $gz bytes gzip"
