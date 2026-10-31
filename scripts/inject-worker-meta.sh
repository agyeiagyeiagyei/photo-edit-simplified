#!/usr/bin/env sh
# Trunk post-build hook: point <meta name="pes-worker"> at the fingerprinted
# worker bootstrap that build.sh just emitted. Patches the staged HTML —
# trunk only publishes dist/ after hooks succeed. Fails the build when the
# placeholder or the manifest is missing, so a deploy can never ship a page
# that cannot start its worker.
set -eu
root="${TRUNK_SOURCE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
out="${TRUNK_STAGING_DIR:-${TRUNK_DIST_DIR:-$root/dist}}/index.html"
meta="$root/target/pes-worker-meta.txt"
[ -f "$out" ] || { echo "missing $out" >&2; exit 1; }
[ -f "$meta" ] || { echo "missing $meta — run ./build.sh before trunk build" >&2; exit 1; }
python3 - "$out" "$(cat "$meta")" <<'EOF'
import re, sys
path, name = sys.argv[1], sys.argv[2]
html = open(path, encoding="utf-8").read()
tag = f'<meta name="pes-worker" content="{name}"/>'
pat = re.compile(r'<meta\s+name="pes-worker"[^>]*>')
if not pat.search(html):
    sys.exit("index.html has no pes-worker meta placeholder")
open(path, "w", encoding="utf-8").write(pat.sub(tag, html, count=1))
EOF
