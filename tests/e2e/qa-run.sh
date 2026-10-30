#!/usr/bin/env bash
# qa-run.sh <label> [base-url]   (env REPEAT=n, default 3)
# Runs the suite REPEAT times against the live site (default) or a URL, keeps each run's metrics as
# results-<label>-<i>-metrics.json, and prints a markdown comparison (median) vs the 2026-10-30 live
# baseline (BASELINE=path overrides; skipped when the file is absent).
set -u
E2E="$(cd "$(dirname "$0")" && pwd)"; cd "$E2E" || exit 1
LABEL=${1:-run}; URL=${2:-https://agyeiagyeiagyei.github.io/photo-edit-simplified/}; REPEAT=${REPEAT:-3}
BASELINE=${BASELINE:-results-live-metrics.json}
worst=0; files=()
for i in $(seq 1 $REPEAT); do
  rm -rf results
  PES_BASE_URL="$URL" npx playwright test --reporter=list 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -vE '^\s*$' | grep -vE '^\s+at ' | tee "results-$LABEL-$i.log" | grep -E '✘|passed|failed|skipped'
  code=${PIPESTATUS[0]}; [ $code -gt $worst ] && worst=$code
  cp results/metrics.json "results-$LABEL-$i-metrics.json" 2>/dev/null && files+=("results-$LABEL-$i-metrics.json")
done
echo; echo "## QA run '$LABEL' ($REPEAT× on $URL) vs baseline (live, c2aee95)"
if [ -f "$BASELINE" ]; then
  python3 -I compare.py "$BASELINE" "${files[@]}" --label "$LABEL"
else
  echo "(no baseline at $BASELINE — skipping comparison)"
fi
exit $worst
