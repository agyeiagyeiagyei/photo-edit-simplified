//! Selection-brush rasterization.
//!
//! A stroke is a soft-edged disc walked along the pointer path. The coverage
//! of one stroke is accumulated first (max over dabs) so overlapping dabs at
//! a soft edge do not build up: one pass over a pixel yields the same value as
//! any other. Add keeps the brighter of mask and coverage; Remove scales the
//! mask down by the coverage, so at softness 0 an Add followed by Remove over
//! the same path returns an empty mask exactly.

use crate::stroke;

/// Bounding box of a stroke's coverage, in mask pixels, plus the coverage.
#[derive(Clone, PartialEq, Debug)]
pub struct StrokeMask {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

/// Coverage of a stroke over a `mask_w` × `mask_h` mask. Points are in mask
/// pixels; `radius` is in mask pixels; `softness` 0 = hard edge, 1 = falloff
/// from the centre. Returns None for an empty path or a non-positive radius.
pub fn stroke_mask(
    mask_w: usize,
    mask_h: usize,
    points: &[(f32, f32)],
    radius: f32,
    softness: f32,
) -> Option<StrokeMask> {
    if mask_w == 0 || mask_h == 0 || points.is_empty() || radius <= 0.0 {
        return None;
    }
    let (mut x0, mut y0) = (f32::INFINITY, f32::INFINITY);
    let (mut x1, mut y1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for &(x, y) in points {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let left = ((x0 - radius).floor().max(0.0) as usize).min(mask_w);
    let top = ((y0 - radius).floor().max(0.0) as usize).min(mask_h);
    let right = ((x1 + radius).ceil().max(0.0) as usize + 1).min(mask_w);
    let bottom = ((y1 + radius).ceil().max(0.0) as usize + 1).min(mask_h);
    if right <= left || bottom <= top {
        return None;
    }
    let w = right - left;
    let h = bottom - top;
    let mut data = vec![0u8; w * h];
    let spacing = (radius * 0.25).max(0.75);
    let dabs = stroke::dab_positions(points, spacing);
    // dab_positions does not force a dab at the final point; a short drag can
    // end between dabs, so stamp the endpoint explicitly.
    for dab in dabs
        .iter()
        .map(|d| (d.x, d.y))
        .chain(points.last().copied())
    {
        stamp(&mut data, w, h, left, top, dab.0, dab.1, radius, softness);
    }
    Some(StrokeMask { x: left, y: top, w, h, data })
}

fn stamp(
    data: &mut [u8],
    w: usize,
    h: usize,
    origin_x: usize,
    origin_y: usize,
    cx: f32,
    cy: f32,
    radius: f32,
    softness: f32,
) {
    let x0 = ((cx - radius).floor().max(0.0) as usize).saturating_sub(origin_x);
    let y0 = ((cy - radius).floor().max(0.0) as usize).saturating_sub(origin_y);
    let x1 = (((cx + radius).ceil().max(0.0) as usize) + 1)
        .min(origin_x + w)
        .saturating_sub(origin_x);
    let y1 = (((cy + radius).ceil().max(0.0) as usize) + 1)
        .min(origin_y + h)
        .saturating_sub(origin_y);
    for py in y0..y1 {
        let dy = (origin_y + py) as f32 + 0.5 - cy;
        for px in x0..x1 {
            let dx = (origin_x + px) as f32 + 0.5 - cx;
            let d = (dx * dx + dy * dy).sqrt() / radius;
            if d >= 1.0 {
                continue;
            }
            let a = if softness <= 0.001 {
                255
            } else {
                let t = ((1.0 - d) / softness).clamp(0.0, 1.0);
                (t * t * (3.0 - 2.0 * t) * 255.0).round() as u8
            };
            let v = &mut data[py * w + px];
            if a > *v {
                *v = a;
            }
        }
    }
}

/// Combine a stroke's coverage with a mask. Add takes the brighter value;
/// Remove scales by the inverse coverage. Both clamp to 0..=255.
pub fn apply_stroke(mask: &mut [u8], mask_w: usize, mask_h: usize, stroke: &StrokeMask, add: bool) {
    if stroke.w == 0 || stroke.h == 0 || stroke.x + stroke.w > mask_w || stroke.y + stroke.h > mask_h {
        return;
    }
    for y in 0..stroke.h {
        let row = (stroke.y + y) * mask_w + stroke.x;
        let srow = y * stroke.w;
        for x in 0..stroke.w {
            let a = stroke.data[srow + x] as u16;
            let v = &mut mask[row + x];
            if add {
                *v = (*v).max(a as u8);
            } else {
                *v = ((*v as u16 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(w: usize, h: usize) -> Vec<u8> {
        vec![0u8; w * h]
    }

    #[test]
    fn hard_stroke_fills_inside_and_leaves_outside_alone() {
        let w = 64;
        let mut m = mask(w, w);
        let s = stroke_mask(w, w, &[(32.0, 32.0)], 10.0, 0.0).unwrap();
        apply_stroke(&mut m, w, w, &s, true);
        assert_eq!(m[32 * w + 32], 255);
        assert_eq!(m[32 * w + (32 + 9)], 255);
        assert_eq!(m[32 * w + (32 + 11)], 0);
        assert_eq!(m[0], 0);
    }

    #[test]
    fn soft_edge_falls_off_monotonically() {
        let w = 64;
        let mut m = mask(w, w);
        // Click on a pixel centre so the centre sample is exactly d = 0.
        let s = stroke_mask(w, w, &[(32.5, 32.5)], 12.0, 1.0).unwrap();
        apply_stroke(&mut m, w, w, &s, true);
        let center = m[32 * w + 32];
        let mid = m[32 * w + 36];
        let edge = m[32 * w + 42];
        assert_eq!(center, 255);
        assert!(mid < center && mid > edge, "center={center} mid={mid} edge={edge}");
        assert_eq!(m[32 * w + 46], 0);
    }

    #[test]
    fn add_then_remove_at_hardness_restores_empty_mask() {
        let w = 48;
        let h = 40;
        let mut m = mask(w, h);
        let pts = [(12.0, 12.0), (30.0, 26.0)];
        let s = stroke_mask(w, h, &pts, 6.0, 0.0).unwrap();
        apply_stroke(&mut m, w, h, &s, true);
        assert!(m.iter().any(|&v| v == 255));
        apply_stroke(&mut m, w, h, &s, false);
        assert!(m.iter().all(|&v| v == 0));
    }

    #[test]
    fn add_clamps_and_remove_scales_existing_values() {
        let w = 8;
        let mut m = vec![200u8; w * w];
        let s = stroke_mask(w, w, &[(4.0, 4.0)], 3.0, 0.0).unwrap();
        apply_stroke(&mut m, w, w, &s, true);
        assert_eq!(m[4 * w + 4], 255);
        apply_stroke(&mut m, w, w, &s, false);
        assert_eq!(m[4 * w + 4], 0);
        assert_eq!(m[0], 200);
    }

    #[test]
    fn coverage_overlaps_do_not_accumulate() {
        // Two dabs at the same spot must produce one coat, not two.
        let w = 32;
        let once = stroke_mask(w, w, &[(16.0, 16.0)], 8.0, 1.0).unwrap();
        let twice = stroke_mask(w, w, &[(16.0, 16.0), (16.0, 16.0)], 8.0, 1.0).unwrap();
        assert_eq!(once.data, twice.data);
    }

    #[test]
    fn stroke_clips_at_mask_edges() {
        let w = 24;
        let h = 20;
        let s = stroke_mask(w, h, &[(-5.0, -5.0), (3.0, 4.0)], 6.0, 0.2).unwrap();
        assert!(s.x == 0 && s.y == 0);
        assert!(s.x + s.w <= w && s.y + s.h <= h);
        let mut m = mask(w, h);
        apply_stroke(&mut m, w, h, &s, true);
        assert!(m.iter().any(|&v| v > 0));
    }

    #[test]
    fn dragging_between_points_covers_the_segment() {
        let w = 64;
        let mut m = mask(w, w);
        let s = stroke_mask(w, w, &[(8.0, 32.0), (56.0, 32.0)], 4.0, 0.0).unwrap();
        apply_stroke(&mut m, w, w, &s, true);
        for x in [10, 20, 32, 44, 54] {
            assert_eq!(m[32 * w + x], 255, "gap at x={x}");
        }
    }
}
