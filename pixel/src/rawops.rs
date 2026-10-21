//! RAW develop adjustments: white balance, exposure, highlight recovery,
//! shadow lift / noise suppression, and capture sharpening.
//!
//! All of it runs in linear light — sRGB is decoded in, adjusted, re-encoded
//! out — and happens before the gamma-space colour controls. The base image
//! is the camera's rendering, so temperature is a relative correction: the
//! neutral point (5500 K) reproduces the camera's as-shot white balance, and
//! moving away from it applies a blackbody-derived von Kries gain.

use crate::types::{RawAdjust, NEUTRAL_K};

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

/// 3x3 box blur of the sRGB luma, used as the unsharp reference. One byte
/// per pixel so a 24MP develop pass adds 48MB, not another RGBA buffer.
fn blur_luma(buf: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut horiz = vec![0u8; w * h];
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
            horiz[row + x] = ((s >> 8) / (x1 - x0 + 1) as u32) as u8;
        }
    }
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        let y0 = y.saturating_sub(1);
        let y1 = (y + 1).min(h - 1);
        let rows = (y1 - y0 + 1) as u32;
        for x in 0..w {
            let mut s = 0u32;
            for yy in y0..=y1 {
                s += horiz[yy * w + x] as u32;
            }
            out[y * w + x] = (s / rows) as u8;
        }
    }
    out
}

/// Apply the develop stage in place. Identity leaves the buffer untouched.
pub fn apply(buf: &mut [u8], w: usize, h: usize, adj: &RawAdjust) {
    if adj.is_identity() || w == 0 || h == 0 {
        return;
    }
    let sharp = (adj.sharpen / 100.0).clamp(0.0, 1.0);
    let hl = (adj.highlights / 100.0).clamp(0.0, 1.0);
    let sh = (adj.shadows / 100.0).clamp(0.0, 1.0);
    let nr = (adj.noise / 100.0).clamp(0.0, 1.0);
    let gains = wb_gains(adj.temp, adj.tint);
    let exp_gain = 2f32.powf(adj.exposure.clamp(-5.0, 5.0));

    let mut dec = [0f32; 256];
    for (i, v) in dec.iter_mut().enumerate() {
        *v = srgb_to_linear(i as f32 / 255.0);
    }
    let mut enc = [0f32; 1025];
    for (i, v) in enc.iter_mut().enumerate() {
        *v = linear_to_srgb(i as f32 / 1024.0);
    }
    let encode = |v: f32| -> u8 {
        let x = (v.clamp(0.0, 1.0) * 1024.0) as usize;
        (enc[x] * 255.0).round() as u8
    };

    let blur = if sharp > 0.0 { Some(blur_luma(buf, w, h)) } else { None };

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
            let d = (yb as f32 - bp[i] as f32) / 255.0;
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

        // Shadow lift: soft toe that fades out by mid-grey.
        if sh > 0.0 {
            let w8 = (1.0 - (y / 0.18).min(1.0)).powi(2);
            let y2 = y + sh * 0.04 * w8;
            if y2 > y {
                scale_rgb(&mut rgb, y2 / y.max(1e-6));
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
}
