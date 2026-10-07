//! Tone curves, ported from Compositor (MIT, (c) Robbie Tilton):
//! Compositor/Document/Curves.swift. Shape-preserving cubic Hermite
//! interpolation (Fritsch-Carlson slope limiting) so the curve never
//! overshoots between control points. Pixel application reuses the levels
//! LUT loop, as the original does (Curves.swift builds a table and calls
//! levels_apply).

use crate::levels;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CurvePoint {
    pub x: f64,
    pub y: f64,
}

/// Channel order matches levels: 0 = composite RGB, 1..=3 = R/G/B.
/// Each channel's points run from (0,_) to (255,_) with strictly rising x.
#[derive(Clone, Debug, PartialEq)]
pub struct CurvesSettings {
    pub channels: [Vec<CurvePoint>; 4],
}

impl Default for CurvesSettings {
    fn default() -> Self {
        let line = || vec![CurvePoint { x: 0.0, y: 0.0 }, CurvePoint { x: 255.0, y: 255.0 }];
        Self { channels: [line(), line(), line(), line()] }
    }
}

impl CurvesSettings {
    pub fn is_valid(&self) -> bool {
        self.channels.iter().all(|p| {
            (2..=32).contains(&p.len())
                && p.first().map(|p| p.x) == Some(0.0)
                && p.last().map(|p| p.x) == Some(255.0)
                && p.iter().all(|p| {
                    p.x.is_finite() && p.y.is_finite()
                        && (0.0..=255.0).contains(&p.x) && (0.0..=255.0).contains(&p.y)
                })
                && p.windows(2).all(|w| w[0].x < w[1].x)
        })
    }

    /// Every point on the diagonal means the Hermite evaluation is the
    /// identity line (equal secant slopes -> harmonic-mean tangents == 1).
    pub fn is_identity(&self) -> bool {
        self.channels
            .iter()
            .all(|p| p.first().map(|p| (p.x, p.y)) == Some((0.0, 0.0))
                && p.last().map(|p| (p.x, p.y)) == Some((255.0, 255.0))
                && p.iter().all(|p| p.x == p.y))
    }

    /// Curve value at input x (0..=255) for one channel, clamped to 0..=255.
    pub fn value(&self, x: f64, channel: usize) -> f64 {
        let p = &self.channels[channel];
        let i = p.iter().rposition(|pt| pt.x <= x).unwrap_or(0).min(p.len() - 2);
        let d: Vec<f64> = p.windows(2).map(|w| (w[1].y - w[0].y) / (w[1].x - w[0].x)).collect();
        let slope = |j: usize| -> f64 {
            if j == 0 {
                return d[0];
            }
            if j == p.len() - 1 {
                return *d.last().unwrap();
            }
            if d[j - 1] * d[j] <= 0.0 {
                return 0.0;
            }
            2.0 / (1.0 / d[j - 1] + 1.0 / d[j])
        };
        let h = p[i + 1].x - p[i].x;
        let t = ((x - p[i].x) / h).clamp(0.0, 1.0);
        let y = (2.0 * t * t * t - 3.0 * t * t + 1.0) * p[i].y
            + (t * t * t - 2.0 * t * t + t) * h * slope(i)
            + (-2.0 * t * t * t + 3.0 * t * t) * p[i + 1].y
            + (t * t * t - t * t) * h * slope(i + 1);
        y.clamp(0.0, 255.0)
    }

    /// Per-channel LUTs for the pixel loop: [R,G,B] x 256, output 0..=255.
    /// Channel curve first, then composite (as in Compositor).
    pub fn build_tables(&self) -> [[f32; 256]; 3] {
        let mut tables = [[0f32; 256]; 3];
        if !self.is_valid() {
            return Self::default().build_tables();
        }
        for ch in 0..3 {
            for (v, slot) in tables[ch].iter_mut().enumerate() {
                *slot = self.value(self.value(v as f64, ch + 1), 0) as f32;
            }
        }
        tables
    }

    pub fn apply(&self, data: &mut [u8]) {
        if !self.is_identity() {
            levels::levels_apply(data, &self.build_tables());
        }
    }

    pub fn apply_masked(&self, data: &mut [u8], mask: &[u8]) {
        if !self.is_identity() {
            levels::levels_apply_masked(data, mask, &self.build_tables());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(ch: usize, pts: Vec<(f64, f64)>) -> CurvesSettings {
        let mut s = CurvesSettings::default();
        s.channels[ch] = pts.into_iter().map(|(x, y)| CurvePoint { x, y }).collect();
        s
    }

    #[test]
    fn identity_is_noop() {
        let s = CurvesSettings::default();
        assert!(s.is_valid());
        assert!(s.is_identity());
        let tables = s.build_tables();
        let mut px = vec![10u8, 128, 250, 255];
        levels::levels_apply(&mut px, &tables);
        assert_eq!(&px, &[10, 128, 250, 255]);
    }

    #[test]
    fn collinear_points_are_identity() {
        let s = settings(0, vec![(0.0, 0.0), (64.0, 64.0), (200.0, 200.0), (255.0, 255.0)]);
        assert!(s.is_identity());
        for x in [0.0, 31.5, 128.0, 254.0] {
            assert!((s.value(x, 0) - x).abs() < 0.5, "value({x}) = {}", s.value(x, 0));
        }
    }

    #[test]
    fn midpoint_lifts_midtones() {
        let s = settings(0, vec![(0.0, 0.0), (128.0, 192.0), (255.0, 255.0)]);
        assert!((s.value(128.0, 0) - 192.0).abs() < 1e-9);
        assert!(s.value(64.0, 0) > 96.0 && s.value(64.0, 0) < 192.0);
        // Endpoints pinned.
        assert_eq!(s.value(0.0, 0), 0.0);
        assert_eq!(s.value(255.0, 0), 255.0);
    }

    #[test]
    fn no_overshoot_between_handles() {
        // Sharp peak: secant signs flip, so tangents at the peak go to 0 and
        // the segment 32..64 must decrease monotonically, never overshooting.
        let s = settings(0, vec![(0.0, 0.0), (32.0, 255.0), (64.0, 0.0), (255.0, 255.0)]);
        let mut prev = s.value(32.0, 0);
        for x in 33..=64 {
            let v = s.value(x as f64, 0);
            assert!(v <= prev + 1e-9, "not monotone at {x}: {prev} -> {v}");
            assert!(v >= 0.0);
            prev = v;
        }
    }

    #[test]
    fn composite_applies_after_channel() {
        // R channel halves everything; composite identity.
        let s = settings(1, vec![(0.0, 0.0), (255.0, 128.0)]);
        let tables = s.build_tables();
        assert!((tables[0][255] - 128.0).abs() <= 1.0);
        assert_eq!(tables[1][255], 255.0); // G untouched
        // Composite maps 0 -> 128 (floor lift); R channel identity.
        let s = settings(0, vec![(0.0, 128.0), (255.0, 255.0)]);
        let tables = s.build_tables();
        assert!((tables[0][0] - 128.0).abs() <= 1.0);
    }

    #[test]
    fn apply_matches_value() {
        let s = settings(0, vec![(0.0, 0.0), (100.0, 180.0), (255.0, 255.0)]);
        let tables = s.build_tables();
        let mut px = vec![100u8, 30, 220, 255];
        levels::levels_apply(&mut px, &tables);
        for (ch, &v) in [100.0f64, 30.0, 220.0].iter().enumerate() {
            let direct = s.value(s.value(v, ch + 1), 0).round() as i32;
            assert!((px[ch] as i32 - direct).abs() <= 1);
        }
        assert_eq!(px[3], 255);
    }

    #[test]
    fn invalid_falls_back_to_identity() {
        let mut s = CurvesSettings::default();
        s.channels[0] = vec![CurvePoint { x: 10.0, y: 0.0 }, CurvePoint { x: 255.0, y: 255.0 }];
        assert!(!s.is_valid());
        let tables = s.build_tables();
        for (v, &t) in tables[0].iter().enumerate() {
            assert_eq!(t, v as f32);
        }
    }
}
