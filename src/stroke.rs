//! Dab placement for stamp-style brush strokes.
//!
//! A stamp brush walks a freehand polyline and places a shape ("dab") every
//! `spacing * size` pixels. This module is pure math over pixel-space points
//! so it is testable natively; canvas rendering lives in main.rs.

/// One stamp along a stroke: center in pixels and the stroke's travel angle
/// (radians, screen space, 0 = +x) at that point.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Dab {
    pub x: f32,
    pub y: f32,
    pub angle: f32,
}

/// Walk `points` placing a dab every `spacing_px` of travel. Always emits a
/// dab at the first point (a click with no drag still stamps once). Does not
/// force a dab at the final point unless it lands on the spacing grid.
pub fn dab_positions(points: &[(f32, f32)], spacing_px: f32) -> Vec<Dab> {
    let mut out = Vec::new();
    let Some(&(x0, y0)) = points.first() else { return out };
    let spacing = spacing_px.max(0.5);
    out.push(Dab { x: x0, y: y0, angle: 0.0 });
    let mut prev = (x0, y0);
    let mut since_last = 0.0f32;
    let mut first_angle: Option<f32> = None;
    for &(x, y) in &points[1..] {
        let dx = x - prev.0;
        let dy = y - prev.1;
        let seg_len = (dx * dx + dy * dy).sqrt();
        if seg_len <= 1e-6 {
            continue;
        }
        let angle = dy.atan2(dx);
        if first_angle.is_none() {
            first_angle = Some(angle);
            if let Some(d) = out.first_mut() {
                d.angle = angle;
            }
        }
        let mut travelled = spacing - since_last;
        while travelled <= seg_len {
            let t = travelled / seg_len;
            out.push(Dab { x: prev.0 + dx * t, y: prev.1 + dy * t, angle });
            travelled += spacing;
        }
        since_last = seg_len - (travelled - spacing);
        prev = (x, y);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(x0: f32, y0: f32, x1: f32, y1: f32, n: usize) -> Vec<(f32, f32)> {
        (0..=n)
            .map(|i| {
                let t = i as f32 / n as f32;
                (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
            })
            .collect()
    }

    #[test]
    fn empty_stroke_has_no_dabs() {
        assert!(dab_positions(&[], 10.0).is_empty());
    }

    #[test]
    fn click_stamps_once() {
        let dabs = dab_positions(&[(5.0, 7.0)], 10.0);
        assert_eq!(dabs.len(), 1);
        assert_eq!((dabs[0].x, dabs[0].y), (5.0, 7.0));
    }

    #[test]
    fn dabs_are_spacing_apart_on_straight_line() {
        // 100px line, spacing 10 -> dab at 0, 10, 20, ..., 100 = 11 dabs.
        let dabs = dab_positions(&line(0.0, 0.0, 100.0, 0.0, 50), 10.0);
        assert_eq!(dabs.len(), 11, "got {:?}", dabs.iter().map(|d| d.x).collect::<Vec<_>>());
        for w in dabs.windows(2) {
            let d = ((w[1].x - w[0].x).powi(2) + (w[1].y - w[0].y).powi(2)).sqrt();
            assert!((d - 10.0).abs() < 0.01, "spacing was {d}");
        }
    }

    #[test]
    fn spacing_carries_across_polyline_kinks() {
        // L shape: 15px right then 15px down, spacing 10 -> dabs at
        // (0,0), (10,0), (15,5)... total 4 (at arc lengths 0,10,20,30).
        let pts = vec![(0.0, 0.0), (15.0, 0.0), (15.0, 15.0)];
        let dabs = dab_positions(&pts, 10.0);
        assert_eq!(dabs.len(), 4, "got {dabs:?}");
        assert!((dabs[2].x - 15.0).abs() < 0.01 && (dabs[2].y - 5.0).abs() < 0.01);
    }

    #[test]
    fn angle_follows_travel_direction() {
        let pts = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        let dabs = dab_positions(&pts, 5.0);
        let horiz = dabs.iter().find(|d| d.x > 4.0 && d.y.abs() < 0.01).unwrap();
        assert!(horiz.angle.abs() < 0.01);
        let vert = dabs.iter().rev().find(|d| d.y > 4.0).unwrap();
        assert!((vert.angle - std::f32::consts::FRAC_PI_2).abs() < 0.01);
    }

    #[test]
    fn first_dab_angle_matches_first_segment() {
        let dabs = dab_positions(&[(0.0, 0.0), (0.0, 50.0)], 10.0);
        assert!((dabs[0].angle - std::f32::consts::FRAC_PI_2).abs() < 0.01);
    }

    #[test]
    fn short_stroke_still_stamps() {
        // Total travel 6px with spacing 20: only the initial dab.
        assert_eq!(dab_positions(&line(0.0, 0.0, 6.0, 0.0, 6), 20.0).len(), 1);
    }

    #[test]
    fn zero_spacing_is_clamped_not_infinite() {
        let dabs = dab_positions(&line(0.0, 0.0, 100.0, 0.0, 10), 0.0);
        assert!(dabs.len() >= 200 && dabs.len() <= 202, "got {}", dabs.len());
    }

    #[test]
    fn duplicate_points_do_not_stall() {
        let mut pts = vec![(0.0, 0.0); 5];
        pts.extend(line(0.0, 0.0, 30.0, 0.0, 10));
        let dabs = dab_positions(&pts, 10.0);
        assert_eq!(dabs.len(), 4);
    }
}
