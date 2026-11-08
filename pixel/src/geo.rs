//! Geometry pipeline shared by app and worker: rotate/crop math, target
//! sizing, stroke rasterization. Resampling after geometry lives in
//! [`crate::resample`], driven by [`crate::pipeline`].

use std::borrow::Cow;

use crate::ops;
use crate::types::EditParams;

/// Apply rotation (quarter-turns then fine angle with auto-crop) to a buffer.
/// The identity case borrows the source instead of copying it; callers that
/// need to mutate convert with `to_mut`/`into_owned`.
pub fn geometry<'a>(
    pixels: &'a [u8],
    w: usize,
    h: usize,
    edit: &EditParams,
) -> (Cow<'a, [u8]>, usize, usize) {
    let (p, w, h): (Cow<'a, [u8]>, usize, usize) = if !edit.rot90.is_multiple_of(4) {
        let (v, w, h) = ops::rotate_90(pixels, w, h, edit.rot90);
        (Cow::Owned(v), w, h)
    } else {
        (Cow::Borrowed(pixels), w, h)
    };
    if edit.fine_angle.abs() > 0.01 {
        let (v, w, h) = ops::rotate_auto_crop(&p, w, h, edit.fine_angle);
        (Cow::Owned(v), w, h)
    } else {
        (p, w, h)
    }
}

/// 16-bit counterpart of [`geometry`] for the neutral RAW frame. Same math;
/// the identity case borrows the source.
pub fn geometry_wide<'a>(
    pixels: &'a [u16],
    w: usize,
    h: usize,
    edit: &EditParams,
) -> (Cow<'a, [u16]>, usize, usize) {
    let (p, w, h): (Cow<'a, [u16]>, usize, usize) = if !edit.rot90.is_multiple_of(4) {
        let (v, w, h) = ops::rotate_90_wide(pixels, w, h, edit.rot90);
        (Cow::Owned(v), w, h)
    } else {
        (Cow::Borrowed(pixels), w, h)
    };
    if edit.fine_angle.abs() > 0.01 {
        let (v, w, h) = ops::rotate_auto_crop_wide(&p, w, h, edit.fine_angle);
        (Cow::Owned(v), w, h)
    } else {
        (p, w, h)
    }
}

/// Dimensions after geometry() without running it.
pub fn working_dims(w: usize, h: usize, edit: &EditParams) -> (usize, usize) {
    let (w, h) = if edit.rot90 % 2 == 1 { (h, w) } else { (w, h) };
    if edit.fine_angle.abs() > 0.01 {
        let a = edit.fine_angle.to_radians().abs();
        let (sin, cos) = (a.sin(), a.cos());
        let bw = w as f32 * cos + h as f32 * sin;
        let bh = w as f32 * sin + h as f32 * cos;
        let x1 = (w * w) as f32 / (2.0 * bw);
        let x2 = (w * h) as f32 / (2.0 * bh);
        let half = x1.min(x2);
        (
            (half * 2.0).floor().max(1.0) as usize,
            (half * 2.0 * h as f32 / w as f32).floor().max(1.0) as usize,
        )
    } else {
        (w, h)
    }
}

/// Quarter-turn mapping in normalized coords: original image → rotated frame.
fn rot90_norm_fwd(p: (f32, f32), k: u8) -> (f32, f32) {
    match k % 4 {
        1 => (1.0 - p.1, p.0),
        2 => (1.0 - p.0, 1.0 - p.1),
        3 => (p.1, 1.0 - p.0),
        _ => p,
    }
}

/// Inverse of [`rot90_norm_fwd`]: rotated frame → original image.
fn rot90_norm_inv(p: (f32, f32), k: u8) -> (f32, f32) {
    match k % 4 {
        1 => (p.1, 1.0 - p.0),
        2 => (1.0 - p.0, 1.0 - p.1),
        3 => (1.0 - p.1, p.0),
        _ => p,
    }
}

/// Map an original-image normalized point to the working-frame normalized
/// space that UI strokes are captured in (rot90, then fine-angle auto-crop).
pub fn work_from_orig_norm(
    p: (f32, f32),
    fw: usize,
    fh: usize,
    edit: &EditParams,
) -> (f32, f32) {
    let (rx, ry) = rot90_norm_fwd(p, edit.rot90);
    if edit.fine_angle.abs() <= 0.01 {
        return (rx, ry);
    }
    let (rw, rh) = if edit.rot90 % 2 == 1 { (fh, fw) } else { (fw, fh) };
    let (ww, wh) = working_dims(fw, fh, edit);
    let a = edit.fine_angle.to_radians();
    let (sn, cs) = a.sin_cos();
    let dx = (rx - 0.5) * rw as f32;
    let dy = (ry - 0.5) * rh as f32;
    (
        0.5 + (dx * cs - dy * sn) / ww as f32,
        0.5 + (dx * sn + dy * cs) / wh as f32,
    )
}

/// Map a working-frame normalized point back to the original image through
/// the geometry. Inverse of [`work_from_orig_norm`].
pub fn orig_from_work_norm(
    p: (f32, f32),
    fw: usize,
    fh: usize,
    edit: &EditParams,
) -> (f32, f32) {
    let (rx, ry) = if edit.fine_angle.abs() > 0.01 {
        let (rw, rh) = if edit.rot90 % 2 == 1 { (fh, fw) } else { (fw, fh) };
        let (ww, wh) = working_dims(fw, fh, edit);
        let a = edit.fine_angle.to_radians();
        let (sn, cs) = a.sin_cos();
        let dx = (p.0 - 0.5) * ww as f32;
        let dy = (p.1 - 0.5) * wh as f32;
        (
            0.5 + (dx * cs + dy * sn) / rw as f32,
            0.5 + (-dx * sn + dy * cs) / rh as f32,
        )
    } else {
        p
    };
    rot90_norm_inv((rx, ry), edit.rot90)
}

/// Crop rect in pixels for an image of (w, h).
pub fn crop_px(edit: &EditParams, w: usize, h: usize) -> (usize, usize, usize, usize) {
    let c = edit.crop;
    let x = (c.x * w as f32).round() as usize;
    let y = (c.y * h as f32).round() as usize;
    let cw = ((c.w * w as f32).round() as usize).max(1).min(w - x);
    let ch = ((c.h * h as f32).round() as usize).max(1).min(h - y);
    (x, y, cw, ch)
}

/// Render size for the hi cache: enough that the requested total-image
/// region covers the display, with headroom — capped by max_edge and by
/// the full-resolution working dims (rendering past the source buys nothing).
pub fn hi_target(
    req_w: f64,
    req_h: f64,
    full_w: usize,
    full_h: usize,
    max_edge: u32,
) -> (usize, usize) {
    let edge = (req_w.max(req_h) * 1.25).round();
    let cap = (max_edge as f64).min(full_w.max(full_h) as f64);
    let t = edge.min(cap).max(1.0);
    let scale = t / full_w.max(full_h) as f64;
    if scale < 0.999 {
        (
            (full_w as f64 * scale).round().max(1.0) as usize,
            (full_h as f64 * scale).round().max(1.0) as usize,
        )
    } else {
        (full_w, full_h)
    }
}

/// Rasterize a polyline stroke into a coverage mask as soft discs of radius r
/// (255 at the center, feathering to 0 at the edge), taking the max on overlap.
pub fn stamp_stroke(coverage: &mut [u8], w: usize, h: usize, pts: &[(f32, f32)], r: f32) {
    let mut stamp = |cx: f32, cy: f32| {
        let x0 = (cx - r).floor().max(0.0) as usize;
        let y0 = (cy - r).floor().max(0.0) as usize;
        let x1 = ((cx + r).ceil() as usize).min(w - 1);
        let y1 = ((cy + r).ceil() as usize).min(h - 1);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                if d < r {
                    // Plateau to 60% of the radius, linear feather beyond —
                    // overlapping stamps keep full strength between centers.
                    let v = if d <= r * 0.6 {
                        255
                    } else {
                        (255.0 * (r - d) / (r * 0.4)) as u8
                    };
                    let i = y * w + x;
                    coverage[i] = coverage[i].max(v);
                }
            }
        }
    };
    let step = (r / 3.0).max(1.0);
    for pair in pts.windows(2) {
        let (ax, ay) = (pair[0].0 * w as f32, pair[0].1 * h as f32);
        let (bx, by) = (pair[1].0 * w as f32, pair[1].1 * h as f32);
        let len = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
        let n = (len / step).ceil().max(1.0) as usize;
        for i in 0..=n {
            let t = i as f32 / n as f32;
            stamp(ax + (bx - ax) * t, ay + (by - ay) * t);
        }
    }
    if pts.len() == 1 {
        stamp(pts[0].0 * w as f32, pts[0].1 * h as f32);
    }
}

/// Replay recorded Heal/Clone strokes onto a working frame of (w, h) that has
/// already been through [`geometry`] from the (fw, fh) original. Stroke points
/// are original-image normalized and map through the geometry; the radius is
/// a fraction of the working diagonal, matching the live capture, so a replay
/// at a different working resolution scales with it. Returns true when any
/// stroke changed pixels.
pub fn replay_strokes(
    pixels: &mut [u8],
    w: usize,
    h: usize,
    fw: usize,
    fh: usize,
    edit: &EditParams,
) -> bool {
    use crate::types::RetouchStroke;
    if edit.strokes.is_empty() || w == 0 || h == 0 {
        return false;
    }
    let diag = ((w * w + h * h) as f32).sqrt();
    let mut coverage = vec![0u8; w * h];
    let mut changed = false;
    for s in &edit.strokes {
        coverage.fill(0);
        let pts: Vec<(f32, f32)> = s
            .pts
            .iter()
            .map(|p| work_from_orig_norm(*p, fw, fh, edit))
            .collect();
        let r = (s.radius * diag).max(1.0);
        stamp_stroke(&mut coverage, w, h, &pts, r);
        if !coverage.iter().any(|&v| v > 0) {
            continue;
        }
        if s.is_clone() {
            let Some(src) = s.src else { continue };
            let src = work_from_orig_norm(src, fw, fh, edit);
            let first = pts[0];
            let dx = ((src.0 - first.0) * w as f32).round() as i32;
            let dy = ((src.1 - first.1) * h as f32).round() as i32;
            crate::clone::clone_stamp(pixels, w, h, &coverage, dx, dy);
            changed = true;
        } else {
            let mode = match s.mode {
                RetouchStroke::SMOOTH_FILL => crate::heal::HealMode::SmoothFill,
                RetouchStroke::PROXIMITY_MATCH => crate::heal::HealMode::ProximityMatch,
                _ => crate::heal::HealMode::ContentAware,
            };
            changed |= crate::heal::spot_heal(pixels, &coverage, w, h, 1.0, mode, s.seed);
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RetouchStroke;

    #[test]
    fn work_orig_norm_round_trip() {
        let (w, h) = (1600usize, 1067usize);
        for (k, angle) in [(0u8, 0.0f32), (1, 0.0), (2, 0.0), (3, 0.0), (1, -4.25), (3, 7.0)] {
            let mut edit = EditParams::default();
            edit.rot90 = k;
            edit.fine_angle = angle;
            for &p in &[
                (0.0f32, 0.0f32),
                (1.0, 0.0),
                (0.0, 1.0),
                (1.0, 1.0),
                (0.5, 0.5),
                (0.31, 0.77),
            ] {
                let work = work_from_orig_norm(p, w, h, &edit);
                let back = orig_from_work_norm(work, w, h, &edit);
                assert!(
                    (back.0 - p.0).abs() < 1e-4 && (back.1 - p.1).abs() < 1e-4,
                    "rot {k} angle {angle} p {p:?} back {back:?}"
                );
            }
        }
    }

    #[test]
    fn work_from_orig_norm_matches_rotate_90_pixels() {
        let (w, h) = (8usize, 4usize);
        // Pixel (x = 1, y = 2), in centers.
        let p = (1.5 / w as f32, 2.5 / h as f32);
        for k in 0..4u8 {
            let mut edit = EditParams::default();
            edit.rot90 = k;
            // Where rotate_90 puts that pixel, as a center in the rotated frame.
            let expected = match k {
                1 => (1.5 / h as f32, 1.5 / w as f32),
                2 => (6.5 / w as f32, 1.5 / h as f32),
                3 => (2.5 / h as f32, 6.5 / w as f32),
                _ => p,
            };
            let got = work_from_orig_norm(p, w, h, &edit);
            assert!((got.0 - expected.0).abs() < 1e-5, "k {k}: {got:?} vs {expected:?}");
            assert!((got.1 - expected.1).abs() < 1e-5, "k {k}: {got:?} vs {expected:?}");
        }
    }

    #[test]
    fn fine_angle_mapping_matches_auto_crop_sampling() {
        let (w, h) = (400usize, 300usize);
        let mut edit = EditParams::default();
        edit.fine_angle = 6.5;
        let (ow, oh) = working_dims(w, h, &edit);
        // The sampler rotate_auto_crop uses, at an output center.
        let a = -edit.fine_angle.to_radians();
        let (sn, cs) = a.sin_cos();
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        for &(u, v) in &[(0.25f32, 0.6f32), (0.5, 0.5), (0.83, 0.12)] {
            let src = (u * w as f32, v * h as f32);
            let work = work_from_orig_norm((u, v), w, h, &edit);
            let dx = work.0 * ow as f32 - ow as f32 / 2.0;
            let dy = work.1 * oh as f32 - oh as f32 / 2.0;
            let sx = cx + dx * cs - dy * sn;
            let sy = cy + dx * sn + dy * cs;
            assert!((sx - src.0).abs() < 0.01, "x {sx} vs {}", src.0);
            assert!((sy - src.1).abs() < 0.01, "y {sy} vs {}", src.1);
        }
    }

    #[test]
    fn replay_strokes_is_deterministic_and_location_aware() {
        let (w, h) = (96usize, 64usize);
        let mut src = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                src[i] = ((x / 7 + y / 7) % 2) as u8 * 200;
                src[i + 1] = (x * 2) as u8;
                src[i + 2] = (y * 3) as u8;
                src[i + 3] = 255;
            }
        }
        let mut edit = EditParams::default();
        edit.strokes.push(RetouchStroke {
            mode: RetouchStroke::CLONE,
            radius: 0.05,
            pts: vec![(0.7, 0.7), (0.72, 0.7)],
            src: Some((0.3, 0.3)),
            seed: 7,
        });
        let mut a = src.clone();
        let mut b = src.clone();
        assert!(replay_strokes(&mut a, w, h, w, h, &edit));
        assert!(replay_strokes(&mut b, w, h, w, h, &edit));
        assert_eq!(a, b, "same stroke must replay identically");
        let under = (44 * w + 68) * 4;
        assert_ne!(&a[under..under + 4], &src[under..under + 4]);
        assert_eq!(&a[0..4], &src[0..4], "far corner untouched");
    }
}
