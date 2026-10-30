#!/usr/bin/env python3
"""compare.py baseline.json new1.json [new2.json ...] --label NAME
Prints a markdown table: baseline vs the MEDIAN of the new runs (numeric metrics only).
Flags: 🟢 improvement beyond the noise floor, 🔴 regression beyond it, ~ inside the noise floor.
Noise floor (measured 2026-10-30, same commit, live Pages): compute-bound metrics ±8 %, network-bound
(`shell_visible_ms`, `*_fetch_ms`, `dom_content_loaded_ms`, `load_event_ms`, `cached_shell_visible_ms`) ±35 %."""
import json, sys, statistics
args = [a for a in sys.argv[1:] if not a.startswith('--')]
label = sys.argv[sys.argv.index('--label') + 1] if '--label' in sys.argv else (args[2] if len(args) > 2 and not args[2].endswith('.json') else 'new')
files = [a for a in args if a.endswith('.json')]
base = json.load(open(files[0])); runs = [json.load(open(f)) for f in files[1:]]
NET = ('shell_visible_ms', 'cached_shell_visible_ms', 'wasm_main_fetch_ms', 'wasm_worker_fetch_ms', 'dom_content_loaded_ms', 'load_event_ms')
def flat(d):
    out = {}
    for _, m in d.items():
        for k, v in m.items():
            if isinstance(v, (int, float)) and not isinstance(v, bool): out[k] = v
    return out
b = flat(base); ns = [flat(r) for r in runs]
keys = sorted(set(b) | set().union(*[set(n) for n in ns]))
print(f'| metric | baseline | {label} (median of {len(ns)}) | Δ | verdict |\n|---|---:|---:|---:|---|')
for k in keys:
    vals = [n[k] for n in ns if k in n]
    nv = statistics.median(vals) if vals else None; bv = b.get(k)
    if bv is None or nv is None or bv == 0:
        print(f'| `{k}` | {bv if bv is not None else "—"} | {nv if nv is not None else "—"} | | |'); continue
    pct = (nv - bv) / bv * 100; floor = 35 if k in NET else 8
    verdict = '~ noise' if abs(pct) < floor else ('🟢 better' if pct < 0 else '🔴 worse')
    if not (k.endswith('_ms') or k.endswith('_bytes')): verdict = '' if pct == 0 else 'changed'
    print(f'| `{k}` | {bv:g} | {nv:g} | {pct:+.0f}% | {verdict} |')
