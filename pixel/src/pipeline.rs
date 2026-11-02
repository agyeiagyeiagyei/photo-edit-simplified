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

/// Selection mask for `edit` at `frame`'s size, when the edit is masked.
pub fn mask_for(frame: &Frame, edit: &EditParams) -> Option<Vec<u8>> {
    edit.selection
        .as_ref()
        .map(|sel| ops::selection_mask(sel, frame.w, frame.h))
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
