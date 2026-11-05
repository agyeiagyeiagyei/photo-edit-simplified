//! The one parametric render pipeline: geometry → resample → develop →
//! colour. The worker, the hi-res preview, the soft-preview fallback and the
//! export all run these stages, so a stage change lands everywhere at once.
//!
//! The worker caches between `prepare` and `finish` (a slider tick copies the
//! prepared base instead of re-rotating the sensor frame); every other site
//! calls `render`.

use std::borrow::Cow;
use std::rc::Rc;

use crate::color;
use crate::geo::geometry;
use crate::ops;
use crate::rawops;
use crate::resample;
use crate::types::{AdjustLayer, EditParams, Selection, SelectionKind};
use crate::wide;

/// RGBA8 pixels at a known size.
pub struct Frame {
    pub pixels: Vec<u8>,
    pub w: usize,
    pub h: usize,
}

impl Frame {
    pub fn new(pixels: Vec<u8>, w: usize, h: usize) -> Self {
        debug_assert_eq!(pixels.len(), w * h * 4, "pipeline: frame size mismatch");
        Frame { pixels, w, h }
    }
}

/// A 16-bit sRGB RGBA frame: the un-toned neutral RAW develop, carried
/// through geometry and resample before the develop and camera-tone stages.
pub struct Frame16 {
    pub pixels: Vec<u16>,
    pub w: usize,
    pub h: usize,
}

impl Frame16 {
    pub fn new(pixels: Vec<u16>, w: usize, h: usize) -> Self {
        debug_assert_eq!(pixels.len(), w * h * 4, "pipeline: frame16 size mismatch");
        Frame16 { pixels, w, h }
    }
}

/// Size a render produces, as `geo::hi_target` computes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub w: usize,
    pub h: usize,
}

impl Target {
    pub fn new(w: usize, h: usize) -> Self {
        Target { w, h }
    }
}

/// Geometry + resample: the source frame as this render sees it, before
/// develop and colour.
pub fn prepare(src: &[u8], w: usize, h: usize, edit: &EditParams, target: Target) -> Frame {
    let (p, gw, gh) = geometry(src, w, h, edit);
    resize(&p, gw, gh, target)
}

/// Resample a geometry result to the render target. Split out for the
/// worker, which times geometry and resample separately around its cache.
pub fn resize(src: &[u8], w: usize, h: usize, target: Target) -> Frame {
    if (w, h) == (target.w, target.h) {
        Frame { pixels: src.to_vec(), w, h }
    } else {
        Frame {
            pixels: resample::resample(src, w, h, target.w, target.h),
            w: target.w,
            h: target.h,
        }
    }
}

/// Geometry + resample on a neutral 16-bit frame, before develop and tone.
pub fn prepare_neutral(
    src: &[u16],
    w: usize,
    h: usize,
    edit: &EditParams,
    target: Target,
) -> Frame16 {
    let (p, gw, gh) = crate::geo::geometry_wide(src, w, h, edit);
    resize_wide(&p, gw, gh, target)
}

/// Resample a neutral geometry result to the render target.
pub fn resize_wide(src: &[u16], w: usize, h: usize, target: Target) -> Frame16 {
    if (w, h) == (target.w, target.h) {
        Frame16 { pixels: src.to_vec(), w, h }
    } else {
        Frame16 {
            pixels: resample::resample_wide(src, w, h, target.w, target.h),
            w: target.w,
            h: target.h,
        }
    }
}

/// Whether a RAW edit can render from the un-toned 16-bit develop: the
/// develop is actually non-identity, and neither stage the neutral path
/// lacks (denoise, capture sharpen) is in use. Callers fall back to the
/// legacy u8 pipeline otherwise, so an edit never silently loses a stage.
pub fn neutral_eligible(edit: &EditParams) -> bool {
    // Denoise and capture sharpening run inside the neutral path (after the
    // camera tone, in `rawops::detail_toned_wide`), so they no longer push a
    // RAW back onto the legacy toned-then-develop rendering (#15).
    !edit.raw.is_identity()
}

/// Selection mask for an edit at `w`×`h`, when the edit is masked.
pub fn mask_for_dims(w: usize, h: usize, edit: &EditParams) -> Option<Vec<u8>> {
    edit.selection
        .as_ref()
        .map(|sel| ops::selection_mask(sel, w, h))
}

/// Selection mask for `edit` at `frame`'s size, when the edit is masked.
pub fn mask_for(frame: &Frame, edit: &EditParams) -> Option<Vec<u8>> {
    mask_for_dims(frame.w, frame.h, edit)
}

/// Develop stage: white balance, exposure and the lighting controls in linear
/// light, before any gamma-space colour adjustment.
///
/// A RAW edit returns the developed frame as a 16-bit sRGB buffer so colour can
/// run at that precision and the pipeline quantises to u8 once, with a dither;
/// an identity develop returns `None` and colour stays on the u8 buffer.
pub fn develop(frame: &mut Frame, edit: &EditParams, px_scale: f32) -> Option<Vec<u16>> {
    if edit.raw.is_identity() {
        return None;
    }
    let mut buf = wide::widen(&frame.pixels);
    rawops::develop_wide(&mut buf, frame.w, frame.h, &edit.raw, px_scale);
    Some(buf)
}

/// Colour stage: blurred levels/curves/adjust, then grain. `mask` is a
/// selection mask at the frame's size, or `None` for a whole-frame edit.
/// `wide_buf` is a developed 16-bit frame from [`develop`]; when present the
/// result is narrowed to the frame's u8 pixels with the final dither.
pub fn colour(
    frame: &mut Frame,
    edit: &EditParams,
    seed: u32,
    mask: Option<&[u8]>,
    wide_buf: Option<Vec<u16>>,
) {
    if let Some(mut buf) = wide_buf {
        if edit.base_color_touched() {
            color::apply_color_effects_wide(&mut buf, frame.w, frame.h, edit, seed, mask);
        }
        apply_adjust_layers_wide(&mut buf, frame.w, frame.h, edit);
        wide::narrow_dithered(&buf, &mut frame.pixels, frame.w, frame.h);
        return;
    }
    if !edit.is_color_touched() {
        return;
    }
    if edit.base_color_touched() {
        match mask {
            Some(m) => color::apply_color_effects_masked(
                &mut frame.pixels,
                m,
                frame.w,
                frame.h,
                edit,
                seed,
            ),
            None => color::apply_color_effects(&mut frame.pixels, frame.w, frame.h, edit, seed),
        }
    }
    apply_adjust_layers_u8(&mut frame.pixels, frame.w, frame.h, edit);
}

enum LayerMask<'a> {
    /// No mask: the layer applies to the whole frame.
    Whole,
    /// Mask bytes at the frame's size.
    Bytes(Cow<'a, [u8]>),
    /// The mask cannot be rebuilt (e.g. a wand raster the worker has not been
    /// sent): skip the layer rather than apply it globally.
    Skip,
}

fn layer_mask(layer: &AdjustLayer, w: usize, h: usize) -> LayerMask<'_> {
    let Some(sel) = &layer.mask else {
        return if layer.invert {
            LayerMask::Skip
        } else {
            LayerMask::Whole
        };
    };
    if let SelectionKind::Mask { data, width, height } = &sel.kind {
        if *width == 0 || *height == 0 || data.len() != width * height {
            return LayerMask::Skip;
        }
    }
    let mut m = ops::selection_mask(sel, w, h);
    if layer.invert {
        for v in &mut m {
            *v = 255 - *v;
        }
    }
    LayerMask::Bytes(Cow::Owned(m))
}

pub fn apply_adjust_layers_u8(buf: &mut [u8], w: usize, h: usize, edit: &EditParams) {
    for layer in edit.adjust_layers.iter().filter(|l| l.active()) {
        match layer_mask(layer, w, h) {
            LayerMask::Skip => {}
            LayerMask::Whole => {
                color::apply_point_adjust_u8(buf, &layer.adjust, None, layer.opacity)
            }
            LayerMask::Bytes(m) => {
                color::apply_point_adjust_u8(buf, &layer.adjust, Some(&m), layer.opacity)
            }
        }
    }
}

/// Apply every active adjustment layer to a 16-bit sRGB frame at `w`×`h`.
/// Windowed renders must rebase the masks first with [`rebase_layer_masks`].
pub fn apply_adjust_layers_wide(buf: &mut [u16], w: usize, h: usize, edit: &EditParams) {
    for layer in edit.adjust_layers.iter().filter(|l| l.active()) {
        match layer_mask(layer, w, h) {
            LayerMask::Skip => {}
            LayerMask::Whole => {
                color::apply_point_adjust_wide(buf, &layer.adjust, None, layer.opacity)
            }
            LayerMask::Bytes(m) => {
                color::apply_point_adjust_wide(buf, &layer.adjust, Some(&m), layer.opacity)
            }
        }
    }
}

fn crop_mask(p: &[u8], w: usize, cx: usize, cy: usize, cw: usize, ch: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(cw * ch);
    for y in cy..cy + ch {
        out.extend_from_slice(&p[y * w + cx..y * w + cx + cw]);
    }
    out
}

fn resample_mask_nearest(p: &[u8], w: usize, h: usize, tw: usize, th: usize) -> Vec<u8> {
    let mut out = vec![0u8; tw * th];
    for y in 0..th {
        let sy = y * h / th;
        for x in 0..tw {
            out[y * tw + x] = p[sy * w + x * w / tw];
        }
    }
    out
}

/// Rebase each layer's mask from working-frame coordinates into the rendered
/// window `rect` at `tw`×`th`. A windowed render must rasterize the mask over
/// the full working frame (normalized selection coordinates refer to it), take
/// the rect, then rescale — the same treatment the global selection gets.
pub fn rebase_layer_masks(
    edit: &EditParams,
    work_w: usize,
    work_h: usize,
    rect: (usize, usize, usize, usize),
    tw: usize,
    th: usize,
) -> EditParams {
    if edit.adjust_layers.is_empty() || work_w == 0 || work_h == 0 || tw == 0 || th == 0 {
        return edit.clone();
    }
    let (cx, cy, cw, ch) = rect;
    if cw == 0 || ch == 0 || cx + cw > work_w || cy + ch > work_h {
        return edit.clone();
    }
    let mut out = edit.clone();
    for layer in &mut out.adjust_layers {
        let Some(sel) = layer.mask.take() else {
            if layer.invert {
                layer.visible = false;
            }
            continue;
        };
        let mut full = ops::selection_mask(&sel, work_w, work_h);
        if layer.invert {
            for v in &mut full {
                *v = 255 - *v;
            }
        }
        let win = crop_mask(&full, work_w, cx, cy, cw, ch);
        let mask = if (tw, th) != (cw, ch) {
            resample_mask_nearest(&win, cw, ch, tw, th)
        } else {
            win
        };
        layer.mask = Some(Selection {
            kind: SelectionKind::Mask {
                data: Rc::new(mask),
                width: tw,
                height: th,
            },
            feather: 0.0,
        });
        layer.invert = false;
    }
    out
}

/// Develop + colour on a prepared frame.
pub fn finish(
    frame: &mut Frame,
    edit: &EditParams,
    seed: u32,
    mask: Option<&[u8]>,
    px_scale: f32,
) {
    let wide_buf = develop(frame, edit, px_scale);
    colour(frame, edit, seed, mask, wide_buf);
}

/// `finish` with the mask taken from `edit.selection`.
pub fn finish_edit(frame: &mut Frame, edit: &EditParams, seed: u32, px_scale: f32) {
    let wide_buf = develop(frame, edit, px_scale);
    let mask = if edit.base_color_touched() {
        mask_for(frame, edit)
    } else {
        None
    };
    colour(frame, edit, seed, mask.as_deref(), wide_buf);
}

/// The whole stage set on a 16-bit sRGB RGBA frame, for callers that own the
/// final quantisation (the worker and the export narrow with
/// [`wide::narrow_dithered`] once).
pub fn finish_wide(
    buf: &mut [u16],
    w: usize,
    h: usize,
    edit: &EditParams,
    seed: u32,
    mask: Option<&[u8]>,
    px_scale: f32,
) {
    rawops::develop_wide(buf, w, h, &edit.raw, px_scale);
    if edit.base_color_touched() {
        color::apply_color_effects_wide(buf, w, h, edit, seed, mask);
    }
    apply_adjust_layers_wide(buf, w, h, edit);
}

/// Neutral develop (white-relative, on unclipped data), then the camera tone
/// curve, then colour. `tone` is the table from [`rawops::tone_table`].
pub fn finish_neutral_wide(
    buf: &mut [u16],
    w: usize,
    h: usize,
    edit: &EditParams,
    seed: u32,
    mask: Option<&[u8]>,
    white: f32,
    tone: Option<&[u16]>,
    px_scale: f32,
) {
    rawops::develop_neutral_wide(buf, w, h, &edit.raw, white);
    if let Some(table) = tone {
        rawops::apply_tone_wide(buf, table);
    }
    rawops::detail_toned_wide(buf, w, h, &edit.raw, px_scale);
    if edit.base_color_touched() {
        color::apply_color_effects_wide(buf, w, h, edit, seed, mask);
    }
    apply_adjust_layers_wide(buf, w, h, edit);
}

/// `finish_neutral_wide` with the mask taken from `edit.selection`.
pub fn finish_neutral(
    frame: &mut Frame16,
    edit: &EditParams,
    seed: u32,
    white: f32,
    tone: Option<&[u16]>,
    px_scale: f32,
) {
    let mask = if edit.base_color_touched() {
        mask_for_dims(frame.w, frame.h, edit)
    } else {
        None
    };
    finish_neutral_wide(
        &mut frame.pixels,
        frame.w,
        frame.h,
        edit,
        seed,
        mask.as_deref(),
        white,
        tone,
        px_scale,
    );
}

/// The neutral pipeline for a source frame rendered to `target`.
pub fn render_neutral(
    src: &[u16],
    w: usize,
    h: usize,
    edit: &EditParams,
    target: Target,
    seed: u32,
    white: f32,
    tone: Option<&[u16]>,
) -> Frame16 {
    let px_scale = px_scale_for(w, h, target);
    let mut frame = prepare_neutral(src, w, h, edit, target);
    finish_neutral(&mut frame, edit, seed, white, tone, px_scale);
    frame
}

/// The whole pipeline for a source frame rendered to `target`.
pub fn render(
    src: &[u8],
    w: usize,
    h: usize,
    edit: &EditParams,
    target: Target,
    seed: u32,
) -> Frame {
    let mut frame = prepare(src, w, h, edit, target);
    let px_scale = px_scale_for(w, h, target);
    finish_edit(&mut frame, edit, seed, px_scale);
    frame
}

/// Sensor pixels per rendered pixel, for stages whose radius is in sensor
/// pixels (capture sharpening).
pub fn px_scale_for(w: usize, h: usize, target: Target) -> f32 {
    if w == 0 || h == 0 || target.w == 0 || target.h == 0 {
        return 1.0;
    }
    (((w * h) as f64) / ((target.w * target.h) as f64)).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AdjustLayer, PointAdjust, RawAdjust, Selection, SelectionKind};

    fn fixture(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                px[i] = (x * 7 + y * 3) as u8;
                px[i + 1] = (x * 3 + y * 11) as u8;
                px[i + 2] = (x * 13 + y * 5) as u8;
                px[i + 3] = 255;
            }
        }
        px
    }

    fn touched_edit() -> EditParams {
        let mut edit = EditParams::default();
        edit.brightness = 0.2;
        edit.contrast = 0.1;
        edit.saturation = -0.15;
        edit.raw = RawAdjust { exposure: 0.3, highlights: 40.0, ..RawAdjust::default() };
        edit
    }

    #[test]
    fn px_scale_tracks_source_to_target_area() {
        let half = px_scale_for(6192, 4128, Target::new(3096, 2064));
        assert!((half - 2.0).abs() < 1e-4, "half-size scale = {half}");
        assert!((px_scale_for(40, 30, Target::new(40, 30)) - 1.0).abs() < 1e-6);
        assert_eq!(px_scale_for(0, 10, Target::new(5, 5)), 1.0);
    }

    #[test]
    fn identity_finish_leaves_pixels_untouched() {
        let px = fixture(16, 12);
        let mut frame = Frame::new(px.clone(), 16, 12);
        finish_edit(&mut frame, &EditParams::default(), 7, 1.0);
        assert_eq!(frame.pixels, px);
    }

    #[test]
    fn render_equals_prepare_then_finish() {
        let px = fixture(32, 24);
        let edit = touched_edit();
        let out = render(&px, 32, 24, &edit, Target::new(16, 12), 42);
        let mut staged = prepare(&px, 32, 24, &edit, Target::new(16, 12));
        finish(&mut staged, &edit, 42, None, 1.0);
        assert_eq!(out.pixels, staged.pixels);
        assert_eq!((out.w, out.h), (16, 12));
    }

    #[test]
    fn native_target_skips_the_resampler() {
        let px = fixture(20, 10);
        let mut edit = EditParams::default();
        edit.contrast = 0.2;
        let out = render(&px, 20, 10, &edit, Target::new(20, 10), 1);
        let mut direct = Frame::new(px.clone(), 20, 10);
        finish_edit(&mut direct, &edit, 1, 1.0);
        assert_eq!(out.pixels, direct.pixels);
    }

    /// 16-bit sRGB ramp over the first quarter of the range, so +2 EV and a
    /// full shadow lift produce a very slow 8-bit gradient after encoding.
    fn ramp16(w: usize, h: usize) -> Vec<u16> {
        let mut buf = Vec::with_capacity(w * h * 4);
        for _ in 0..h {
            for x in 0..w {
                let v = (x as u32 * 16383 / (w as u32 - 1)) as u16;
                buf.extend_from_slice(&[v, v, v, 65535]);
            }
        }
        buf
    }

    fn longest_run(row: &[u8]) -> usize {
        let (mut best, mut run) = (1usize, 1usize);
        for i in 1..row.len() {
            if row[i] == row[i - 1] {
                run += 1;
                best = best.max(run);
            } else {
                run = 1;
            }
        }
        best
    }

    fn banding_edit() -> EditParams {
        EditParams {
            raw: RawAdjust { exposure: 2.0, shadows: 100.0, ..RawAdjust::default() },
            ..EditParams::default()
        }
    }

    fn assert_dithered_ramp(w: usize, h: usize) {
        let mut buf = ramp16(w, h);
        finish_wide(&mut buf, w, h, &banding_edit(), 1, None, 1.0);
        let mut out = vec![0u8; w * h * 4];
        wide::narrow_dithered(&buf, &mut out, w, h);
        let row: Vec<u8> = out[..w * 4].chunks_exact(4).map(|p| p[0]).collect();
        let step = row
            .windows(2)
            .map(|p| (p[0] as i32 - p[1] as i32).abs())
            .max()
            .unwrap();
        assert!(step <= 1, "adjacent-band step {step} at w={w}");
        let mut plain = vec![0u8; w * h * 4];
        wide::narrow_plain(&buf, &mut plain);
        let plain: Vec<u8> = plain[..w * 4].chunks_exact(4).map(|p| p[0]).collect();
        let transitions = |r: &[u8]| r.windows(2).filter(|p| p[0] != p[1]).count();
        // The dither is what turns each code plateau into texture: the plain
        // quantisation is flat for over a hundred pixels, the dithered row is
        // not, and it flips values far more often inside every band.
        assert!(
            longest_run(&row) * 2 <= longest_run(&plain),
            "no dither texture: {} vs {}",
            longest_run(&row),
            longest_run(&plain)
        );
        assert!(
            transitions(&row) > 10 * transitions(&plain),
            "dither too weak: {} vs {}",
            transitions(&row),
            transitions(&plain)
        );
    }

    #[test]
    fn banding_ramp_is_dithered() {
        assert_dithered_ramp(4096, 8);
    }

    #[test]
    #[ignore = "24 MP fixture; run with --release"]
    fn banding_ramp_24mp_is_dithered() {
        assert_dithered_ramp(6000, 4000);
    }

    fn tone_curve() -> [u8; 256] {
        // Monotone lift: the camera curve a dark neutral develop needs.
        let mut lut = [0u8; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = (10 + i * 235 / 255) as u8;
        }
        lut
    }

    #[test]
    fn identity_neutral_with_tone_matches_the_toned_frame() {
        let (w, h) = (64usize, 8usize);
        let untoned: Vec<u8> = (0..w * h * 4)
            .map(|i| match i % 4 {
                0 => (i / 4 % w * 255 / (w - 1)) as u8,
                1 => ((i / 4 * 7) % 256) as u8,
                2 => 40,
                _ => 255,
            })
            .collect();
        let lut = tone_curve();
        let toned: Vec<u8> = untoned
            .chunks_exact(4)
            .flat_map(|p| [lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize], p[3]])
            .collect();
        let mut buf: Vec<u16> = untoned.iter().map(|&v| v as u16 * 257).collect();
        let table = rawops::tone_table(&lut);
        finish_neutral_wide(
            &mut buf,
            w,
            h,
            &EditParams::default(),
            1,
            None,
            1.0,
            Some(&table),
            1.0,
        );
        let mut out = vec![0u8; w * h * 4];
        wide::narrow_dithered(&buf, &mut out, w, h);
        let max = out.iter().zip(&toned).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
        assert!(max <= 1, "tone stage moved a pixel by {max} codes");
    }

    #[test]
    fn neutral_develop_scales_thresholds_by_white() {
        let (w, h) = (8usize, 8usize);
        let v = (0.9 * 65535.0) as u16;
        let frame = vec![v; w * h * 4];
        let adj = RawAdjust { highlights: 100.0, ..RawAdjust::default() };
        let mut less = frame.clone();
        rawops::develop_neutral_wide(&mut less, w, h, &adj, 0.8);
        let mut more = frame.clone();
        rawops::develop_neutral_wide(&mut more, w, h, &adj, 0.3);
        assert!(less[0] < v, "highlight recovery did not act at all");
        assert!(
            less[0] > more[0],
            "smaller white must compress more: {} vs {}",
            less[0],
            more[0]
        );
    }

    #[test]
    fn tone_table_is_monotone_and_anchored() {
        let lut = tone_curve();
        let table = rawops::tone_table(&lut);
        assert_eq!(table.len(), 65536);
        assert_eq!(table[0], lut[0] as u16 * 257);
        assert_eq!(table[65535], lut[255] as u16 * 257);
        assert!(table.windows(2).all(|p| p[0] <= p[1]));
    }

    #[test]
    fn masked_finish_only_touches_the_selection() {
        let px = fixture(40, 30);
        // Develop is global; only the colour stage honours the selection.
        let mut edit = EditParams::default();
        edit.brightness = 0.25;
        edit.contrast = 0.2;
        edit.selection = Some(Selection {
            kind: SelectionKind::Rect { x: 0.0, y: 0.0, w: 0.5, h: 1.0 },
            feather: 0.0,
        });
        let out = render(&px, 40, 30, &edit, Target::new(40, 30), 3);
        // Outside the selection the frame must be the untouched source.
        for y in 0..30 {
            for x in 20..40 {
                let i = (y * 40 + x) * 4;
                assert_eq!(&out.pixels[i..i + 4], &px[i..i + 4], "pixel ({x},{y}) moved");
            }
        }
    }

    /// Textured foliage-like fixture: a saturated green field with
    /// per-channel noise and a few hard edges, in the un-toned neutral range.
    fn neutral_foliage(w: usize, h: usize) -> Vec<u16> {
        let mut v = vec![0u16; w * h * 4];
        let mut s = 99u32;
        for y in 0..h {
            for x in 0..w {
                let base: [i32; 3] = if (x / 16 + y / 16) % 2 == 0 { [70, 150, 45] } else { [40, 95, 30] };
                for c in 0..3 {
                    s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    let d = ((s >> 16) as i32 % 13) - 6;
                    v[(y * w + x) * 4 + c] = ((base[c] + d).clamp(0, 255) as u16) * 257;
                }
                v[(y * w + x) * 4 + 3] = 65535;
            }
        }
        v
    }

    fn render_neutral_means(raw: RawAdjust) -> [f64; 3] {
        let (w, h) = (128usize, 96usize);
        let src = neutral_foliage(w, h);
        let lut = tone_curve();
        let table = rawops::tone_table(&lut);
        let edit = EditParams { raw, ..EditParams::default() };
        let frame = render_neutral(&src, w, h, &edit, Target::new(w, h), 1, 1.0, Some(&table));
        let mut out = vec![0u8; w * h * 4];
        wide::narrow_dithered(&frame.pixels, &mut out, w, h);
        let n = (w * h) as f64;
        let mut m = [0f64; 3];
        for p in out.chunks_exact(4) {
            for c in 0..3 {
                m[c] += p[c] as f64;
            }
        }
        m.map(|x| x / n)
    }

    #[test]
    fn denoise_and_sharpen_keep_the_neutral_path() {
        let mut edit = EditParams::default();
        edit.raw.denoise = 10.0;
        assert!(neutral_eligible(&edit), "denoise alone must render through the neutral path");
        edit.raw.denoise = 0.0;
        edit.raw.sharpen = 10.0;
        assert!(neutral_eligible(&edit), "sharpening alone must render through the neutral path");
        assert!(!neutral_eligible(&EditParams::default()));
    }

    #[test]
    fn denoise_and_sharpen_are_tone_neutral_in_the_neutral_path() {
        // #15: turning denoise on shifted the median ~+9 codes as shot and
        // reshaped the curve under an exposure lift. Means must stay put.
        for exposure in [0.0f32, 1.5] {
            let base = RawAdjust { exposure, tint: 1.0, ..RawAdjust::default() };
            let reference = render_neutral_means(base);
            for (denoise, sharpen) in [(10.0f32, 0.0f32), (50.0, 0.0), (0.0, 30.0), (50.0, 30.0)] {
                let got = render_neutral_means(RawAdjust { denoise, sharpen, ..base });
                for c in 0..3 {
                    assert!(
                        (got[c] - reference[c]).abs() <= 1.0,
                        "exposure {exposure}, denoise {denoise}, sharpen {sharpen}: channel {c} mean {:.2} vs {:.2}",
                        got[c],
                        reference[c]
                    );
                }
            }
        }
    }

    fn point_edit(brightness: f32, contrast: f32) -> PointAdjust {
        PointAdjust { brightness, contrast, ..PointAdjust::default() }
    }

    fn adjust_layer(adjust: PointAdjust, mask: Option<Selection>) -> AdjustLayer {
        AdjustLayer {
            id: 1,
            visible: true,
            opacity: 1.0,
            mask,
            invert: false,
            mask_version: 1,
            adjust,
        }
    }

    fn only_layer(layer: AdjustLayer) -> EditParams {
        EditParams { adjust_layers: vec![layer], ..EditParams::default() }
    }

    fn with_luts(mut adj: PointAdjust) -> PointAdjust {
        let mut levels = crate::levels::LevelsSettings::default();
        levels.ranges[0].gamma = 0.85;
        levels.ranges[1].black = 12.0;
        levels.ranges[1].white = 240.0;
        let mut curves = crate::curves::CurvesSettings::default();
        curves.channels[0] = vec![
            crate::curves::CurvePoint { x: 0.0, y: 0.0 },
            crate::curves::CurvePoint { x: 128.0, y: 150.0 },
            crate::curves::CurvePoint { x: 255.0, y: 255.0 },
        ];
        adj.levels = levels;
        adj.curves = curves;
        adj
    }

    #[test]
    fn rect_masked_adjust_layer_only_touches_inside_the_mask() {
        let (w, h) = (40usize, 30usize);
        let px = fixture(w, h);
        let edit = only_layer(adjust_layer(
            point_edit(0.25, 0.1),
            Some(Selection {
                kind: SelectionKind::Rect { x: 0.0, y: 0.0, w: 0.5, h: 1.0 },
                feather: 0.0,
            }),
        ));
        let mut applied = px.clone();
        color::apply_point_adjust_u8(&mut applied, &point_edit(0.25, 0.1), None, 1.0);
        assert_ne!(applied, px, "test adjustment is a no-op");
        let out = render(&px, w, h, &edit, Target::new(w, h), 3);
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                if x < w / 2 {
                    assert_eq!(&out.pixels[i..i + 4], &applied[i..i + 4], "inside pixel ({x},{y}) wrong");
                } else {
                    assert_eq!(&out.pixels[i..i + 4], &px[i..i + 4], "outside pixel ({x},{y}) moved");
                }
            }
        }
    }

    #[test]
    fn inverted_adjust_layer_acts_outside_the_mask() {
        let (w, h) = (40usize, 30usize);
        let px = fixture(w, h);
        let mut layer = adjust_layer(
            point_edit(0.25, 0.0),
            Some(Selection {
                kind: SelectionKind::Rect { x: 0.0, y: 0.0, w: 0.5, h: 1.0 },
                feather: 0.0,
            }),
        );
        layer.invert = true;
        let mut applied = px.clone();
        color::apply_point_adjust_u8(&mut applied, &point_edit(0.25, 0.0), None, 1.0);
        assert_ne!(applied, px, "test adjustment is a no-op");
        let out = render(&px, w, h, &only_layer(layer), Target::new(w, h), 3);
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                if x >= w / 2 {
                    assert_eq!(&out.pixels[i..i + 4], &applied[i..i + 4], "outside pixel ({x},{y}) wrong");
                } else {
                    assert_eq!(&out.pixels[i..i + 4], &px[i..i + 4], "inside pixel ({x},{y}) moved");
                }
            }
        }
    }

    #[test]
    fn inactive_or_unresolvable_adjust_layers_are_identity() {
        let (w, h) = (16usize, 12usize);
        let px = fixture(w, h);

        let mut invisible = only_layer(adjust_layer(point_edit(0.4, 0.2), None));
        invisible.adjust_layers[0].visible = false;
        assert!(!invisible.is_color_touched());
        assert!(!invisible.base_color_touched());

        let mut zero_opacity = only_layer(adjust_layer(point_edit(0.4, 0.2), None));
        zero_opacity.adjust_layers[0].opacity = 0.0;

        let identity = only_layer(adjust_layer(PointAdjust::default(), None));

        // A Mask whose dims disagree with its bytes is an unresolved worker
        // reference: skip the layer rather than apply it to the whole frame.
        let mut unresolved = only_layer(adjust_layer(
            point_edit(0.4, 0.2),
            Some(Selection {
                kind: SelectionKind::Mask {
                    data: Rc::new(Vec::new()),
                    width: 10,
                    height: 10,
                },
                feather: 0.0,
            }),
        ));
        unresolved.adjust_layers[0].mask_version = 2;

        for edit in [invisible, zero_opacity, identity, unresolved] {
            assert_eq!(
                render(&px, w, h, &edit, Target::new(w, h), 5).pixels,
                px,
                "layer should not change pixels"
            );
        }
    }

    #[test]
    fn active_adjust_layer_counts_as_a_color_edit() {
        let mut edit = only_layer(adjust_layer(point_edit(0.2, 0.0), None));
        assert!(edit.is_color_touched());
        assert!(!edit.base_color_touched());
        assert!(!edit.develop_only().is_color_touched());
        edit.adjust_layers[0].adjust = PointAdjust::default();
        assert!(!edit.is_color_touched());
    }

    #[test]
    fn adjust_layers_apply_bottom_first() {
        let (w, h) = (32usize, 24usize);
        let px = fixture(w, h);
        let contrast = adjust_layer(point_edit(0.0, 0.4), None);
        let brightness = adjust_layer(point_edit(0.2, 0.0), None);
        let mut forward = only_layer(contrast.clone());
        forward.adjust_layers.push(brightness.clone());
        let mut reverse = only_layer(brightness);
        reverse.adjust_layers.push(contrast);

        let mut expect = px.clone();
        color::apply_point_adjust_u8(&mut expect, &forward.adjust_layers[0].adjust, None, 1.0);
        color::apply_point_adjust_u8(&mut expect, &forward.adjust_layers[1].adjust, None, 1.0);
        let out = render(&px, w, h, &forward, Target::new(w, h), 9).pixels;
        assert_eq!(out, expect, "stack order is not applied bottom first");

        let reversed = render(&px, w, h, &reverse, Target::new(w, h), 9).pixels;
        assert_ne!(reversed, out, "order-dependent layers rendered identically");
    }

    #[test]
    fn full_frame_adjust_layer_matches_the_global_stage() {
        let (w, h) = (48usize, 36usize);
        let px = fixture(w, h);
        let adj = with_luts(PointAdjust {
            brightness: 0.12,
            contrast: 0.2,
            saturation: -0.15,
            warmth: 0.1,
            ..PointAdjust::default()
        });

        let global = EditParams {
            brightness: adj.brightness,
            contrast: adj.contrast,
            saturation: adj.saturation,
            warmth: adj.warmth,
            levels: adj.levels,
            curves: adj.curves.clone(),
            ..EditParams::default()
        };
        let want = render(&px, w, h, &global, Target::new(w, h), 11).pixels;
        let unmasked = only_layer(adjust_layer(adj.clone(), None));
        assert_eq!(render(&px, w, h, &unmasked, Target::new(w, h), 11).pixels, want);

        let full_mask = only_layer(adjust_layer(
            adj,
            Some(Selection {
                kind: SelectionKind::Mask {
                    data: Rc::new(vec![255u8; w * h]),
                    width: w,
                    height: h,
                },
                feather: 0.0,
            }),
        ));
        assert_eq!(render(&px, w, h, &full_mask, Target::new(w, h), 11).pixels, want);
    }

    #[test]
    fn full_frame_adjust_layer_matches_the_global_stage_wide() {
        let (w, h) = (32usize, 24usize);
        let px = fixture(w, h);
        let adj = with_luts(PointAdjust {
            brightness: -0.08,
            contrast: 0.3,
            saturation: 0.2,
            warmth: -0.1,
            ..PointAdjust::default()
        });
        let global = EditParams {
            brightness: adj.brightness,
            contrast: adj.contrast,
            saturation: adj.saturation,
            warmth: adj.warmth,
            levels: adj.levels,
            curves: adj.curves.clone(),
            ..EditParams::default()
        };
        let mut a: Vec<u16> = px.iter().map(|&v| v as u16 * 257).collect();
        finish_wide(&mut a, w, h, &global, 4, None, 1.0);
        let mut b: Vec<u16> = px.iter().map(|&v| v as u16 * 257).collect();
        finish_wide(&mut b, w, h, &only_layer(adjust_layer(adj, None)), 4, None, 1.0);
        assert_eq!(a, b);
    }

    fn crop_pixels(p: &[u8], w: usize, cx: usize, cy: usize, cw: usize, ch: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(cw * ch * 4);
        for y in cy..cy + ch {
            out.extend_from_slice(&p[(y * w + cx) * 4..(y * w + cx + cw) * 4]);
        }
        out
    }

    #[test]
    fn rebased_layer_mask_window_equals_the_full_frame_crop() {
        let (w, h) = (40usize, 30usize);
        let (cx, cy, cw, ch) = (10usize, 6usize, 20usize, 12usize);
        let px = fixture(w, h);
        let edit = only_layer(adjust_layer(
            point_edit(0.18, 0.15),
            Some(Selection {
                kind: SelectionKind::Rect { x: 0.2, y: 0.1, w: 0.5, h: 0.7 },
                feather: 0.0,
            }),
        ));

        let full = render(&px, w, h, &edit, Target::new(w, h), 2).pixels;
        let want = crop_pixels(&full, w, cx, cy, cw, ch);

        let rebased = rebase_layer_masks(&edit, w, h, (cx, cy, cw, ch), cw, ch);
        let mask = rebased.adjust_layers[0].mask.as_ref().unwrap();
        match &mask.kind {
            SelectionKind::Mask { data, width, height } => {
                assert_eq!((data.len(), *width, *height), (cw * ch, cw, ch));
            }
            other => panic!("rebased mask kind {other:?}"),
        }
        assert_eq!(mask.feather, 0.0);
        assert!(!rebased.adjust_layers[0].invert);

        let src = crop_pixels(&px, w, cx, cy, cw, ch);
        let mut win = Frame::new(src, cw, ch);
        finish_edit(&mut win, &rebased, 2, 1.0);
        assert_eq!(win.pixels, want);
    }

    #[test]
    fn all_zero_mask_is_bit_identical_to_no_layer() {
        let (w, h) = (24usize, 18usize);
        let px = fixture(w, h);
        let edit = only_layer(adjust_layer(
            point_edit(0.35, 0.2),
            Some(Selection {
                kind: SelectionKind::Mask {
                    data: Rc::new(vec![0u8; w * h]),
                    width: w,
                    height: h,
                },
                feather: 0.0,
            }),
        ));
        assert!(edit.is_color_touched());
        assert_eq!(render(&px, w, h, &edit, Target::new(w, h), 6).pixels, px);
    }

    fn box_downscale(p: &[u8], w: usize, h: usize, f: usize) -> (Vec<u8>, usize, usize) {
        let (tw, th) = (w / f, h / f);
        let mut out = vec![0u8; tw * th * 4];
        for y in 0..th {
            for x in 0..tw {
                for c in 0..4 {
                    let mut sum = 0u32;
                    for dy in 0..f {
                        for dx in 0..f {
                            sum += p[((y * f + dy) * w + x * f + dx) * 4 + c] as u32;
                        }
                    }
                    out[(y * tw + x) * 4 + c] = (sum / (f * f) as u32) as u8;
                }
            }
        }
        (out, tw, th)
    }

    #[test]
    fn feathered_mask_proxy_matches_full_resolution() {
        let (w, h) = (256usize, 192usize);
        let px = fixture(w, h);
        let edit = only_layer(adjust_layer(
            point_edit(0.2, 0.12),
            Some(Selection {
                kind: SelectionKind::Rect { x: 0.2, y: 0.2, w: 0.6, h: 0.6 },
                feather: 0.05,
            }),
        ));
        let full = render(&px, w, h, &edit, Target::new(w, h), 8).pixels;
        let (small, sw, sh) = box_downscale(&px, w, h, 2);
        let proxy = render(&small, sw, sh, &edit, Target::new(sw, sh), 8).pixels;
        let (want, _, _) = box_downscale(&full, w, h, 2);
        let mean = want
            .chunks_exact(4)
            .zip(proxy.chunks_exact(4))
            .map(|(a, b)| (0..3).map(|c| (a[c] as i32 - b[c] as i32).abs()).sum::<i32>() as f64 / 3.0)
            .sum::<f64>()
            / (sw * sh) as f64;
        assert!(mean <= 2.0, "proxy/full masked mean ΔRGB {mean:.2} > 2.0");
    }

    #[test]
    fn rebase_handles_invert_and_whole_frame_layers() {
        let mut inverted = adjust_layer(
            point_edit(0.2, 0.0),
            Some(Selection {
                kind: SelectionKind::Rect { x: 0.5, y: 0.0, w: 0.5, h: 1.0 },
                feather: 0.0,
            }),
        );
        inverted.invert = true;
        let mut whole = adjust_layer(point_edit(0.2, 0.0), None);
        whole.invert = true;
        let edit = EditParams { adjust_layers: vec![inverted, whole], ..EditParams::default() };
        let out = rebase_layer_masks(&edit, 40, 30, (0, 0, 20, 30), 20, 30);
        assert!(!out.adjust_layers[0].invert, "invert must be baked into the mask");
        let SelectionKind::Mask { data, .. } = &out.adjust_layers[0].mask.as_ref().unwrap().kind else {
            panic!("expected a baked mask");
        };
        // Right half was selected then inverted: the left half now carries it.
        assert!(data.iter().take(10).all(|&v| v == 255));
        assert!(!out.adjust_layers[1].visible, "an inverted no-mask layer covers nothing");
    }
}
