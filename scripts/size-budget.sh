#!/usr/bin/env bash
# Fail when a gzipped wasm bundle exceeds budgets.json. Run after a release
# build (trunk build --release; worker built with wasm-bindgen into dist).
set -euo pipefail
cd "$(dirname "$0")/.."

main=$(ls dist/photo-edit-simplified-*_bg.wasm 2>/dev/null | head -1 || true)
worker=dist/pixel-worker/pixel_worker_bg.wasm
[ -n "$main" ] && [ -f "$main" ] || { echo "missing main wasm in dist — run trunk build"; exit 1; }
[ -f "$worker" ] || { echo "missing worker wasm in dist"; exit 1; }

main_gz=$(gzip -9c "$main" | wc -c)
worker_gz=$(gzip -9c "$worker" | wc -c)

python3 - "$main_gz" "$worker_gz" <<'EOF'
import json, sys
budget = json.load(open('budgets.json'))
main, worker = int(sys.argv[1]), int(sys.argv[2])
ok = True
for name, value, limit in (
    ('main', main, budget['main_wasm_gzip_bytes']),
    ('worker', worker, budget['worker_wasm_gzip_bytes']),
):
    pct = value / limit * 100
    status = 'ok' if value <= limit else 'OVER BUDGET'
    print(f'{name:6} {value/1e6:6.2f} MB gzip  limit {limit/1e6:.2f} MB ({pct:.0f}%)  {status}')
    if value > limit:
        ok = False
print(json.dumps({'main_wasm_gzip_bytes': main, 'worker_wasm_gzip_bytes': worker}))
sys.exit(0 if ok else 1)
EOF
