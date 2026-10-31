# GPU parametric preview (WebGL2)

Design for WS4-C (#8). The CPU pipeline stays the export path and the parity
oracle; the GPU path only replaces the display render of whole-frame parametric
edits.

## Offscreen canvas, 2D blit

The visible `.canvas-wrap canvas` is a 2D context: layers composite with
Canvas2D and every tool overlay draws into it. A canvas cannot change context
type after first use, and moving layers/overlays onto WebGL would rewrite the
whole editor surface. The GPU renders into a detached WebGL2 canvas sized
exactly to the display backing (`need_w × need_h`, CSS px × DPR, capped at
`HI_MAX_EDGE`), then one 1:1 `drawImage` copies it into the visible canvas.
Layers and overlays draw after that, scaled from working coordinates exactly as
the crop-window path already does. This is the one deviation from "render
straight into the visible canvas": the pixels are produced at display size; only
the final copy lives on the 2D canvas.

## Base texture

`render_base` — a new worker op — returns the existing render pipeline's base:
the stored frame after geometry (rot90 + fine angle) and resample, before
`rawops` and colour. It reuses `BASE_CACHE`, so the base costs one
geometry+resample per (source, geometry, target), never per edit. The source is
whatever `render_source` picks (sensor pixels or the display proxy), so a RAW
shows GPU edits from its camera-JPEG proxy before the sensor develop lands. The
RGBA8 base (≤3200 px long edge, ~27 MB at 3200×2133) is uploaded once; slider
ticks only change uniforms.

Key: `(item, worker source id, source fingerprint, rot90, fine angle, base w, h)`.
A source swap requests a new base and keeps the old one on screen until it
lands (stale-while-revalidate).

## Shader stages

One fragment shader, one draw, over the display-sized output. Order mirrors the
CPU:

1. sample the base (bilinear), sRGB space;
2. sRGB → linear (exact piecewise, as `rawops`);
3. WB gains (`wb_gains`, computed in Rust) × exposure `2^ev`;
4. highlight shoulder (knee 0.6, exponential), skipped when 0;
5. shadow toe (`0.04·sh·(1−min(y/0.18,1))²`);
6. shadow chroma NR (luma-preserving, `t = nr·(1−min(y/0.12,1))·0.8`);
7. linear → sRGB and quantise exactly as `rawops::apply` (`floor(v·1024)`,
   1025-step encode, round to u8);
8. levels+curves: composed 256-byte per-channel LUTs (`compose_tables`) in one
   256×3 R8 texture, nearest, u8 in/out;
9. adjust: brightness/contrast/saturation/warmth on 0..255 values, same math
   and truncation as `color::adjust_rgb`;
10. grain: the CPU hash (`noise_hash`), Box–Muller, per-channel decorrelation,
    seed = item id, pixel index from `gl_FragCoord` at display resolution;
11. blur (>0 only): two separable Gaussian passes (radius `ceil(3σ)`,
    σ = amount/100·8) between the tone stage and colour, matching
    `blur::blur_apply`'s kernel; four draws total.

Cropped views keep the worker's crop-window renderer: it draws from the full
frame at native density, which a 3200 px proxy cannot match, and it is already
cheaper than a whole-frame hi render. The Crop tab itself shows the whole frame,
so it uses the GPU path. Selections, `denoise` and `sharpen` are mask/spatial
stages the shader does not implement — those edits keep the CPU path (a mask
texture is a follow-up).

## Gating and fallbacks

- Auto (default): GPU on when WebGL2 initialises **and** the renderer is
  hardware. The context is requested with `failIfMajorPerformanceCaveat`, and
  the unmasked renderer (`WEBGL_debug_renderer_info`) is checked for
  SwiftShader/llvmpipe/software names. Software rasterisers keep the CPU
  pipeline: their draw is an order of magnitude slower, and the WebGL → 2D
  blit alone measured 273 ms at 2888×1925 in the CI container (a GPU copy on
  hardware).
- `?gpu=1` forces the GPU path, `?gpu=0` forces CPU — used by the e2e suite.
- Any context/shader/link/upload failure latches the GPU off for the session;
  the existing CPU path runs untouched.

## Parity and perf gates

- e2e forces the GPU, renders the same deterministic edit (brightness,
  contrast, saturation, warmth, levels gamma and black point) on both paths and
  compares the canvases downscaled to 512 px: PSNR ≥ 45 dB. Measured 53 dB.
  Grain, blur, selections, denoise and sharpen are excluded from the parity
  edit (resolution-dependent or CPU-only).
- Per tick on hardware: `preview.render` ≤ 16 ms (the 5 ms submission target is
  reported as `gpu.render`, but that span is command submission, not GPU
  execution). Headless CI is SwiftShader, so the timing marks are recorded and
  the 16 ms assertion only runs on a hardware renderer; the structural
  assertion — no `hi.request` / `worker.render` per slider tick — runs
  everywhere.
- Memory: one base texture (≤ 27 MB) plus two display-sized targets when blur
  is active; dropped on base invalidation.
