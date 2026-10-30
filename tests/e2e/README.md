# photo-edit-simplified — QA end-to-end harness (Playwright)

Desktop Chrome, 1600×1000 @ DPR 2 (forces the hi-res render path). Every spec writes
its timings to `results/metrics.json` via `recordMetrics()`.

## Layout (drop into `tests/e2e/` in the repo)
```
package.json                @playwright/test only
playwright.config.ts        PES_BASE_URL picks local dist (default) or the live Pages URL
gen_fixtures.py             PIL script: two deterministic 24 MP JPEGs + a PNG → fixtures/ (do not commit fixtures)
tests/helpers.ts            openApp / importFiles / canvasMean / setRange / zipEntryCount / recordMetrics
tests/01-smoke.spec.ts      shell load, console errors, service worker, wasm transfer sizes, cached reload
tests/02-edit-flow.spec.ts  import 2×24MP, hi-res arrival, slider latency + long tasks, 1:1 export dims,
                            "Download N edited" = one zip with N entries (issue #1), screenshot baseline
tests/03-session.spec.ts    reload → IndexedDB restore timing
tests/04-raw.spec.ts        ARW import → camera preview → sensor-res export (skips without a RAW fixture)
tests/05-perf.spec.ts       window.__pes.perf() marks match docs/PERF.md
```

## Run
```sh
npm i && npx playwright install chromium
python3 gen_fixtures.py fixtures
# local build (serves $PES_DIST via python http.server on :8787):
PES_DIST=/path/to/repo/dist npx playwright test
# live site:
PES_BASE_URL=https://agyeiagyeiagyei.github.io/photo-edit-simplified/ npx playwright test
# first run of the screenshot test:
npx playwright test --update-snapshots
```

`PES_DIST` defaults to the repo's `dist/` (`../../dist` from this directory). The local
`dist/` must be built for serving at `/` (`trunk build --release --public-url /`);
the deploy build keeps the `/photo-edit-simplified/` prefix.

`PES_CHROMIUM` optionally points at a system Chromium instead of the Playwright
download (this container has `/usr/bin/chromium`; CI installs Playwright's).
Launch args include `--disable-dev-shm-usage` because container `/dev/shm` is small.
In a proxied environment (this agent container), run with
`NO_PROXY=127.0.0.1,localhost` or Playwright's webServer health check goes through
the outbound proxy and the local run fails with `ERR_CONNECTION_REFUSED`.

RAW fixture: put any CC0 `.ARW`/`.DNG` in `../../dl-raw/` (repo root, gitignored; or change
`RAW_DIR` in `helpers.ts`). QA uses `Sony - ILCE-5000 - 12bit compressed (3:2).ARW` from
raw.pixls.us (CC0).

`qa-run.sh <label> [url]` runs the suite `REPEAT` times (default 3) against the live site
by default, keeps each run's metrics, and prints a markdown comparison via `compare.py`
against `results-live-metrics.json` (set `BASELINE=…` to point elsewhere). It is the
QA original adapted from zsh to bash for this Linux checkout.

## Metrics emitted (names are stable; assert on them in CI)
shell_visible_ms, cached_shell_visible_ms, wasm_main_transfer_bytes, wasm_worker_transfer_bytes,
import2_to_thumbs_ms, import2_to_canvas_ms, hires_after_import_ms, slider_to_preview_ms,
hires_after_edit_ms, ten_ticks_total_ms, ten_ticks_long_tasks, export_1x1_ms, export_w/h,
zip2_ms, restore3_to_thumbs_ms, restore3_to_canvas_ms, raw_import_to_canvas_ms, raw_hires_proxy_ms, raw_export_ms.
