//! Levels adjustment, ported from Compositor (MIT, (c) Robbie Tilton):
//! Compositor/Document/Levels.swift + LevelsAutomatic.swift and
//! Compositor/Rendering/LevelsPixels.c. Math is kept identical to the
//! original; the pixel loop differs only because web ImageData is
//! non-premultiplied, so the C version's unpremultiply step is unnecessary.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelRange {
    pub black: f64,
    pub gamma: f64,
    pub white: f64,
    pub output_black: f64,
    pub output_white: f64,
}

impl Default for LevelRange {
    fn default() -> Self {
        Self { black: 0.0, gamma: 1.0, white: 255.0, output_black: 0.0, output_white: 255.0 }
    }
}

impl LevelRange {
    pub fn normalized(&self) -> Self {
        fn clamp(n: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
            if n.is_finite() { n.min(hi).max(lo) } else { fallback }
        }
        let mut r = *self;
        r.black = clamp(r.black, 0.0, 254.0, 0.0);
        r.white = clamp(r.white, r.black + 1.0, 255.0, 255.0);
        r.gamma = clamp(r.gamma, 0.1, 9.99, 1.0);
        r.output_black = clamp(r.output_black, 0.0, 255.0, 0.0);
        r.output_white = clamp(r.output_white, 0.0, 255.0, 255.0);
        r
    }

    /// Input/output in 0..=255.
    pub fn apply(&self, value: f64) -> f64 {
        let s = self.normalized();
        let input = ((value - s.black) / (s.white - s.black)).clamp(0.0, 1.0);
        s.output_black + input.powf(1.0 / s.gamma) * (s.output_white - s.output_black)
    }
}

/// Channel order matches Compositor: 0 = composite RGB, 1..=3 = R/G/B.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LevelsSettings {
    pub ranges: [LevelRange; 4],
}

impl LevelsSettings {
    pub fn is_identity(&self) -> bool {
        self.ranges.iter().all(|r| r.normalized() == LevelRange::default())
    }

    /// Channel adjustment first, then the composite RGB range (as in Compositor).
    /// `channel` is 1..=3 for R/G/B.
    pub fn apply(&self, value: f64, channel: usize) -> f64 {
        self.ranges[0].apply(self.ranges[channel].apply(value))
    }

    /// Per-channel LUTs for the pixel loop: [R,G,B] × 256, output 0..=255.
    pub fn build_tables(&self) -> [[f32; 256]; 3] {
        let mut tables = [[0f32; 256]; 3];
        for ch in 0..3 {
            for (v, slot) in tables[ch].iter_mut().enumerate() {
                *slot = self.apply(v as f64, ch + 1) as f32;
            }
        }
        tables
    }
}

/// Apply LUTs in place to a non-premultiplied RGBA buffer. Port of
/// levels_apply() minus the premultiplied-alpha handling.
pub fn levels_apply(data: &mut [u8], tables: &[[f32; 256]; 3]) {
    for px in data.chunks_exact_mut(4) {
        if px[3] == 0 {
            continue;
        }
        for ch in 0..3 {
            let x = px[ch] as f32;
            let lo = x as usize;
            let hi = (lo + 1).min(255);
            let frac = x - lo as f32;
            let table = &tables[ch];
            px[ch] = (table[lo] + (table[hi] - table[lo]) * frac)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
    }
}

/// Apply LUTs through a per-pixel mask (0 = untouched, 255 = full).
pub fn levels_apply_masked(data: &mut [u8], mask: &[u8], tables: &[[f32; 256]; 3]) {
    let mut adjusted = data.to_vec();
    levels_apply(&mut adjusted, tables);
    for (i, px) in data.chunks_exact_mut(4).enumerate() {
        let m = mask[i] as u32;
        if m == 0 {
            continue;
        }
        for ch in 0..3 {
            let orig = px[ch] as u32;
            let new = adjusted[i * 4 + ch] as u32;
            px[ch] = ((orig * (255 - m) + new * m + 127) / 255) as u8;
        }
    }
}

/// Channel histograms plus a composite (mean of channels), matching
/// levels_histogram(). Returns [RGB, R, G, B].
pub fn histogram(data: &[u8]) -> [[f64; 256]; 4] {
    let mut bins = [[0f64; 256]; 4];
    for px in data.chunks_exact(4) {
        if px[3] == 0 {
            continue;
        }
        for ch in 0..3 {
            let v = px[ch] as usize;
            bins[ch + 1][v] += 1.0;
            bins[0][v] += 1.0 / 3.0;
        }
    }
    bins
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoMode {
    /// Shared interval across channels, preserves channel relationships.
    Contrast,
    /// Per-channel endpoint stretch.
    Color,
    /// Per-channel stretch plus neutralizing gamma.
    ColorNeutral,
}

/// Port of LevelsAuto.settings(histogram:). `hist` is [RGB, R, G, B].
pub fn auto_settings(hist: &[[f64; 256]; 4], mode: AutoMode) -> LevelsSettings {
    fn endpoints(bins: &[f64; 256]) -> Option<(f64, f64)> {
        let total: f64 = bins.iter().sum();
        if total <= 0.0 {
            return None;
        }
        let mut low = 0usize;
        let mut high = 255usize;
        let mut sum = 0.0;
        for (i, &b) in bins.iter().enumerate() {
            sum += b;
            if sum > total * 0.001 {
                low = i;
                break;
            }
        }
        let mut sum = 0.0;
        for (i, &b) in bins.iter().enumerate().rev() {
            sum += b;
            if sum > total * 0.001 {
                high = i;
                break;
            }
        }
        (low < high).then_some((low as f64, high as f64))
    }

    let mut result = LevelsSettings::default();
    match mode {
        AutoMode::Contrast => {
            let limits: Vec<(f64, f64)> = hist[1..].iter().filter_map(endpoints).collect();
            let low = limits.iter().map(|l| l.0).fold(f64::INFINITY, f64::min);
            let high = limits.iter().map(|l| l.1).fold(f64::NEG_INFINITY, f64::max);
            if low < high {
                result.ranges[0] = LevelRange { black: low, white: high, ..Default::default() };
            }
        }
        _ => {
            for c in 1..=3 {
                let Some((low, high)) = endpoints(&hist[c]) else { continue };
                let mut range = LevelRange { black: low, white: high, ..Default::default() };
                if mode == AutoMode::ColorNeutral {
                    let total: f64 = hist[c].iter().sum();
                    let mean = hist[c]
                        .iter()
                        .enumerate()
                        .map(|(i, &b)| range.apply(i as f64) / 255.0 * b)
                        .sum::<f64>()
                        / total;
                    if mean > 0.0 && mean < 1.0 {
                        range.gamma = (mean.ln() / 0.5f64.ln()).clamp(0.1, 9.99);
                    }
                }
                result.ranges[c] = range;
            }
        }
    }
    result
}

/// Display-only vertical scaling (port of LevelsHistogramDisplay.scale): keep
/// linear ratios but cap isolated spikes at 4x the interior 95th percentile.
pub fn histogram_display_scale(bins: &[f64; 256]) -> f64 {
    let peak = bins.iter().filter(|b| b.is_finite() && **b > 0.0).fold(0.0f64, |a, &b| a.max(b));
    if peak <= 0.0 {
        return 0.0;
    }
    let mut interior: Vec<f64> = bins[1..255].iter().copied().filter(|b| b.is_finite() && *b > 0.0).collect();
    if interior.is_empty() {
        return peak;
    }
    interior.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let typical = interior[((interior.len() - 1) as f64 * 0.95) as usize];
    peak.min(typical * 4.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_noop() {
        let s = LevelsSettings::default();
        assert!(s.is_identity());
        let tables = s.build_tables();
        let mut px = vec![10u8, 128, 250, 255, 0, 0, 0, 0];
        levels_apply(&mut px, &tables);
        assert_eq!(&px[..4], &[10, 128, 250, 255]);
        assert_eq!(&px[4..], &[0, 0, 0, 0]); // fully transparent untouched
    }

    #[test]
    fn endpoint_stretch() {
        let mut s = LevelsSettings::default();
        for c in 1..=3 {
            s.ranges[c] = LevelRange { black: 50.0, white: 200.0, ..Default::default() };
        }
        let tables = s.build_tables();
        let mut px = vec![50u8, 125, 200, 255, 10, 220, 128, 255];
        levels_apply(&mut px, &tables);
        assert_eq!(px[0], 0); // black point → 0
        assert_eq!(px[2], 255); // white point → 255
        assert!((px[1] as i32 - 128).abs() <= 1); // mid maps linearly
        assert_eq!(px[3], 255); // alpha untouched
        assert_eq!(px[4], 0); // below black clamps
        assert_eq!(px[5], 255); // above white clamps
    }

    #[test]
    fn gamma_midtones() {
        let mut r = LevelRange::default();
        r.gamma = 2.0;
        // input 128/255 ≈ 0.502; output = 0.502^(1/2) ≈ 0.7086 → ~181
        let out = r.apply(128.0);
        assert!((out - 181.0).abs() < 1.5);
    }

    #[test]
    fn auto_contrast_shared_interval() {
        let mut hist = [[0f64; 256]; 4];
        // R: 10..200, G: 30..220, B: 5..250 (uniform)
        for i in 10..=200 { hist[1][i] = 1.0; }
        for i in 30..=220 { hist[2][i] = 1.0; }
        for i in 5..=250 { hist[3][i] = 1.0; }
        let s = auto_settings(&hist, AutoMode::Contrast);
        // shared interval = min of lows, max of highs
        assert!((s.ranges[0].black - 5.0).abs() < 2.0);
        assert!((s.ranges[0].white - 250.0).abs() < 2.0);
        assert_eq!(s.ranges[1], LevelRange::default()); // channels untouched
    }

    #[test]
    fn auto_color_per_channel() {
        let mut hist = [[0f64; 256]; 4];
        for i in 10..=200 { hist[1][i] = 1.0; }
        for i in 30..=220 { hist[2][i] = 1.0; }
        for i in 5..=250 { hist[3][i] = 1.0; }
        let s = auto_settings(&hist, AutoMode::Color);
        assert!((s.ranges[1].black - 10.0).abs() < 2.0);
        assert!((s.ranges[2].white - 220.0).abs() < 2.0);
        assert!((s.ranges[3].black - 5.0).abs() < 2.0);
        assert_eq!(s.ranges[0], LevelRange::default()); // composite untouched
    }

    #[test]
    fn histogram_counts() {
        // 2 opaque pixels: (255,0,0) and (0,255,0)
        let data = [255u8, 0, 0, 255, 0, 255, 0, 255];
        let h = histogram(&data);
        assert_eq!(h[1][255], 1.0);
        assert_eq!(h[2][255], 1.0);
        assert_eq!(h[3][0], 2.0);
        assert!((h[0][255] - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn masked_apply_blends() {
        let mut s = LevelsSettings::default();
        for c in 1..=3 {
            s.ranges[c] = LevelRange { black: 0.0, white: 128.0, ..Default::default() };
        }
        let tables = s.build_tables();
        // two identical pixels, mask 255 on first, 0 on second
        let mut px = vec![64u8, 64, 64, 255, 64, 64, 64, 255];
        levels_apply_masked(&mut px, &[255u8, 0], &tables);
        assert_eq!(px[0], 128); // stretched
        assert_eq!(px[4], 64); // untouched
    }

    #[test]
    fn display_scale_caps_spike() {
        let mut bins = [1.0f64; 256];
        bins[0] = 10_000.0; // solid-color spike at the edge
        let scale = histogram_display_scale(&bins);
        // interior 95th percentile is 1.0, cap = 4.0
        assert_eq!(scale, 4.0);
    }
}
