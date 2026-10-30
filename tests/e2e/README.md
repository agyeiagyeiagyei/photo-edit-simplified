# photo-edit-simplified — QA end-to-end harness (Playwright)

Desktop Chrome, 1600×1000 @ DPR 2 (forces the hi-res render path). Every spec writes
its timings to `results/metrics.json` via `recordMetrics()`.

## Layout (drop into `tests/e2e/` in the repo)
```
package.json                @playwright/test only
playwright.config.ts        PES_BASE_URL picks local dist (default) or the live Pages URL
gen_fixtures.py             PIL script: two deterministic 24 MP JPEGs + a PNG → fixtures/ (do not commit fixtures)
gen_dng.py                  deterministic synthetic 2600×2000 RGGB DNG → dl-raw/ (CI always generates one)
tests/helpers.ts            openApp / importFiles / canvasMean / setRange / zipEntryCount / recordMetrics /
                            waitCanvasWidthAbove (rAF) / perfMarks
tests/01-smoke.spec.ts      shell load, console errors, service worker, wasm transfer sizes, cached reload
tests/02-edit-flow.spec.ts  import 2×24MP, hi-res arrival, slider latency + long tasks, cold edit,
                            1:1 export dims, "Download N edited" = one zip with N entries (issue #1),
                            screenshot baseline
tests/03-session.spec.ts    reload → IndexedDB restore timing
tests/04-raw.spec.ts        one test per RAW in dl-raw/: preview → sensor-res export (skips when none)
tests/05-perf.spec.ts       window.__pes.perf() marks match docs/PERF.md
```

## Run
```sh
npm i && npx playwright install chromium
python3 gen_fixtures.py fixtures
python3 gen_dng.py ../../dl-raw/synthetic.dng   # optional locally; CI always runs it
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

RAW fixtures: `gen_dng.py` writes a deterministic synthetic DNG (default 2600×2000) that
CI always generates, so `04-raw` never skips there. Put any real `.ARW`/`.DNG` in
`../../dl-raw/` (repo root, gitignored; or change `RAW_DIR` in `helpers.ts`) and the spec
runs one test per file. The synthetic DNG's embedded JPEG is deliberately small (650×500),
so `raw_hires_proxy_ms` measures the sensor-develop proxy path; a real ARW with a large
embedded preview exercises the camera-JPEG path instead. raw.pixls.us is best-effort in CI
only (throttled and reset-prone; no range resume), with timeouts, never required.

`qa-run.sh <label> [url]` runs the suite `REPEAT` times (default 3) against the live site
by default, keeps each run's metrics, and prints a markdown comparison via `compare.py`
against `results-live-metrics.json` (set `BASELINE=…` to point elsewhere). It is the
QA original adapted from zsh to bash for this Linux checkout.

## v3 changes (2026-10-30, after WS1 landed)
- Hi-res arrival is measured with `page.waitForFunction(..., { polling: 'raf' })` (`waitCanvasWidthAbove`), **not**
  `expect.poll`, whose 100/250/500/1000 ms intervals quantise timings and produced a false "+399 %" regression.
- `hires_after_edit_ms` is the **steady state** (first sharp frame landed before the edit) and is measured from the
  slider change (the old spec waited 400 ms first, and stamped after the ten-tick loop). The cold case — edit right
  after import, before the full-res decode — is its own test: `hires_after_edit_cold_ms`,
  `cold_edit_longest_task_ms`, `cold_edit_blocked_ms`.
- `perfMarks()` folds the app's `window.__pes.perf()` marks into metrics as `mark_<name>` (e.g. `mark_worker_render`,
  `mark_worker_render_resample`, `mark_hi_blit`); empty on builds older than WS1.
- `04-raw` parametrises over every RAW in `dl-raw/` (synthetic DNG in CI, plus any real file) and records
  `raw_fixture` with `raw_export_longest_task_ms` / `raw_export_main_thread_blocked_ms` (long tasks during export).

## Metrics emitted (names are stable; assert on them in CI)
shell_visible_ms, cached_shell_visible_ms, wasm_main_transfer_bytes, wasm_worker_transfer_bytes,
import2_to_thumbs_ms, import2_to_canvas_ms, hires_after_import_ms, slider_to_preview_ms,
hires_after_edit_ms, hires_after_edit_cold_ms, cold_edit_longest_task_ms, cold_edit_blocked_ms,
ten_ticks_total_ms, ten_ticks_long_tasks, export_1x1_ms, export_w/h,
zip2_ms, restore3_to_thumbs_ms, restore3_to_canvas_ms, raw_fixture, raw_import_to_canvas_ms, raw_hires_proxy_ms,
raw_export_ms, raw_export_longest_task_ms, raw_export_main_thread_blocked_ms, mark_*.
