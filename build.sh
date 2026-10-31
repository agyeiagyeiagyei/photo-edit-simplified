#!/usr/bin/env bash
# Build the pixel worker: wasm, wasm-bindgen glue, then wasm-opt -O3.
# Emits content-hashed filenames into public/pixel-worker, plus the bootstrap
# run.<hash>.js generated from scripts/pixel-worker-run.js. Same pipeline
# locally and in CI; run `trunk build` afterwards (its post-build hook injects
# the run.js name into dist/index.html as <meta name="pes-worker">).
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

OUT=public/pixel-worker
# Everything in here is generated (run.js is templated from scripts/), so a
# clean directory keeps old hashes from being published.
rm -rf "$OUT"
mkdir -p "$OUT"

cargo build -p pixel-worker --release --target wasm32-unknown-unknown
"$WB_BIN" --target web --out-dir "$OUT" target/wasm32-unknown-unknown/release/pixel_worker.wasm
"$WASM_OPT" -O3 --enable-simd --enable-bulk-memory \
  -o "$OUT/pixel_worker_bg.wasm" "$OUT/pixel_worker_bg.wasm"

# Content-hash the wasm, then point the glue at it and hash the glue: a new
# app wasm never talks to a stale worker, and the hashed files are cache-first
# for the service worker.
hash() { sha256sum "$1" | cut -c1-16; }
rm -f "$OUT"/*.d.ts
wasm_hash=$(hash "$OUT/pixel_worker_bg.wasm")
mv "$OUT/pixel_worker_bg.wasm" "$OUT/pixel_worker_bg.$wasm_hash.wasm"
sed -i "s/pixel_worker_bg\.wasm/pixel_worker_bg.$wasm_hash.wasm/g" "$OUT/pixel_worker.js"
glue_hash=$(hash "$OUT/pixel_worker.js")
mv "$OUT/pixel_worker.js" "$OUT/pixel_worker.$glue_hash.js"
sed "s#\./pixel_worker\.js#./pixel_worker.$glue_hash.js#" scripts/pixel-worker-run.js > "$OUT/run.js"
run_hash=$(hash "$OUT/run.js")
mv "$OUT/run.js" "$OUT/run.$run_hash.js"

# The trunk post-build hook reads this and writes <meta name="pes-worker">.
mkdir -p target
echo "pixel-worker/run.$run_hash.js" > target/pes-worker-meta.txt

wasm="$OUT/pixel_worker_bg.$wasm_hash.wasm"
raw=$(wc -c < "$wasm")
gz=$(gzip -9c "$wasm" | wc -c)
echo "worker wasm: $raw bytes raw / $gz bytes gzip"
