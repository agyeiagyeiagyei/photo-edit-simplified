#!/usr/bin/env bash
# Fail when a gzipped wasm bundle exceeds budgets.json. Run after a release
# build (trunk build --release; worker built with wasm-bindgen into dist).
set -euo pipefail
cd "$(dirname "$0")/.."

main=$(ls dist/photo-edit-simplified-*_bg.wasm 2>/dev/null | head -1 || true)
worker=$(ls dist/pixel-worker/pixel_worker_bg.*.wasm 2>/dev/null | head -1 || true)
[ -n "$main" ] && [ -f "$main" ] || { echo "missing main wasm in dist — run trunk build"; exit 1; }
[ -n "$worker" ] && [ -f "$worker" ] || { echo "missing worker wasm in dist"; exit 1; }

main_gz=$(gzip -9c "$main" | wc -c)
worker_gz=$(gzip -9c "$worker" | wc -c)

# Subject tool assets download lazily on first use of Select > Subject: ORT
# Web plus the int8 MobileSAM encoder/decoder in the current model version.
version=$(python3 -c "import json;print(json.load(open('dist/models/mobilesam/current.json'))['version'])")
subject_files=(
  dist/vendor/ort/ort.wasm.min.mjs
  dist/vendor/ort/ort-wasm-simd-threaded.mjs
  dist/vendor/ort/ort-wasm-simd-threaded.wasm
  dist/mobilesam/worker.js
  "dist/models/mobilesam/$version/encoder.int8.onnx"
  "dist/models/mobilesam/$version/decoder.int8.onnx"
)
subject_gz=0
for f in "${subject_files[@]}"; do
  [ -f "$f" ] || { echo "missing subject asset: $f"; exit 1; }
  subject_gz=$((subject_gz + $(gzip -9c "$f" | wc -c)))
done

python3 - "$main_gz" "$worker_gz" "$subject_gz" <<'EOF'
import json, sys
budget = json.load(open('budgets.json'))
main, worker, subject = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
ok = True
for name, value, limit in (
    ('main', main, budget['main_wasm_gzip_bytes']),
    ('worker', worker, budget['worker_wasm_gzip_bytes']),
    ('subject', subject, budget['subject_lazy_gzip_bytes']),
):
    pct = value / limit * 100
    status = 'ok' if value <= limit else 'OVER BUDGET'
    print(f'{name:7} {value/1e6:6.2f} MB gzip  limit {limit/1e6:.2f} MB ({pct:.0f}%)  {status}')
    if value > limit:
        ok = False
cap = budget['subject_lazy_gzip_hard_cap_bytes']
print(f'subject hard cap {cap/1e6:.0f} MB: ' + ('ok' if subject <= cap else 'OVER'))
if subject > cap:
    ok = False
print(json.dumps({
    'main_wasm_gzip_bytes': main,
    'worker_wasm_gzip_bytes': worker,
    'subject_lazy_gzip_bytes': subject,
}))
sys.exit(0 if ok else 1)
EOF
