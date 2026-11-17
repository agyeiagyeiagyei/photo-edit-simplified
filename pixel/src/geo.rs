//! Geometry pipeline shared by app and worker: rotate/crop math, target
//! sizing, stroke rasterization. Resampling after geometry lives in
//! [`crate::resample`], driven by [`crate::pipeline`].

use std::borrow::Cow;
use std::rc::Rc;

use crate::ops;
use crate::types::{EditParams, Selection, SelectionKind};

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

/// Straighten angle as [`geometry`] applies it: within ±0.01° counts as none.
fn straighten_of(edit: &EditParams) -> f32 {
    if edit.fine_angle.abs() > 0.01 {
        edit.fine_angle
    } else {
        0.0
    }
}

/// True when [`geometry`] turns and straightens both edits the same way.
pub fn same_geometry(a: &EditParams, b: &EditParams) -> bool {
    a.rot90 % 4 == b.rot90 % 4 && straighten_of(a) == straighten_of(b)
}

/// Carry a selection drawn in `from`'s working frame into `to`'s, so it stays
/// on the same pixels of the photo when the photo is turned or straightened.
/// `fw`×`fh` is the unturned photo's size; only its aspect matters.
///
/// A change of quarter turns alone is exact: corners map to corners and a
/// mask turns pixel for pixel. A straighten change maps rect and lasso
/// corners exactly (a rect becomes a four-point lasso) and resamples a mask
/// once, bilinearly. What leaves the frame is dropped, so callers that apply
/// several straighten changes should remap from the first selection each time.
pub fn remap_selection(
    sel: &Selection,
    fw: usize,
    fh: usize,
    from: &EditParams,
    to: &EditParams,
) -> Selection {
    if same_geometry(from, to) {
        return sel.clone();
    }
    let turns = (to.rot90 % 4 + 4 - from.rot90 % 4) % 4;
    let quarter = straighten_of(from) == straighten_of(to);
    let fwd = |p| work_from_orig_norm(orig_from_work_norm(p, fw, fh, from), fw, fh, to);
    let kind = match &sel.kind {
        SelectionKind::Rect { x, y, w, h } if quarter => {
            let (ax, ay) = fwd((*x, *y));
            let (bx, by) = fwd((x + w, y + h));
            SelectionKind::Rect {
                x: ax.min(bx),
                y: ay.min(by),
                w: (bx - ax).abs(),
                h: (by - ay).abs(),
            }
        }
        SelectionKind::Rect { x, y, w, h } => SelectionKind::Lasso(
            [(*x, *y), (x + w, *y), (x + w, y + h), (*x, y + h)].map(fwd).to_vec(),
        ),
        SelectionKind::Lasso(pts) => SelectionKind::Lasso(pts.iter().map(|&p| fwd(p)).collect()),
        SelectionKind::Mask { data, width, height } => {
            let (data, width, height) = if quarter {
                rotate_plane(data, *width, *height, turns)
            } else {
                // Keep the mask's density relative to its frame.
                let (sw, sh) = working_dims(fw, fh, from);
                let (dw, dh) = working_dims(fw, fh, to);
                let ow = ((dw * width) as f32 / sw as f32).round().max(1.0) as usize;
                let oh = ((dh * height) as f32 / sh as f32).round().max(1.0) as usize;
                let inv = |p| work_from_orig_norm(orig_from_work_norm(p, fw, fh, to), fw, fh, from);
                (resample_affine(data, *width, *height, ow, oh, inv), ow, oh)
            };
            SelectionKind::Mask { data: Rc::new(data), width, height }
        }
    };
    Selection { kind, feather: sel.feather }
}

/// Turn a one-byte-per-pixel plane clockwise by `turns` quarter turns, the
/// same way [`ops::rotate_90`] turns RGBA.
fn rotate_plane(src: &[u8], w: usize, h: usize, turns: u8) -> (Vec<u8>, usize, usize) {
    let k = turns % 4;
    let (ow, oh) = if k % 2 == 1 { (h, w) } else { (w, h) };
    let mut out = vec![0u8; src.len()];
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match k {
                1 => (h - 1 - y, x),
                2 => (w - 1 - x, h - 1 - y),
                3 => (y, w - 1 - x),
                _ => (x, y),
            };
            out[ny * ow + nx] = src[y * w + x];
        }
    }
    (out, ow, oh)
}

/// Resample a `w`×`h` plane into `ow`×`oh` through `inv`, which maps an
/// output-normalized point to a source-normalized one. The map between two
/// working frames is affine, so three points fix it. Bilinear inside the
/// source, edge-clamped for the outer half pixel, 0 beyond.
fn resample_affine(
    src: &[u8],
    w: usize,
    h: usize,
    ow: usize,
    oh: usize,
    inv: impl Fn((f32, f32)) -> (f32, f32),
) -> Vec<u8> {
    let o = inv((0.0, 0.0));
    let ex = inv((1.0, 0.0));
    let ey = inv((0.0, 1.0));
    let (wf, hf) = (w as f32, h as f32);
    // Source-pixel steps per output pixel along x and y.
    let (ax, ay) = ((ex.0 - o.0) * wf / ow as f32, (ex.1 - o.1) * hf / ow as f32);
    let (bx, by) = ((ey.0 - o.0) * wf / oh as f32, (ey.1 - o.1) * hf / oh as f32);
    let mut out = vec![0u8; ow * oh];
    for y in 0..oh {
        let fy = y as f32 + 0.5;
        for x in 0..ow {
            let fx = x as f32 + 0.5;
            let sx = o.0 * wf + ax * fx + bx * fy - 0.5;
            let sy = o.1 * hf + ay * fx + by * fy - 0.5;
            if sx < -0.5 || sy < -0.5 || sx > wf - 0.5 || sy > hf - 0.5 {
                continue;
            }
            let sx = sx.clamp(0.0, wf - 1.0);
            let sy = sy.clamp(0.0, hf - 1.0);
            let (x0, y0) = (sx as usize, sy as usize);
            let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
            let (tx, ty) = (sx - x0 as f32, sy - y0 as f32);
            let p = |x: usize, y: usize| src[y * w + x] as f32;
            let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
            let bot = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
            out[y * ow + x] = (top + (bot - top) * ty).round() as u8;
        }
    }
    out
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

/// Choose the source anchor for a heal stroke captured on a source frame at
/// (sw, sh), in original-image normalized coordinates (None for Smooth Fill,
/// which has no source patch). The app runs this on the display proxy when the
/// stroke is made; storing the anchor with the stroke means every replay
/// reuses the same source region at its own resolution, so the preview and
/// the export heal identically instead of each running its own search.
pub fn heal_anchor(
    pixels: &[u8],
    sw: usize,
    sh: usize,
    pts_orig: &[(f32, f32)],
    radius: f32,
    mode: u8,
    edit: &EditParams,
) -> Option<(f32, f32)> {
    use crate::types::RetouchStroke;
    let mode = match mode {
        RetouchStroke::CONTENT_AWARE => crate::heal::HealMode::ContentAware,
        RetouchStroke::PROXIMITY_MATCH => crate::heal::HealMode::ProximityMatch,
        _ => return None,
    };
    if pts_orig.is_empty() || sw == 0 || sh == 0 {
        return None;
    }
    let (frame, w, h) = geometry(pixels, sw, sh, edit);
    if w == 0 || h == 0 {
        return None;
    }
    let pts: Vec<(f32, f32)> = pts_orig
        .iter()
        .map(|&p| work_from_orig_norm(p, sw, sh, edit))
        .collect();
    let diag = ((w * w + h * h) as f32).sqrt();
    let mut coverage = vec![0u8; w * h];
    stamp_stroke(&mut coverage, w, h, &pts, (radius * diag).max(1.0));
    let (ox, oy) = crate::heal::find_source(&frame, &coverage, w, h, mode)?;
    let anchor = (pts[0].0 + ox as f32 / w as f32, pts[0].1 + oy as f32 / h as f32);
    Some(orig_from_work_norm(anchor, sw, sh, edit))
}

/// Replay recorded Heal/Clone strokes onto a working frame of (w, h) that has
/// already been through [`geometry`] from the (fw, fh) original. Stroke points
/// are original-image normalized and map through the geometry; the radius is
/// a fraction of the working diagonal, matching the live capture, so a replay
/// at a different working resolution scales with it. A heal stroke that
/// carries a source anchor ([`heal_anchor`]) samples that patch instead of
/// searching again. Returns true when any stroke changed pixels.
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
            // A stored anchor (original normalized) pins the patch; scaling
            // its offset to this working frame keeps the same source region
            // at any replay resolution.
            let fixed = s.src.map(|src| {
                let src = work_from_orig_norm(src, fw, fh, edit);
                (
                    ((src.0 - pts[0].0) * w as f32).round() as i64,
                    ((src.1 - pts[0].1) * h as f32).round() as i64,
                )
            });
            changed |= crate::heal::spot_heal_from(pixels, &coverage, w, h, 1.0, mode, s.seed, fixed);
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

    #[test]
    fn stored_heal_anchor_replays_the_same_patch_at_any_resolution() {
        let (w, h) = (192usize, 144usize);
        let mut small = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                small[i] = (((x * 29 + y * 11) % 97) * 2) as u8;
                small[i + 1] = ((((x / 6) * 13 + (y / 4) * 5) % 89) * 2) as u8;
                small[i + 2] = (((x ^ y) % 67) * 3) as u8;
                small[i + 3] = 255;
            }
        }
        // 2x nearest upscale: the same content at twice the resolution.
        let mut big = vec![0u8; w * 2 * h * 2 * 4];
        for y in 0..h * 2 {
            for x in 0..w * 2 {
                let s = ((y / 2) * w + x / 2) * 4;
                let d = (y * w * 2 + x) * 4;
                big[d..d + 4].copy_from_slice(&small[s..s + 4]);
            }
        }
        let pts = [(0.5f32, 0.5f32)];
        let radius = 0.05f32;
        let seed = 11u32;
        for rot in [0u8, 1] {
            let mut edit = EditParams::default();
            edit.rot90 = rot;
            let anchor = heal_anchor(&small, w, h, &pts, radius, RetouchStroke::CONTENT_AWARE, &edit)
                .expect("textured frame has a source patch");
            edit.strokes.push(RetouchStroke {
                mode: RetouchStroke::CONTENT_AWARE,
                radius,
                pts: pts.to_vec(),
                src: Some(anchor),
                seed,
            });
            let (ga, sw, sh) = geometry(&small, w, h, &edit);
            let (gb, bw, bh) = geometry(&big, w * 2, h * 2, &edit);
            let mut a = ga.into_owned();
            let mut b = gb.into_owned();
            assert!(replay_strokes(&mut a, sw, sh, w, h, &edit));
            assert!(replay_strokes(&mut b, bw, bh, w * 2, h * 2, &edit));
            assert_eq!((bw, bh), (sw * 2, sh * 2));
            // Box-average the 2x replay back to the 1x working grid.
            let mut bd = vec![0u8; sw * sh * 4];
            for y in 0..sh {
                for x in 0..sw {
                    for c in 0..4 {
                        let s = ((y * 2) * bw + x * 2) * 4 + c;
                        let s2 = ((y * 2 + 1) * bw + x * 2) * 4 + c;
                        let s3 = ((y * 2) * bw + x * 2 + 1) * 4 + c;
                        let s4 = ((y * 2 + 1) * bw + x * 2 + 1) * 4 + c;
                        bd[(y * sw + x) * 4 + c] =
                            ((b[s] as u32 + b[s2] as u32 + b[s3] as u32 + b[s4] as u32) / 4) as u8;
                    }
                }
            }
            let (cx, cy) = ((sw / 2) as i64, (sh / 2) as i64);
            let r = 10i64;
            let (mut mse, mut n) = (0f64, 0u64);
            for y in cy - r..=cy + r {
                for x in cx - r..=cx + r {
                    if (x - cx).pow(2) + (y - cy).pow(2) > r * r {
                        continue;
                    }
                    let i = ((y * sw as i64 + x) * 4) as usize;
                    for c in 0..3 {
                        let d = a[i + c] as f64 - bd[i + c] as f64;
                        mse += d * d;
                        n += 1;
                    }
                }
            }
            let psnr = if mse > 0.0 { 10.0 * (255.0f64 * 255.0 / (mse / n as f64)).log10() } else { 99.0 };
            assert!(psnr > 30.0, "rot {rot}: 1x and 2x replays differ ({psnr:.1} dB)");
        }
    }

    /// A 300×200 RGBA photo with an off-centre bar and disc in red, well
    /// inside the frame so a straighten crop never reaches them.
    fn shapes_photo() -> (Vec<u8>, usize, usize) {
        let (w, h) = (300usize, 200usize);
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let bar = (60..140).contains(&x) && (50..110).contains(&y);
                let disc = (x as i64 - 190).pow(2) + (y as i64 - 120).pow(2) <= 900;
                let i = (y * w + x) * 4;
                px[i] = if bar || disc { 255 } else { 0 };
                px[i + 3] = 255;
            }
        }
        (px, w, h)
    }

    /// The shapes as a mask in `edit`'s working frame, read off the turned photo.
    fn shapes_mask(edit: &EditParams) -> (Vec<u8>, usize, usize) {
        let (px, w, h) = shapes_photo();
        let (g, gw, gh) = geometry(&px, w, h, edit);
        (g.chunks_exact(4).map(|p| if p[0] > 127 { 255 } else { 0 }).collect(), gw, gh)
    }

    fn geo(rot90: u8, fine_angle: f32) -> EditParams {
        EditParams { rot90, fine_angle, ..EditParams::default() }
    }

    fn mask_of(sel: &Selection) -> (Vec<u8>, usize, usize) {
        match &sel.kind {
            SelectionKind::Mask { data, width, height } => ((**data).clone(), *width, *height),
            k => panic!("expected a mask, got {k:?}"),
        }
    }

    fn iou(a: &[u8], b: &[u8]) -> f32 {
        let (mut both, mut any) = (0u32, 0u32);
        for (&p, &q) in a.iter().zip(b) {
            both += (p > 127 && q > 127) as u32;
            any += (p > 127 || q > 127) as u32;
        }
        both as f32 / any.max(1) as f32
    }

    #[test]
    fn a_turned_mask_lands_on_the_same_pixels() {
        let pairs = [(0u8, 1u8), (0, 2), (0, 3), (1, 2), (3, 1), (2, 0)];
        for (a, b) in pairs {
            let (from, to) = (geo(a, 0.0), geo(b, 0.0));
            let (m, mw, mh) = shapes_mask(&from);
            let sel = Selection {
                kind: SelectionKind::Mask { data: Rc::new(m), width: mw, height: mh },
                feather: 0.02,
            };
            let out = remap_selection(&sel, 300, 200, &from, &to);
            assert_eq!(out.feather, 0.02);
            let (got, gw, gh) = mask_of(&out);
            let (want, ww, wh) = shapes_mask(&to);
            assert_eq!((gw, gh), (ww, wh), "{a}->{b}: dims");
            assert!(got == want, "{a}->{b}: a quarter turn must move the mask exactly");
        }
    }

    #[test]
    fn a_straightened_mask_lands_on_the_same_pixels() {
        let cases = [
            ((0u8, 0.0f32), (0u8, 6.0f32)),
            ((0, 6.0), (0, -3.5)),
            ((1, 0.0), (1, -5.0)),
            ((0, 4.0), (1, 4.0)),
            ((2, -7.0), (3, 2.0)),
        ];
        for ((ra, fa), (rb, fb)) in cases {
            let (from, to) = (geo(ra, fa), geo(rb, fb));
            let (m, mw, mh) = shapes_mask(&from);
            let sel = Selection {
                kind: SelectionKind::Mask { data: Rc::new(m), width: mw, height: mh },
                feather: 0.0,
            };
            let (got, gw, gh) = mask_of(&remap_selection(&sel, 300, 200, &from, &to));
            let (want, ww, wh) = shapes_mask(&to);
            assert_eq!((gw, gh), (ww, wh), "{ra}/{fa} -> {rb}/{fb}: dims");
            let score = iou(&got, &want);
            assert!(score > 0.95, "{ra}/{fa} -> {rb}/{fb}: IoU {score:.3}");
        }
    }

    #[test]
    fn rects_and_lassos_follow_the_photo() {
        // The bar, as a rect in the unturned frame.
        let bar = Selection {
            kind: SelectionKind::Rect { x: 0.2, y: 0.25, w: 80.0 / 300.0, h: 0.3 },
            feather: 0.0,
        };
        for (rb, fb) in [(1u8, 0.0f32), (2, 0.0), (3, 0.0), (0, 5.0), (1, -6.0)] {
            let to = geo(rb, fb);
            let out = remap_selection(&bar, 300, 200, &geo(0, 0.0), &to);
            match (&out.kind, fb == 0.0) {
                (SelectionKind::Rect { .. }, true) | (SelectionKind::Lasso(_), false) => {}
                (k, _) => panic!("{rb}/{fb}: unexpected kind {k:?}"),
            }
            let (want, ww, wh) = shapes_mask(&to);
            let got = ops::selection_mask(&out, ww, wh);
            // Only the bar is selected; the disc stays out.
            let bar_px = got.iter().filter(|&&v| v > 127).count();
            let hit = got.iter().zip(&want).filter(|(&g, &w)| g > 127 && w > 127).count();
            assert!(hit as f32 > 0.97 * bar_px as f32, "{rb}/{fb}: the rect left the bar");
            assert!((bar_px as f32 - 80.0 * 60.0).abs() < 0.08 * 4800.0, "{rb}/{fb}: area {bar_px}");
        }
        // A lasso maps point by point, and four turns bring it home.
        let lasso = Selection {
            kind: SelectionKind::Lasso(vec![(0.1, 0.2), (0.4, 0.15), (0.35, 0.6)]),
            feather: 0.0,
        };
        let mut cur = lasso.clone();
        for k in 0..4u8 {
            cur = remap_selection(&cur, 300, 200, &geo(k, 0.0), &geo((k + 1) % 4, 0.0));
        }
        let SelectionKind::Lasso(pts) = &cur.kind else { panic!("lasso became {:?}", cur.kind) };
        let SelectionKind::Lasso(orig) = &lasso.kind else { unreachable!() };
        for (p, q) in pts.iter().zip(orig) {
            assert!((p.0 - q.0).abs() < 1e-5 && (p.1 - q.1).abs() < 1e-5, "{p:?} vs {q:?}");
        }
    }

    #[test]
    fn same_geometry_keeps_the_selection() {
        let sel = Selection {
            kind: SelectionKind::Mask { data: Rc::new(vec![7u8; 6]), width: 3, height: 2 },
            feather: 0.1,
        };
        let e = geo(1, 0.005); // under the straighten threshold: no change
        let out = remap_selection(&sel, 300, 200, &geo(1, 0.0), &e);
        let (SelectionKind::Mask { data: a, .. }, SelectionKind::Mask { data: b, .. }) = (&sel.kind, &out.kind) else {
            panic!()
        };
        assert!(Rc::ptr_eq(a, b));
    }
}
