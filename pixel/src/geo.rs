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

/// An edit that only turns and straightens: the frame an [`Area`] was drawn
/// in. One copy of the default edit's construction, not one per caller.
///
/// [`Area`]: crate::types::Area
#[inline(never)]
fn turned_by(rot90: u8, fine_angle: f32) -> EditParams {
    EditParams { rot90, fine_angle, ..EditParams::default() }
}

/// Replay a recorded Cut or Delete: clear alpha under its selection, carried
/// from the geometry it was made in to the frame's. Colours stay, so what is
/// under the hole still develops and a later clone can fill it.
fn erase_alpha<T: crate::resample::Sample>(
    pixels: &mut [T],
    w: usize,
    h: usize,
    fw: usize,
    fh: usize,
    e: &crate::types::Area,
    edit: &EditParams,
) -> bool {
    let from = turned_by(e.rot90, e.fine_angle);
    let mask = ops::selection_mask(&remap_selection(&e.sel, fw, fh, &from, edit), w, h);
    let mut changed = false;
    for (px, &m) in pixels.chunks_exact_mut(4).zip(&mask) {
        if m > 0 {
            px[3] = T::from_u32((px[3].to_u32() * (255 - m as u32) + 127) / 255);
            changed = true;
        }
    }
    changed
}

/// The recorded cuts of `edit` alone, on an 8-bit frame that has been through
/// [`geometry`]: for the app's low-res preview, which does not replay the
/// Heal and Clone strokes but must not show a hole filled in.
pub fn replay_erases(pixels: &mut [u8], w: usize, h: usize, fw: usize, fh: usize, edit: &EditParams) {
    for e in edit.strokes.iter().filter_map(|s| s.erased()) {
        erase_alpha(pixels, w, h, fw, fh, e, edit);
    }
}

/// The recorded Content fills of `edit` alone, on the low-res preview as it
/// is stored, before [`geometry`]: the app's soft frames would otherwise show
/// what a fill removed. The texture is this frame's own, not the one the
/// full-size fill finds. Returns true when a fill wrote pixels.
pub fn replay_fills(pixels: &mut [u8], w: usize, h: usize, edit: &EditParams) -> bool {
    let unturned = turned_by(0, 0.0);
    let mut changed = false;
    for a in edit.strokes.iter().filter(|s| s.mode == crate::types::RetouchStroke::FILL).filter_map(|s| s.area.as_ref()) {
        let from = turned_by(a.rot90, a.fine_angle);
        let mask = ops::selection_mask(&remap_selection(&a.sel, w, h, &from, &unturned), w, h);
        changed |= crate::fill::fill_near(pixels, &mask, w, h).is_some();
    }
    changed
}

/// The sample types a frame comes in, each with its own [`geometry`].
pub trait Frame: crate::resample::Sample {
    fn turned<'a>(pixels: &'a [Self], w: usize, h: usize, edit: &EditParams) -> (Cow<'a, [Self]>, usize, usize);
}

impl Frame for u8 {
    fn turned<'a>(pixels: &'a [u8], w: usize, h: usize, edit: &EditParams) -> (Cow<'a, [u8]>, usize, usize) {
        geometry(pixels, w, h, edit)
    }
}

impl Frame for u16 {
    fn turned<'a>(pixels: &'a [u16], w: usize, h: usize, edit: &EditParams) -> (Cow<'a, [u16]>, usize, usize) {
        geometry_wide(pixels, w, h, edit)
    }
}

/// Stable fingerprint of a retouch list, for cache keys.
pub fn strokes_fingerprint(strokes: &[crate::types::RetouchStroke]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for s in strokes {
        if let Some(e) = &s.area {
            mix(e.id);
            mix(e.rot90 as u64);
            mix(e.fine_angle.to_bits() as u64);
        }
        mix(s.mode as u64);
        mix(s.radius.to_bits() as u64);
        mix(s.seed as u64);
        mix(s.pts.len() as u64);
        for p in &s.pts {
            mix(p.0.to_bits() as u64);
            mix(p.1.to_bits() as u64);
        }
        let src = s.src;
        mix(src.is_some() as u64);
        if let Some((x, y)) = src {
            mix(x.to_bits() as u64);
            mix(y.to_bits() as u64);
        }
    }
    h
}

thread_local! {
    /// What each recorded Content fill synthesised, newest last. A fill takes
    /// seconds on a full-size frame and a render replays the list every time
    /// it rebuilds its base, so the result is kept and pasted.
    static PATCHES: std::cell::RefCell<Vec<(u64, Rc<crate::fill::Patch>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Memory the kept fills may hold. Past it the oldest go, and are synthesised
/// again when a render next needs them.
const PATCH_BYTES: usize = 192 << 20;

fn patch_get(key: u64) -> Option<Rc<crate::fill::Patch>> {
    PATCHES.with(|c| {
        let mut c = c.borrow_mut();
        let i = c.iter().position(|(k, _)| *k == key)?;
        let e = c.remove(i);
        c.push(e.clone());
        Some(e.1)
    })
}

fn patch_put(key: u64, patch: crate::fill::Patch) -> Rc<crate::fill::Patch> {
    let patch = Rc::new(patch);
    PATCHES.with(|c| {
        let mut c = c.borrow_mut();
        c.push((key, patch.clone()));
        while c.len() > 1 && c.iter().map(|(_, p)| p.rgba.len() * 2 + p.cover.len()).sum::<usize>() > PATCH_BYTES {
            c.remove(0);
        }
    });
    patch
}

/// Drop the kept fills, so the next replay synthesises them again.
pub fn forget_fills() {
    PATCHES.with(|c| c.borrow_mut().clear());
}

/// Replay a recorded Content fill, entry `at` of the list. Returns true when
/// it wrote pixels.
///
/// With the photo's source frame in hand the fill is synthesised in the
/// geometry it was recorded in, from that frame with the entries before it
/// replayed, and kept. A frame in another turn or straighten gets the same
/// pixels carried over, not a new search: what the user saw is what stays,
/// and a straighten drag does not wait for a fill at each step. Without the
/// source (`src` None) the fill is synthesised on the frame as it is.
#[allow(clippy::too_many_arguments)]
fn replay_fill<T: Frame>(
    pixels: &mut [T],
    w: usize,
    h: usize,
    src: Option<&[T]>,
    fw: usize,
    fh: usize,
    edit: &EditParams,
    at: usize,
    a: &crate::types::Area,
) -> bool {
    let mut from = turned_by(a.rot90, a.fine_angle);
    // The patch is in this frame's own pixels: no carrying over.
    let here = src.is_none() || same_geometry(&from, edit);
    let mut key = strokes_fingerprint(&edit.strokes[..at]);
    let mut mix = |v: u64| key = (key ^ v).wrapping_mul(0x0000_0100_0000_01b3);
    // The photo, by a sparse sample of its pixels.
    let base: &[T] = src.unwrap_or(pixels);
    for v in base.iter().step_by((base.len() / 251).max(1)) {
        mix(v.to_u32() as u64);
    }
    for v in [a.id, fw as u64, fh as u64, std::mem::size_of::<T>() as u64] {
        mix(v);
    }
    if src.is_none() {
        for v in [edit.rot90 as u64 % 4, straighten_of(edit).to_bits() as u64, w as u64] {
            mix(v);
        }
    }
    let patch = match patch_get(key) {
        Some(p) => p,
        None if here => {
            let sel = remap_selection(&a.sel, fw, fh, &from, edit);
            let Some(p) = crate::fill::fill_near(pixels, &ops::selection_mask(&sel, w, h), w, h) else {
                return false;
            };
            // The fill wrote the frame itself.
            patch_put(key, p);
            return true;
        }
        None => {
            let Some(src) = src else { return false };
            // The frame the fill was recorded on, with the entries before it.
            from.strokes = edit.strokes[..at].to_vec();
            let (frame, w0, h0) = T::turned(src, fw, fh, &from);
            let mut frame = frame.into_owned();
            replay_strokes_from(&mut frame, w0, h0, Some(src), fw, fh, &from);
            let Some(p) = crate::fill::fill_near(&mut frame, &ops::selection_mask(&a.sel, w0, h0), w0, h0) else {
                return false;
            };
            patch_put(key, p)
        }
    };
    if here {
        if patch.x + patch.w > w || patch.y + patch.h > h {
            return false;
        }
        for (i, _) in patch.cover.iter().enumerate().filter(|(_, &c)| c != 0) {
            let d = ((patch.y + i / patch.w) * w + patch.x + i % patch.w) * 4;
            for c in 0..4 {
                pixels[d + c] = T::from_u32(patch.rgba[i * 4 + c] as u32);
            }
        }
        return true;
    }
    carry_patch(pixels, w, h, &patch, fw, fh, &from, edit)
}

/// Paste a fill's patch, made in `from`'s working frame, into the frame of
/// `to`. The map between two working frames is affine. A quarter turn lands
/// on pixel centres and copies; a straighten samples the patch bilinearly,
/// as [`geometry`] samples the frame, wherever a sample touches the fill.
#[allow(clippy::too_many_arguments)]
fn carry_patch<T: Frame>(
    pixels: &mut [T],
    w: usize,
    h: usize,
    patch: &crate::fill::Patch,
    fw: usize,
    fh: usize,
    from: &EditParams,
    to: &EditParams,
) -> bool {
    let (w0, h0) = working_dims(fw, fh, from);
    let (w0f, h0f, wf, hf) = (w0 as f32, h0 as f32, w as f32, h as f32);
    // A straightened frame shows the photo half a pixel down and right of
    // where the point maps put it: `rotate_auto_crop` reads its source by
    // pixel index, not by pixel centre. The patch has to sit on the pixels
    // the frame has, so the map between the two frames allows for it.
    let bias = |e: &EditParams| {
        if straighten_of(e) == 0.0 {
            return (0.0, 0.0);
        }
        let (rw, rh) = if e.rot90 % 2 == 1 { (fh, fw) } else { (fw, fh) };
        let p = rot90_norm_inv((0.5 + 0.5 / rw as f32, 0.5 + 0.5 / rh as f32), e.rot90);
        (p.0 - 0.5, p.1 - 0.5)
    };
    let (bt, bf) = (bias(to), bias(from));
    let inv = |p| {
        let o = orig_from_work_norm(p, fw, fh, to);
        work_from_orig_norm((o.0 + bt.0 - bf.0, o.1 + bt.1 - bf.1), fw, fh, from)
    };
    let fwd = |p| work_from_orig_norm(orig_from_work_norm(p, fw, fh, from), fw, fh, to);
    let (o, ex, ey) = (inv((0.0, 0.0)), inv((1.0, 0.0)), inv((0.0, 1.0)));
    // Source-pixel steps per pixel of this frame along x and y.
    let (ax, ay) = ((ex.0 - o.0) * w0f / wf, (ex.1 - o.1) * h0f / wf);
    let (bx, by) = ((ey.0 - o.0) * w0f / hf, (ey.1 - o.1) * h0f / hf);
    // This frame's box around the patch.
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (cx, cy) in [(0, 0), (patch.w, 0), (0, patch.h), (patch.w, patch.h)] {
        let p = fwd(((patch.x + cx) as f32 / w0f, (patch.y + cy) as f32 / h0f));
        x0 = x0.min(p.0 * wf);
        x1 = x1.max(p.0 * wf);
        y0 = y0.min(p.1 * hf);
        y1 = y1.max(p.1 * hf);
    }
    let (x0, y0) = ((x0 - 2.0).max(0.0) as usize, (y0 - 2.0).max(0.0) as usize);
    let (x1, y1) = (((x1 + 3.0).max(0.0) as usize).min(w), ((y1 + 3.0).max(0.0) as usize).min(h));
    let narrow = std::mem::size_of::<T>() == 1;
    // A hair off a pixel centre is rounding: take the pixel.
    let snap = |v: f32| if (v - v.round()).abs() < 0.02 { v.round() } else { v };
    let mut changed = false;
    for y in y0..y1 {
        let fy = y as f32 + 0.5;
        for x in x0..x1 {
            let fx = x as f32 + 0.5;
            let sx = snap(o.0 * w0f + ax * fx + bx * fy - 0.5 - patch.x as f32);
            let sy = snap(o.1 * h0f + ay * fx + by * fy - 0.5 - patch.y as f32);
            let (ix, iy) = (sx.floor(), sy.floor());
            let (tx, ty) = (sx - ix, sy - iy);
            let mut acc = [0f32; 4];
            let mut filled = false;
            for (dx, dy, wgt) in [(0, 0, (1.0 - tx) * (1.0 - ty)), (1, 0, tx * (1.0 - ty)), (0, 1, (1.0 - tx) * ty), (1, 1, tx * ty)] {
                if wgt <= 0.0 {
                    continue;
                }
                // The patch carries a pixel of the frame around the fill, so
                // these stay inside it except at the frame's own edge.
                let px = (ix as i64 + dx).clamp(0, patch.w as i64 - 1) as usize;
                let py = (iy as i64 + dy).clamp(0, patch.h as i64 - 1) as usize;
                let i = py * patch.w + px;
                filled |= patch.cover[i] != 0;
                for (a, v) in acc.iter_mut().zip(&patch.rgba[i * 4..i * 4 + 4]) {
                    *a += wgt * *v as f32;
                }
            }
            if !filled {
                continue;
            }
            let d = (y * w + x) * 4;
            for c in 0..4 {
                // The 8-bit straighten drops the fraction; the 16-bit rounds.
                pixels[d + c] = if narrow { T::from_u32(acc[c] as u32) } else { T::from_f32(acc[c]) };
            }
            changed = true;
        }
    }
    changed
}

/// Whether the Content fill that ends `edit`'s retouch list finds pixels to
/// copy, on the photo's source frame `src`. The fill is kept, so the render
/// that follows a yes pastes it.
pub fn try_fill<T: Frame>(src: &[T], fw: usize, fh: usize, edit: &EditParams) -> bool {
    let Some((at, a)) = edit.strokes.len().checked_sub(1).and_then(|i| Some((i, edit.strokes[i].area.as_ref()?))) else {
        return false;
    };
    let mut before = turned_by(edit.rot90, edit.fine_angle);
    before.strokes = edit.strokes[..at].to_vec();
    let (p, w, h) = T::turned(src, fw, fh, edit);
    let mut frame = p.into_owned();
    replay_strokes_from(&mut frame, w, h, Some(src), fw, fh, &before);
    replay_fill(&mut frame, w, h, Some(src), fw, fh, edit, at, a)
}

/// Replay the retouch list onto a working frame of (w, h) that has already
/// been through [`geometry`] from the (fw, fh) original. Stroke points are
/// original-image normalized and map through the geometry; the radius is a
/// fraction of the working diagonal, matching the live capture, so a replay
/// at a different working resolution scales with it. A heal stroke that
/// carries a source anchor ([`heal_anchor`]) samples that patch instead of
/// searching again. `src` is the (fw, fh) frame `pixels` was turned from; a
/// Content fill needs it to stay the same fill through a later turn or
/// straighten ([`replay_fill`]). Returns true when any stroke changed pixels.
pub fn replay_strokes_from<T: Frame>(
    pixels: &mut [T],
    w: usize,
    h: usize,
    src: Option<&[T]>,
    fw: usize,
    fh: usize,
    edit: &EditParams,
) -> bool {
    use crate::types::RetouchStroke;
    if edit.strokes.is_empty() || w == 0 || h == 0 {
        return false;
    }
    let diag = ((w * w + h * h) as f32).sqrt();
    let mut coverage = Vec::new();
    let mut changed = false;
    for (at, s) in edit.strokes.iter().enumerate() {
        if let Some(a) = &s.area {
            changed |= match s.mode {
                RetouchStroke::ERASE => erase_alpha(pixels, w, h, fw, fh, a, edit),
                RetouchStroke::FILL => replay_fill(pixels, w, h, src, fw, fh, edit, at, a),
                _ => false,
            };
            continue;
        }
        coverage.clear();
        coverage.resize(w * h, 0u8);
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

/// [`replay_strokes_from`] without the photo's source frame.
pub fn replay_strokes<T: Frame>(pixels: &mut [T], w: usize, h: usize, fw: usize, fh: usize, edit: &EditParams) -> bool {
    replay_strokes_from(pixels, w, h, None, fw, fh, edit)
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
            area: None,
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
                area: None,
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

    fn erased(sel: Selection, rot90: u8) -> EditParams {
        let mut edit = EditParams::default();
        edit.strokes.push(RetouchStroke::erase(crate::types::Area { id: 1, sel, rot90, fine_angle: 0.0 }));
        edit
    }

    #[test]
    fn an_erase_clears_alpha_and_keeps_colour_at_8_and_16_bits() {
        let (w, h) = (40usize, 20usize);
        let narrow: Vec<u8> = (0..w * h * 4).map(|i| if i % 4 == 3 { 255 } else { (i % 251) as u8 }).collect();
        let wide: Vec<u16> = narrow.iter().map(|&v| v as u16 * 257).collect();
        let sel = Selection { kind: SelectionKind::Rect { x: 0.25, y: 0.5, w: 0.5, h: 0.5 }, feather: 0.2 };
        let edit = erased(sel, 0);
        let (mut a, mut b) = (narrow.clone(), wide.clone());
        assert!(replay_strokes(&mut a, w, h, w, h, &edit));
        assert!(replay_strokes(&mut b, w, h, w, h, &edit));
        let alpha = |x: usize, y: usize| a[(y * w + x) * 4 + 3];
        assert_eq!(alpha(20, 15), 0, "inside the selection");
        assert_eq!(alpha(2, 2), 255, "outside it");
        assert!((1..255).contains(&alpha(10, 15)), "the feathered edge is partial: {}", alpha(10, 15));
        for (i, (p, q)) in a.iter().zip(&b).enumerate() {
            if i % 4 == 3 {
                assert!((*p as i32 - (*q as f32 / 257.0).round() as i32).abs() <= 1, "alpha {p} vs {q} at {i}");
            } else {
                assert_eq!((*p, *q), (narrow[i], wide[i]), "colour moved at {i}");
            }
        }
    }

    #[test]
    fn an_erase_turns_with_the_photo() {
        // Cut the top-left quarter of an 8x4 photo, then turn it clockwise:
        // that quarter is now the top-right of the 4x8 frame.
        let (w, h) = (8usize, 4usize);
        let sel = Selection { kind: SelectionKind::Rect { x: 0.0, y: 0.0, w: 0.5, h: 0.5 }, feather: 0.0 };
        let mut edit = erased(sel, 0);
        edit.rot90 = 1;
        let src = vec![255u8; w * h * 4];
        let (g, gw, gh) = geometry(&src, w, h, &edit);
        let mut px = g.into_owned();
        assert!(replay_strokes(&mut px, gw, gh, w, h, &edit));
        for y in 0..gh {
            for x in 0..gw {
                let want = if x >= 2 && y < 4 { 0 } else { 255 };
                assert_eq!(px[(y * gw + x) * 4 + 3], want, "({x}, {y})");
            }
        }
    }
    /// Noisy diagonal bands with a bright block in the middle of the frame:
    /// the thing a fill removes.
    fn scene(w: usize, h: usize) -> Vec<u8> {
        let mut seed = 9u32;
        let mut img = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let band = if (x + y) / 8 % 2 == 0 { 120 } else { 70 };
                let v = (band + (seed >> 27)) as u8;
                let block = (w * 2 / 5..w * 3 / 5).contains(&x) && (h * 2 / 5..h * 3 / 5).contains(&y);
                img.extend_from_slice(&if block { [250, 240, 20, 255] } else { [v, v / 2 + 30, 200 - v, 255] });
            }
        }
        img
    }

    /// A fill over the block of [`scene`], recorded in quarter turn `rot90`
    /// of the frame it is given in.
    fn filled(rot90: u8) -> EditParams {
        let sel = Selection { kind: SelectionKind::Rect { x: 0.37, y: 0.37, w: 0.26, h: 0.26 }, feather: 0.0 };
        let mut edit = EditParams::default();
        edit.strokes.push(RetouchStroke::fill(crate::types::Area { id: 7, sel, rot90, fine_angle: 0.0 }));
        edit
    }

    fn render<T: Frame>(src: &[T], w: usize, h: usize, edit: &EditParams) -> (Vec<T>, usize, usize) {
        let (g, gw, gh) = T::turned(src, w, h, edit);
        let mut px = g.into_owned();
        replay_strokes_from(&mut px, gw, gh, Some(src), w, h, edit);
        (px, gw, gh)
    }

    #[test]
    fn a_fill_replays_to_the_same_pixels_from_a_cold_cache() {
        let (w, h) = (120usize, 90usize);
        let src = scene(w, h);
        let edit = filled(0);
        forget_fills();
        let (first, ..) = render(&src, w, h, &edit);
        // The block is gone and nothing outside the selection moved.
        for p in 0..w * h {
            let (x, y) = (p % w, p / w);
            let inside = (45..75).contains(&x) && (34..56).contains(&y);
            let px = &first[p * 4..p * 4 + 4];
            if inside {
                assert!(px[0] < 200, "({x}, {y}) still the block: {px:?}");
            } else if !((43..78).contains(&x) && (32..59).contains(&y)) {
                assert_eq!(px, &src[p * 4..p * 4 + 4], "({x}, {y}) moved");
            }
        }
        // From the kept patch, and synthesised again: the same frame.
        assert_eq!(render(&src, w, h, &edit).0, first);
        forget_fills();
        assert_eq!(render(&src, w, h, &edit).0, first);
        // Without the source frame the fill runs on the frame itself.
        let mut bare = src.clone();
        assert!(replay_strokes(&mut bare, w, h, w, h, &edit));
        assert_eq!(bare, first);
        // 16 bits wide: the same donors.
        let wide: Vec<u16> = src.iter().map(|&v| v as u16 * 257).collect();
        let (wide, ..) = render(&wide, w, h, &edit);
        assert_eq!(wide, first.iter().map(|&v| v as u16 * 257).collect::<Vec<u16>>());
    }

    #[test]
    fn a_fill_keeps_its_pixels_through_a_quarter_turn() {
        let (w, h) = (120usize, 90usize);
        let src = scene(w, h);
        let mut edit = filled(0);
        forget_fills();
        let (flat, ..) = render(&src, w, h, &edit);
        for turns in 1..4u8 {
            edit.rot90 = turns;
            let (want, tw, th) = ops::rotate_90(&flat, w, h, turns);
            // Carried over from the kept patch, then from a cold cache, where
            // the fill is made in the frame it was recorded in first.
            assert_eq!(render(&src, w, h, &edit), (want.clone(), tw, th), "{turns} turns, kept");
            forget_fills();
            assert_eq!(render(&src, w, h, &edit), (want, tw, th), "{turns} turns, cold");
        }
        // Recorded on a turned photo, shown unturned.
        let turned = filled(1);
        forget_fills();
        let (made, mw, mh) = render(&src, w, h, &EditParams { rot90: 1, ..turned.clone() });
        let (back, ..) = ops::rotate_90(&made, mw, mh, 3);
        assert_eq!(render(&src, w, h, &turned).0, back);
    }

    #[test]
    fn a_fill_follows_a_straighten_without_a_new_search() {
        let (w, h) = (240usize, 180usize);
        let src = scene(w, h);
        let mut edit = filled(0);
        forget_fills();
        let (flat, ..) = render(&src, w, h, &edit);
        edit.fine_angle = 4.0;
        // The filled frame, straightened, is what the carried patch must give.
        let (want, sw, sh) = geometry(&flat, w, h, &EditParams { fine_angle: 4.0, ..EditParams::default() });
        let (got, gw, gh) = render(&src, w, h, &edit);
        assert_eq!((gw, gh), (sw, sh));
        let (mut worst, mut sum) = (0i32, 0i64);
        for (a, b) in got.iter().zip(want.iter()) {
            let d = (*a as i32 - *b as i32).abs();
            worst = worst.max(d);
            sum += d as i64;
        }
        assert!(worst <= 2, "worst {worst}");
        assert!(sum as f64 / (got.len() as f64) < 0.05, "mean {}", sum as f64 / got.len() as f64);
        // The preview's own fill, on the stored frame: the block goes there too.
        let mut soft = src.clone();
        assert!(replay_fills(&mut soft, w, h, &edit));
        let mid = (h / 2 * w + w / 2) * 4;
        assert!(soft[mid] < 200, "{:?}", &soft[mid..mid + 4]);
    }

    #[test]
    fn a_fill_is_tried_before_it_is_recorded() {
        let (w, h) = (120usize, 90usize);
        let src = scene(w, h);
        forget_fills();
        assert!(try_fill(&src, w, h, &filled(0)));
        // Everything selected: nothing to copy from.
        let all = Selection { kind: SelectionKind::Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }, feather: 0.0 };
        let mut edit = EditParams::default();
        edit.strokes.push(RetouchStroke::fill(crate::types::Area { id: 8, sel: all, rot90: 0, fine_angle: 0.0 }));
        assert!(!try_fill(&src, w, h, &edit));
        assert!(!try_fill(&src, w, h, &EditParams::default()));
    }

    #[test]
    fn a_fill_sees_the_entries_before_it_and_not_those_after() {
        // A cut next to the block, then the fill: the hole is no donor, and
        // it is still a hole afterwards.
        let (w, h) = (120usize, 90usize);
        let src = scene(w, h);
        let hole = Selection { kind: SelectionKind::Rect { x: 0.1, y: 0.37, w: 0.2, h: 0.26 }, feather: 0.0 };
        let mut edit = erased(hole, 0);
        edit.strokes.extend(filled(0).strokes);
        forget_fills();
        let (px, ..) = render(&src, w, h, &edit);
        for y in 34..56 {
            for x in 13..35 {
                assert_eq!(px[(y * w + x) * 4 + 3], 0, "hole ({x}, {y})");
            }
            for x in 45..75 {
                assert_eq!(px[(y * w + x) * 4 + 3], 255, "fill ({x}, {y})");
            }
        }
        // The same fill without the cut before it is another patch.
        let (alone, ..) = render(&src, w, h, &filled(0));
        assert_eq!(alone[(45 * w + 60) * 4 + 3], 255);
        assert_eq!(strokes_fingerprint(&edit.strokes[..1]) == strokes_fingerprint(&[]), false);
    }
    #[test]
    fn a_carried_patch_sits_on_the_pixels_of_the_frame_it_lands_in() {
        // A plane: any resample of it is exact, so a patch that lands half a
        // pixel out shows as a step of about 145.
        let (w, h) = (160usize, 120usize);
        let src: Vec<u16> = (0..w * h)
            .flat_map(|p| {
                let v = (200 * (p % w) + 90 * (p / w)) as u16;
                [v, v / 2, 60000 - v, 65535]
            })
            .collect();
        let geo = |rot90, fine_angle| EditParams { rot90, fine_angle, ..EditParams::default() };
        for (from, to) in [
            (geo(0, 0.0), geo(0, 5.0)),
            (geo(0, 5.0), geo(0, 0.0)),
            (geo(0, -3.0), geo(0, 6.0)),
            (geo(1, 0.0), geo(0, 4.0)),
            (geo(3, 4.0), geo(1, 0.0)),
            (geo(2, 2.0), geo(1, -7.0)),
            (geo(1, 0.0), geo(2, 0.0)),
        ] {
            let (f, w0, h0) = geometry_wide(&src, w, h, &from);
            // The middle of that frame as a patch, with its ring of context.
            let (px, py, pw, ph) = (w0 * 2 / 5, h0 * 2 / 5, w0 / 5, h0 / 5);
            let mut patch = crate::fill::Patch { x: px, y: py, w: pw, h: ph, rgba: Vec::new(), cover: vec![0; pw * ph] };
            for y in 0..ph {
                patch.rgba.extend_from_slice(&f[((py + y) * w0 + px) * 4..((py + y) * w0 + px + pw) * 4]);
                for x in 0..pw {
                    patch.cover[y * pw + x] = (x > 0 && y > 0 && x + 1 < pw && y + 1 < ph) as u8;
                }
            }
            let (t, tw, th) = geometry_wide(&src, w, h, &to);
            let mut got = t.clone().into_owned();
            assert!(carry_patch(&mut got, tw, th, &patch, w, h, &from, &to));
            let worst = got.iter().zip(t.iter()).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
            assert!(worst <= 4, "{}/{} to {}/{}: {worst}", from.rot90, from.fine_angle, to.rot90, to.fine_angle);
        }
    }
}
