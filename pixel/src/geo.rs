//! Geometry pipeline shared by app and worker: rotate/crop math, target
//! sizing, stroke rasterization. Pure Rust — canvas resampling lives in the
//! caller (DOM canvas in the app, OffscreenCanvas in the worker).

use crate::ops;
use crate::types::EditParams;

/// Apply rotation (quarter-turns then fine angle with auto-crop) to a buffer.
pub fn geometry(pixels: &[u8], w: usize, h: usize, edit: &EditParams) -> (Vec<u8>, usize, usize) {
    let (p, w, h) = if edit.rot90 % 4 != 0 {
        ops::rotate_90(pixels, w, h, edit.rot90)
    } else {
        (pixels.to_vec(), w, h)
    };
    if edit.fine_angle.abs() > 0.01 {
        ops::rotate_auto_crop(&p, w, h, edit.fine_angle)
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
