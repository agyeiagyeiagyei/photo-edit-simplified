//! The one parametric render pipeline: geometry → resample → develop →
//! colour. The worker, the hi-res preview, the soft-preview fallback and the
//! export all run these stages, so a stage change lands everywhere at once.
//!
//! The worker caches between `prepare` and `finish` (a slider tick copies the
//! prepared base instead of re-rotating the sensor frame); every other site
//! calls `render`.

use crate::color;
use crate::geo::geometry;
use crate::ops;
use crate::rawops;
use crate::resample;
use crate::types::EditParams;
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
    !edit.raw.is_identity() && edit.raw.denoise <= 0.0 && edit.raw.sharpen <= 0.0
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
pub fn develop(frame: &mut Frame, edit: &EditParams) -> Option<Vec<u16>> {
    if edit.raw.is_identity() {
        return None;
    }
    let mut buf = wide::widen(&frame.pixels);
    rawops::develop_wide(&mut buf, frame.w, frame.h, &edit.raw);
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
        if edit.is_color_touched() {
            color::apply_color_effects_wide(&mut buf, frame.w, frame.h, edit, seed, mask);
        }
        wide::narrow_dithered(&buf, &mut frame.pixels, frame.w, frame.h);
        return;
    }
    if !edit.is_color_touched() {
        return;
    }
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

/// Develop + colour on a prepared frame.
pub fn finish(frame: &mut Frame, edit: &EditParams, seed: u32, mask: Option<&[u8]>) {
    let wide_buf = develop(frame, edit);
    colour(frame, edit, seed, mask, wide_buf);
}

/// `finish` with the mask taken from `edit.selection`.
pub fn finish_edit(frame: &mut Frame, edit: &EditParams, seed: u32) {
    let wide_buf = develop(frame, edit);
    let mask = if edit.is_color_touched() {
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
) {
    rawops::develop_wide(buf, w, h, &edit.raw);
    if edit.is_color_touched() {
        color::apply_color_effects_wide(buf, w, h, edit, seed, mask);
    }
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
) {
    rawops::develop_neutral_wide(buf, w, h, &edit.raw, white);
    if let Some(table) = tone {
        rawops::apply_tone_wide(buf, table);
    }
    if edit.is_color_touched() {
        color::apply_color_effects_wide(buf, w, h, edit, seed, mask);
    }
}

/// `finish_neutral_wide` with the mask taken from `edit.selection`.
pub fn finish_neutral(
    frame: &mut Frame16,
    edit: &EditParams,
    seed: u32,
    white: f32,
    tone: Option<&[u16]>,
) {
    let mask = if edit.is_color_touched() {
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
    let mut frame = prepare_neutral(src, w, h, edit, target);
    finish_neutral(&mut frame, edit, seed, white, tone);
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
    finish_edit(&mut frame, edit, seed);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RawAdjust, Selection, SelectionKind};

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
    fn identity_finish_leaves_pixels_untouched() {
        let px = fixture(16, 12);
        let mut frame = Frame::new(px.clone(), 16, 12);
        finish_edit(&mut frame, &EditParams::default(), 7);
        assert_eq!(frame.pixels, px);
    }

    #[test]
    fn render_equals_prepare_then_finish() {
        let px = fixture(32, 24);
        let edit = touched_edit();
        let out = render(&px, 32, 24, &edit, Target::new(16, 12), 42);
        let mut staged = prepare(&px, 32, 24, &edit, Target::new(16, 12));
        finish(&mut staged, &edit, 42, None);
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
        finish_edit(&mut direct, &edit, 1);
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
        finish_wide(&mut buf, w, h, &banding_edit(), 1, None);
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
}
