//! RAW develop adjustments: edge-aware denoise, white balance, exposure,
//! highlight recovery, shadow lift / noise suppression, and capture
//! sharpening.
//!
//! The tone stages run in linear light — sRGB is decoded in, adjusted,
//! re-encoded out — and happen before the gamma-space colour controls. The
//! base image is the camera's rendering, so temperature is a relative
//! correction: the neutral point (5500 K) reproduces the camera's as-shot
//! white balance, and moving away from it applies a blackbody-derived von
//! Kries gain.

use std::sync::OnceLock;

use crate::types::{RawAdjust, NEUTRAL_K};
use crate::wide;

/// Linear value where highlight compression starts.
const HL_KNEE: f32 = 0.6;

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Blackbody colour as linear sRGB (Tanner Helland's fit, decoded).
fn blackbody_linear(k: f32) -> [f32; 3] {
    let t = k.clamp(1000.0, 40000.0) / 100.0;
    let (r, g, b) = if t <= 66.0 {
        let g = 99.4708025861 * t.ln() - 161.1195681661;
        let b = if t <= 19.0 {
            0.0
        } else {
            138.5177312231 * (t - 10.0).ln() - 305.0447927307
        };
        (255.0, g, b)
    } else {
        let r = 329.698727446 * (t - 60.0).powf(-0.1332047592);
        let g = 288.1221695283 * (t - 60.0).powf(-0.0755148492);
        (r, g, 255.0)
    };
    [
        srgb_to_linear((r / 255.0).clamp(0.0, 1.0)),
        srgb_to_linear((g / 255.0).clamp(0.0, 1.0)),
        srgb_to_linear((b / 255.0).clamp(0.0, 1.0)),
    ]
}

/// von Kries gains that would make `temp` look neutral, relative to the
/// neutral reference. Tint moves the green axis: positive = magenta.
pub fn wb_gains(temp: f32, tint: f32) -> [f32; 3] {
    let target = blackbody_linear(temp);
    let base = blackbody_linear(NEUTRAL_K);
    let mut g = [
        base[0] / target[0].max(1e-4),
        base[1] / target[1].max(1e-4),
        base[2] / target[2].max(1e-4),
    ];
    let t = (tint / 100.0).clamp(-1.0, 1.0);
    g[1] *= 1.0 - 0.25 * t;
    g[0] *= 1.0 + 0.08 * t;
    g[2] *= 1.0 + 0.08 * t;
    // Normalise so the correction does not also change exposure.
    let y = 0.2126 * g[0] + 0.7152 * g[1] + 0.0722 * g[2];
    [
        (g[0] / y).clamp(0.2, 5.0),
        (g[1] / y).clamp(0.2, 5.0),
        (g[2] / y).clamp(0.2, 5.0),
    ]
}

fn luma(r: f32, g: f32, b: f32) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn scale_rgb(rgb: &mut [f32; 3], s: f32) {
    rgb[0] *= s;
    rgb[1] *= s;
    rgb[2] *= s;
}

/// 16-bit sRGB decode table, built once per process.
fn dec16() -> &'static [f32] {
    static T: OnceLock<Vec<f32>> = OnceLock::new();
    T.get_or_init(|| {
        (0..65536)
            .map(|i| srgb_to_linear(i as f32 / 65535.0))
            .collect()
    })
}

/// Linear -> 16-bit sRGB, sampled at 1/65536 linear steps. Finer than the
/// 8-bit output needs, so the only rounding that reaches the frame is the
/// final dither.
fn enc16() -> &'static [u16] {
    static T: OnceLock<Vec<u16>> = OnceLock::new();
    T.get_or_init(|| {
        (0..=65536)
            .map(|i| {
                (linear_to_srgb(i as f32 / 65536.0) * 65535.0)
                    .round()
                    .clamp(0.0, 65535.0) as u16
            })
            .collect()
    })
}

/// 3x3 box blur of the sRGB luma, used as the unsharp reference. One u16
/// per pixel so a 24MP develop pass adds 48MB, not another RGBA buffer.
fn blur_luma_wide(buf: &[u16], w: usize, h: usize) -> Vec<u16> {
    let mut horiz = vec![0u16; w * h];
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let x0 = x.saturating_sub(1);
            let x1 = (x + 1).min(w - 1);
            let mut s = 0u32;
            for xx in x0..=x1 {
                let i = (row + xx) * 4;
                s += 54 * buf[i] as u32 + 183 * buf[i + 1] as u32 + 19 * buf[i + 2] as u32;
            }
            horiz[row + x] = ((s >> 8) / (x1 - x0 + 1) as u32) as u16;
        }
    }
    let mut out = vec![0u16; w * h];
    for y in 0..h {
        let y0 = y.saturating_sub(1);
        let y1 = (y + 1).min(h - 1);
        let rows = (y1 - y0 + 1) as u32;
        for x in 0..w {
            let mut s = 0u32;
            for yy in y0..=y1 {
                s += horiz[yy * w + x] as u32;
            }
            out[y * w + x] = (s / rows) as u16;
        }
    }
    out
}

/// Apply the develop stage to a 16-bit sRGB RGBA frame in place. Identity
/// leaves the buffer untouched.
pub fn develop_wide(buf: &mut [u16], w: usize, h: usize, adj: &RawAdjust) {
    if adj.is_identity() || w == 0 || h == 0 {
        return;
    }
    // Spatial denoise first, so sharpening and the tone curves act on the
    // cleaned image rather than amplifying sensor noise. The denoiser is
    // still an 8-bit luma/chroma filter; it is the one stage that narrows
    // here, and #9's denoise task moves it to linear luma.
    if adj.denoise > 0.0 {
        let mut px8: Vec<u8> = buf.iter().map(|&v| ((v as u32 + 128) / 257) as u8).collect();
        crate::denoise::smart_denoise(&mut px8, w, h, (adj.denoise / 100.0).clamp(0.0, 1.0));
        for (dst, &v) in buf.iter_mut().zip(px8.iter()) {
            *dst = v as u16 * 257;
        }
    }
    let sharp = (adj.sharpen / 100.0).clamp(0.0, 1.0);
    let hl = (adj.highlights / 100.0).clamp(0.0, 1.0);
    let sh = (adj.shadows / 100.0).clamp(0.0, 1.0);
    let nr = (adj.noise / 100.0).clamp(0.0, 1.0);
    let gains = wb_gains(adj.temp, adj.tint);
    let exp_gain = 2f32.powf(adj.exposure.clamp(-5.0, 5.0));

    let dec = dec16();
    let enc = enc16();
    let encode = |v: f32| -> u16 { enc[(v.clamp(0.0, 1.0) * 65536.0) as usize] };

    let blur = if sharp > 0.0 { Some(blur_luma_wide(buf, w, h)) } else { None };

    for (i, px) in buf.chunks_exact_mut(4).enumerate() {
        let mut rgb = [
            dec[px[0] as usize] * gains[0] * exp_gain,
            dec[px[1] as usize] * gains[1] * exp_gain,
            dec[px[2] as usize] * gains[2] * exp_gain,
        ];

        // Capture sharpening: unsharp gain on the perceptual luma edge
        // signal, applied multiplicatively so hue is preserved in linear.
        if let Some(bp) = &blur {
            let yb = (54 * px[0] as u32 + 183 * px[1] as u32 + 19 * px[2] as u32) >> 8;
            let d = (yb as f32 - bp[i] as f32) / 65535.0;
            let g = (1.0 + sharp * 3.0 * d).clamp(0.2, 4.0);
            scale_rgb(&mut rgb, g);
        }

        let mut y = luma(rgb[0], rgb[1], rgb[2]);

        // Highlight recovery: exponential shoulder above the knee, blended in
        // by the slider. C1-continuous with the untouched range so no edge
        // appears at the knee.
        if hl > 0.0 && y > HL_KNEE {
            let head = 1.0 - HL_KNEE;
            let over = y - HL_KNEE;
            let compressed = HL_KNEE + head * (1.0 - (-over / head).exp());
            let y2 = y + (compressed - y) * hl;
            scale_rgb(&mut rgb, y2 / y);
            y = y2;
        }

        // Shadow lift: soft toe that fades out by mid-grey. Exactly-black has
        // no colour to scale, so it takes the neutral lifted black point —
        // scaling it would leave it at 0 next to a lifted neighbour.
        if sh > 0.0 {
            let w8 = (1.0 - (y / 0.18).min(1.0)).powi(2);
            let y2 = y + sh * 0.04 * w8;
            if y2 > y {
                if y <= 1e-6 {
                    rgb = [y2, y2, y2];
                } else {
                    scale_rgb(&mut rgb, y2 / y);
                }
                y = y2;
            }
        }

        // Shadow noise: suppress chroma (colour blotches) where it is darkest
        // and sensor noise is most visible; luminance is preserved.
        if nr > 0.0 {
            let dark = 1.0 - (y / 0.12).min(1.0);
            let t = nr * dark * 0.8;
            if t > 0.0 {
                rgb[0] = y + (rgb[0] - y) * (1.0 - t);
                rgb[1] = y + (rgb[1] - y) * (1.0 - t);
                rgb[2] = y + (rgb[2] - y) * (1.0 - t);
            }
        }

        px[0] = encode(rgb[0]);
        px[1] = encode(rgb[1]);
        px[2] = encode(rgb[2]);
    }
}

/// Develop the un-toned 16-bit sRGB RAW frame, white-relative.
///
/// The neutral develop spans only the camera's raw range (a dark frame might
/// top out near 5% linear), so every threshold here is evaluated in units of
/// `white` — the linear level the camera-tone curve maps to preview white.
/// The stages themselves match [`develop_wide`]; denoise and capture
/// sharpening stay on the legacy path and callers gate this one off when
/// either is active.
pub fn develop_neutral_wide(buf: &mut [u16], w: usize, h: usize, adj: &RawAdjust, white: f32) {
    if adj.is_identity() || w == 0 || h == 0 {
        return;
    }
    let white = if white.is_finite() && white > 1e-6 { white } else { 1.0 };
    let hl = (adj.highlights / 100.0).clamp(0.0, 1.0);
    let sh = (adj.shadows / 100.0).clamp(0.0, 1.0);
    let nr = (adj.noise / 100.0).clamp(0.0, 1.0);
    let gains = wb_gains(adj.temp, adj.tint);
    let exp_gain = 2f32.powf(adj.exposure.clamp(-5.0, 5.0));

    let dec = dec16();
    let enc = enc16();
    let encode = |v: f32| -> u16 { enc[(v.clamp(0.0, 1.0) * 65536.0) as usize] };
    let inv = 1.0 / white;

    for px in buf.chunks_exact_mut(4) {
        let mut rgb = [
            dec[px[0] as usize] * inv * gains[0] * exp_gain,
            dec[px[1] as usize] * inv * gains[1] * exp_gain,
            dec[px[2] as usize] * inv * gains[2] * exp_gain,
        ];

        let mut y = luma(rgb[0], rgb[1], rgb[2]);

        // Highlight recovery on unclipped data, before the camera tone curve.
        if hl > 0.0 && y > HL_KNEE {
            let head = 1.0 - HL_KNEE;
            let over = y - HL_KNEE;
            let compressed = HL_KNEE + head * (1.0 - (-over / head).exp());
            let y2 = y + (compressed - y) * hl;
            scale_rgb(&mut rgb, y2 / y);
            y = y2;
        }

        if sh > 0.0 {
            let w8 = (1.0 - (y / 0.18).min(1.0)).powi(2);
            let y2 = y + sh * 0.04 * w8;
            if y2 > y {
                if y <= 1e-6 {
                    rgb = [y2, y2, y2];
                } else {
                    scale_rgb(&mut rgb, y2 / y);
                }
                y = y2;
            }
        }

        if nr > 0.0 {
            let dark = 1.0 - (y / 0.12).min(1.0);
            let t = nr * dark * 0.8;
            if t > 0.0 {
                rgb[0] = y + (rgb[0] - y) * (1.0 - t);
                rgb[1] = y + (rgb[1] - y) * (1.0 - t);
                rgb[2] = y + (rgb[2] - y) * (1.0 - t);
            }
        }

        px[0] = encode(rgb[0] * white);
        px[1] = encode(rgb[1] * white);
        px[2] = encode(rgb[2] * white);
    }
}

/// Expand a camera-tone curve into the 16-bit input table the tone stage
/// indexes. Built once per developed item.
pub fn tone_table(tone: &[u8; 256]) -> Vec<u16> {
    let mut knots = [0f32; 256];
    for (k, &v) in knots.iter_mut().zip(tone.iter()) {
        *k = v as f32;
    }
    crate::wide::widen_table(&knots)
}

/// How far the highlight repair pulls the clip point down, as a fraction of
/// the camera curve's top code.
const TONE_TAIL_DROP: f32 = 0.12;

/// The tone table for a render with `highlights` active. At zero this is
/// [`tone_table`] exactly. Above zero the camera curve's clipped tail is
/// replaced by a chord from an adaptive knee to the same top code: the camera
/// tone saturates the top of the develop range (DSC05659: every code ≥190
/// maps to 252), so a highlight edit applied before it would be invisible.
/// The chord darkens the clipped region by [`TONE_TAIL_DROP`] of the top,
/// spreads its formerly-identical inputs over the freed range, and stays
/// monotone and darken-only — below the knee nothing moves.
pub fn tone_table_for(tone: &[u8; 256], highlights: f32) -> Vec<u16> {
    let hl = (highlights / 100.0).clamp(0.0, 1.0);
    let mut knots = [0f32; 256];
    for (k, &v) in knots.iter_mut().zip(tone.iter()) {
        *k = v as f32;
    }
    if hl > 0.0 {
        if let Some(repaired) = unclip_tail(&knots) {
            for (k, r) in knots.iter_mut().zip(repaired) {
                *k += hl * (r - *k);
            }
        }
    }
    crate::wide::widen_table(&knots)
}

/// The camera curve with its clipped tail replaced by the chord from the
/// nearest knee that frees [`TONE_TAIL_DROP`] of the top at the clip point.
/// None when the curve never reaches a plateau.
fn unclip_tail(tone: &[f32; 256]) -> Option<[f32; 256]> {
    let max = tone[255];
    if max <= 0.0 {
        return None;
    }
    let mut p0 = 256;
    for i in (0..256).rev() {
        if tone[i] < max {
            p0 = i + 1;
            break;
        }
    }
    if p0 == 0 || p0 >= 256 {
        return None;
    }
    let target = max * (1.0 - TONE_TAIL_DROP);
    let mut knee = p0 - 1;
    while knee > 0 {
        let a = tone[knee];
        let chord = a + (p0 - knee) as f32 * (max - a) / (255 - knee) as f32;
        if chord <= target {
            break;
        }
        knee -= 1;
    }
    let a = tone[knee];
    let slope = (max - a) / (255 - knee) as f32;
    let mut out = *tone;
    for (p, slot) in out.iter_mut().enumerate().skip(knee) {
        *slot = (a + (p - knee) as f32 * slope).min(tone[p]);
    }
    Some(out)
}

/// Apply the camera tone curve to a developed 16-bit sRGB frame. Alpha is
/// untouched.
pub fn apply_tone_wide(buf: &mut [u16], table: &[u16]) {
    debug_assert_eq!(table.len(), 65536, "tone: table size mismatch");
    for px in buf.chunks_exact_mut(4) {
        px[0] = table[px[0] as usize];
        px[1] = table[px[1] as usize];
        px[2] = table[px[2] as usize];
    }
}

/// Apply the develop stage to an 8-bit sRGB buffer. Legacy entry point (the
/// wand sampler and the ops bench); the render pipeline widens first, develops
/// wide, and dithers once at the end of the colour stage.
pub fn apply(buf: &mut [u8], w: usize, h: usize, adj: &RawAdjust) {
    if adj.is_identity() || w == 0 || h == 0 {
        return;
    }
    let mut wide_buf = wide::widen(buf);
    develop_wide(&mut wide_buf, w, h, adj);
    wide::narrow_plain(&wide_buf, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(r: u8, g: u8, b: u8) -> Vec<u8> {
        vec![r, g, b, 255]
    }

    fn run(p: Vec<u8>, adj: RawAdjust) -> Vec<u8> {
        let mut v = p;
        apply(&mut v, 1, 1, &adj);
        v
    }

    #[test]
    fn identity_is_a_noop() {
        assert_eq!(run(px(128, 90, 40), RawAdjust::default()), px(128, 90, 40));
    }

    #[test]
    fn neutral_temperature_is_a_noop() {
        let a = RawAdjust { temp: NEUTRAL_K, ..Default::default() };
        assert_eq!(run(px(128, 90, 40), a), px(128, 90, 40));
    }

    #[test]
    fn higher_temperature_warms_the_image() {
        let a = RawAdjust { temp: 8000.0, ..Default::default() };
        let out = run(px(128, 128, 128), a);
        assert!(out[0] > out[2] + 10, "expected warm cast, got {out:?}");
    }

    #[test]
    fn lower_temperature_cools_the_image() {
        let a = RawAdjust { temp: 3200.0, ..Default::default() };
        let out = run(px(128, 128, 128), a);
        assert!(out[2] > out[0] + 10, "expected cool cast, got {out:?}");
    }

    #[test]
    fn positive_tint_adds_magenta() {
        let a = RawAdjust { tint: 80.0, ..Default::default() };
        let out = run(px(128, 128, 128), a);
        assert!(out[1] + 5 < out[0], "green should drop, got {out:?}");
        assert!(out[1] + 5 < out[2], "green should drop, got {out:?}");
    }

    #[test]
    fn exposure_is_a_linear_gain() {
        let base = srgb_to_linear(128.0 / 255.0);
        let want = linear_to_srgb((base * 2.0).min(1.0)) * 255.0;
        let a = RawAdjust { exposure: 1.0, ..Default::default() };
        let out = run(px(128, 128, 128), a);
        assert!((out[0] as f32 - want).abs() < 3.0, "got {}, want {}", out[0], want);
        let down = run(px(128, 128, 128), RawAdjust { exposure: -1.0, ..Default::default() });
        let down_want = linear_to_srgb(base * 0.5) * 255.0;
        assert!((down[0] as f32 - down_want).abs() < 3.0);
    }

    #[test]
    fn highlight_recovery_pulls_bright_values_down() {
        let a = RawAdjust { highlights: 100.0, ..Default::default() };
        let out = run(px(250, 250, 250), a);
        assert!(out[0] < 245 && out[0] > 200, "got {out:?}");
    }

    #[test]
    fn highlight_recovery_leaves_midtones_alone() {
        let out = run(px(140, 140, 140), RawAdjust { highlights: 100.0, ..Default::default() });
        assert_eq!(out[0], 140);
    }

    #[test]
    fn shadow_lift_brightens_dark_values() {
        let out = run(px(8, 8, 8), RawAdjust { shadows: 100.0, ..Default::default() });
        assert!(out[0] > 20, "got {out:?}");
        let mid = run(px(150, 150, 150), RawAdjust { shadows: 100.0, ..Default::default() });
        assert_eq!(mid[0], 150);
    }

    #[test]
    fn shadow_noise_suppresses_chroma_without_dimming() {
        let a = RawAdjust { noise: 100.0, ..Default::default() };
        let out = run(px(12, 40, 8), a);
        let before = 40i32 - 12i32;
        let after = out[1] as i32 - out[0] as i32;
        assert!(after.abs() < before.abs(), "chroma not reduced: {out:?}");
        assert!(out[0] as i32 + out[1] as i32 + out[2] as i32 > 30);
    }

    #[test]
    fn sharpening_adds_edge_contrast() {
        // 5x1: dark half, bright half.
        let mut v = Vec::new();
        for x in 0..5 {
            let val = if x < 2 { 96 } else { 160 };
            v.extend_from_slice(&[val, val, val, 255]);
        }
        let before = v.clone();
        apply(&mut v, 5, 1, &RawAdjust { sharpen: 100.0, ..Default::default() });
        // Pixels adjacent to the edge on each side must separate further.
        assert!(v[1 * 4] < before[1 * 4], "dark side {v:?}");
        assert!(v[2 * 4] > before[2 * 4], "bright side {v:?}");
        assert_eq!(v[0], before[0], "far pixels are not touched");
    }

    #[test]
    fn develop_wide_keeps_sub_8_bit_input() {
        // Two source levels that narrow to the same u8 code must survive an
        // exposure lift in the 16-bit path.
        let mut buf = vec![0u16, 0, 0, 65535, 128, 128, 128, 65535];
        develop_wide(&mut buf, 2, 1, &RawAdjust { exposure: 2.0, ..Default::default() });
        assert_ne!(buf[0], buf[4]);
        assert!(buf[4] > buf[0]);
    }

    fn clipped_curve() -> [u8; 256] {
        let mut t = [0u8; 256];
        for (i, v) in t.iter_mut().enumerate() {
            *v = ((i as f32 * 1.2).min(240.0)) as u8;
        }
        t
    }

    fn at(table: &[u16], code: usize) -> u16 {
        table[code * 257]
    }

    #[test]
    fn tone_table_for_is_identity_at_zero() {
        let tone = clipped_curve();
        assert_eq!(tone_table_for(&tone, 0.0), tone_table(&tone));
    }

    #[test]
    fn tone_table_for_keeps_an_unclipped_curve() {
        let mut tone = [0u8; 256];
        for (i, v) in tone.iter_mut().enumerate() {
            *v = i as u8;
        }
        assert_eq!(tone_table_for(&tone, 100.0), tone_table(&tone));
    }

    #[test]
    fn tone_table_for_unclips_the_flat_tail() {
        let tone = clipped_curve();
        let flat = tone_table(&tone);
        let hl = tone_table_for(&tone, 100.0);
        for p in 0..256 {
            assert!(at(&hl, p) <= at(&flat, p), "code {p} brightened");
        }
        assert_eq!(at(&hl, 255), at(&flat, 255), "white moved");
        assert_eq!(at(&hl, 20), at(&flat, 20), "below the knee moved");
        let drop = (at(&flat, 201) as i32 - at(&hl, 201) as i32) / 257;
        assert!((25..=35).contains(&drop), "clip-point drop {drop}");
        // The former plateau resolves distinct, increasing inputs.
        assert!(at(&hl, 210) > at(&hl, 201));
        assert!(at(&hl, 240) > at(&hl, 210));
        assert!(at(&hl, 255) > at(&hl, 240));
        for pair in hl.windows(2) {
            assert!(pair[0] <= pair[1], "not monotone: {pair:?}");
        }
    }

    #[test]
    fn tone_table_for_scales_with_highlights() {
        let tone = clipped_curve();
        let flat = tone_table(&tone);
        let full = tone_table_for(&tone, 100.0);
        let half = tone_table_for(&tone, 50.0);
        assert!(at(&full, 201) < at(&half, 201));
        assert!(at(&half, 201) < at(&flat, 201));
    }
}
