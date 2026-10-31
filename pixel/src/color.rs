//! Fused colour pipeline: levels, curves and the adjust stage share one pass.
//!
//! Levels and curves are per-channel LUTs, so they compose into a single
//! `[u8; 256]` table per channel. The old sequence ran one full-buffer loop
//! per stage, rounding to u8 between them; composing keeps the same u8
//! intermediate values, so the fused output is byte-identical (see the parity
//! tests). Adjust is folded into the same loop: when saturation and warmth are
//! zero it is per channel and could itself be a LUT, but running its math on
//! the LUT result costs less than a second pass over the buffer.

use crate::blur;
use crate::curves::CurvesSettings;
use crate::levels::LevelsSettings;
use crate::noise;
#[cfg(test)]
use crate::ops;
use crate::types::EditParams;

type Tables = [[u8; 256]; 3];

fn compose_tables(levels: &LevelsSettings, curves: &CurvesSettings) -> Tables {
    let lev = levels.build_tables();
    let cur = curves.build_tables();
    let mut out = [[0u8; 256]; 3];
    for (ch, table) in out.iter_mut().enumerate() {
        for (v, slot) in table.iter_mut().enumerate() {
            let mut x = v as u8;
            if !levels.is_identity() {
                x = lev[ch][v].round().clamp(0.0, 255.0) as u8;
            }
            if !curves.is_identity() {
                x = cur[ch][x as usize].round().clamp(0.0, 255.0) as u8;
            }
            *slot = x;
        }
    }
    out
}

/// Bake brightness/contrast into a table. With saturation and warmth at zero
/// the adjust stage is per channel, so it composes into the same u8 table the
/// levels/curves pass uses; a brightness drag then costs three lookups per
/// pixel instead of the float adjust math.
fn tables_with_adjust(t: &Tables, brightness: f32, contrast: f32) -> Tables {
    let mut out = [[0u8; 256]; 3];
    for (ch, table) in out.iter_mut().enumerate() {
        for (v, slot) in table.iter_mut().enumerate() {
            let x = t[ch][v] as f32;
            *slot = adjust_rgb(x, x, x, brightness, contrast, 0.0, 0.0)[ch] as u8;
        }
    }
    out
}

#[inline]
fn blend(orig: u8, new: u8, m: u32) -> u8 {
    ((orig as u32 * (255 - m) + new as u32 * m + 127) / 255) as u8
}

#[inline]
fn adjust_rgb(r: f32, g: f32, bl: f32, brightness: f32, contrast: f32, saturation: f32, warmth: f32) -> [f32; 3] {
    let b = brightness * 255.0;
    let c = contrast;
    let s = 1.0 + saturation;
    let wr = warmth * 40.0;
    let wb = -warmth * 40.0;
    let mut r = (r - 128.0) * (1.0 + c) + 128.0 + b;
    let mut g = (g - 128.0) * (1.0 + c) + 128.0 + b;
    let mut bl = (bl - 128.0) * (1.0 + c) + 128.0 + b;
    let luma = 0.299 * r + 0.587 * g + 0.114 * bl;
    r = luma + (r - luma) * s;
    g = luma + (g - luma) * s;
    bl = luma + (bl - luma) * s;
    r += wr;
    bl += wb;
    [r.clamp(0.0, 255.0), g.clamp(0.0, 255.0), bl.clamp(0.0, 255.0)]
}

#[inline]
fn adjust_px(px: &mut [u8], brightness: f32, contrast: f32, saturation: f32, warmth: f32) {
    let out = adjust_rgb(
        px[0] as f32,
        px[1] as f32,
        px[2] as f32,
        brightness,
        contrast,
        saturation,
        warmth,
    );
    px[0] = out[0] as u8;
    px[1] = out[1] as u8;
    px[2] = out[2] as u8;
}

/// Levels + curves + adjust over the whole buffer, one pass.
pub fn apply_color(data: &mut [u8], edit: &EditParams) {
    let levels_identity = edit.levels.is_identity();
    let curves_identity = edit.curves.is_identity();
    let luts_identity = levels_identity && curves_identity;
    let adjust_identity = edit.brightness == 0.0
        && edit.contrast == 0.0
        && edit.saturation == 0.0
        && edit.warmth == 0.0;
    if luts_identity && adjust_identity {
        return;
    }
    let fold = !adjust_identity && edit.saturation == 0.0 && edit.warmth == 0.0;
    let mut t = compose_tables(&edit.levels, &edit.curves);
    if fold {
        t = tables_with_adjust(&t, edit.brightness, edit.contrast);
    }
    for px in data.chunks_exact_mut(4) {
        if px[3] == 0 {
            // The split sequence skips the LUTs but still runs adjust here.
            if !adjust_identity {
                adjust_px(px, edit.brightness, edit.contrast, edit.saturation, edit.warmth);
            }
            continue;
        }
        if fold || !luts_identity {
            for ch in 0..3 {
                px[ch] = t[ch][px[ch] as usize];
            }
        }
        if !fold && !adjust_identity {
            adjust_px(px, edit.brightness, edit.contrast, edit.saturation, edit.warmth);
        }
    }
}

/// Masked variant: each stage is blended through the mask in order, exactly
/// as the separate `*_masked` passes did.
pub fn apply_color_masked(data: &mut [u8], mask: &[u8], edit: &EditParams) {
    let lev_t = edit.levels.build_tables();
    let cur_t = edit.curves.build_tables();
    let levels_identity = edit.levels.is_identity();
    let curves_identity = edit.curves.is_identity();
    let adjust_identity = edit.brightness == 0.0
        && edit.contrast == 0.0
        && edit.saturation == 0.0
        && edit.warmth == 0.0;
    if levels_identity && curves_identity && adjust_identity {
        return;
    }
    for (px, m) in data.chunks_exact_mut(4).zip(mask.iter()) {
        let m = *m as u32;
        if m != 0 {
            if px[3] != 0 {
                if !levels_identity {
                    for ch in 0..3 {
                        let v = lev_t[ch][px[ch] as usize].round().clamp(0.0, 255.0) as u8;
                        px[ch] = blend(px[ch], v, m);
                    }
                }
                if !curves_identity {
                    for ch in 0..3 {
                        let v = cur_t[ch][px[ch] as usize].round().clamp(0.0, 255.0) as u8;
                        px[ch] = blend(px[ch], v, m);
                    }
                }
            }
            if !adjust_identity {
                let orig = [px[0] as f32, px[1] as f32, px[2] as f32];
                let adj = adjust_rgb(
                    orig[0],
                    orig[1],
                    orig[2],
                    edit.brightness,
                    edit.contrast,
                    edit.saturation,
                    edit.warmth,
                );
                let a = m as f32 / 255.0;
                for ch in 0..3 {
                    px[ch] = (orig[ch] + (adj[ch] - orig[ch]) * a) as u8;
                }
            }
        }
    }
}

/// The full colour stage used by previews and exports: blur, fused
/// levels/curves/adjust, then grain.
pub fn apply_color_effects(data: &mut [u8], w: usize, h: usize, edit: &EditParams, seed: u32) {
    blur::blur_apply(data, w, h, edit.blur);
    apply_color(data, edit);
    noise::noise_add(
        data,
        w,
        h,
        edit.grain,
        edit.grain_gaussian,
        edit.grain_mono,
        seed,
    );
}

/// Masked variant of [`apply_color_effects`].
pub fn apply_color_effects_masked(
    data: &mut [u8],
    mask: &[u8],
    w: usize,
    h: usize,
    edit: &EditParams,
    seed: u32,
) {
    blur::blur_apply_masked(data, mask, w, h, edit.blur);
    apply_color_masked(data, mask, edit);
    noise::noise_add_masked(
        data,
        mask,
        w,
        h,
        edit.grain,
        edit.grain_gaussian,
        edit.grain_mono,
        seed,
    );
}

/// Reference sequence (pre-fusion) used by the parity tests.
#[cfg(test)]
fn apply_color_split(data: &mut [u8], edit: &EditParams) {
    if !edit.levels.is_identity() {
        crate::levels::levels_apply(data, &edit.levels.build_tables());
    }
    edit.curves.apply(data);
    ops::adjust(
        data,
        edit.brightness,
        edit.contrast,
        edit.saturation,
        edit.warmth,
    );
}

#[cfg(test)]
fn apply_color_split_masked(data: &mut [u8], mask: &[u8], edit: &EditParams) {
    if !edit.levels.is_identity() {
        crate::levels::levels_apply_masked(data, mask, &edit.levels.build_tables());
    }
    edit.curves.apply_masked(data, mask);
    ops::adjust_masked(
        data,
        mask,
        edit.brightness,
        edit.contrast,
        edit.saturation,
        edit.warmth,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curves::CurvePoint;
    use crate::levels::{LevelRange, LevelsSettings};
    use crate::types::EditParams;

    fn buffer(n: usize) -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                if i % 4 == 3 {
                    255
                } else {
                    (x & 0xff) as u8
                }
            })
            .collect()
    }

    fn edit(levels: bool, curves: bool, b: f32, c: f32, s: f32, w: f32) -> EditParams {
        let mut e = EditParams::default();
        if levels {
            e.levels = LevelsSettings {
                ranges: [
                    LevelRange { black: 8.0, gamma: 1.2, white: 244.0, output_black: 3.0, output_white: 252.0 },
                    LevelRange { black: 0.0, gamma: 0.9, white: 255.0, output_black: 0.0, output_white: 255.0 },
                    LevelRange { black: 4.0, gamma: 1.05, white: 250.0, output_black: 0.0, output_white: 255.0 },
                    LevelRange { black: 2.0, gamma: 1.1, white: 253.0, output_black: 1.0, output_white: 254.0 },
                ],
            };
        }
        if curves {
            let mut cs = crate::curves::CurvesSettings::default();
            for ch in 0..4 {
                cs.channels[ch] = vec![
                    CurvePoint { x: 0.0, y: 0.0 },
                    CurvePoint { x: 96.0, y: 72.0 },
                    CurvePoint { x: 192.0, y: 210.0 },
                    CurvePoint { x: 255.0, y: 255.0 },
                ];
            }
            e.curves = cs;
        }
        e.brightness = b;
        e.contrast = c;
        e.saturation = s;
        e.warmth = w;
        e
    }

    fn cases() -> Vec<EditParams> {
        vec![
            edit(false, false, 0.0, 0.0, 0.0, 0.0),
            edit(false, false, 0.1, 0.2, 0.0, 0.0),
            edit(false, false, 0.0, 0.0, 0.3, 0.0),
            edit(false, false, 0.05, -0.15, -0.25, 0.2),
            edit(true, false, 0.0, 0.0, 0.0, 0.0),
            edit(false, true, 0.0, 0.0, 0.0, 0.0),
            edit(true, true, 0.0, 0.0, 0.0, 0.0),
            edit(false, false, 0.12, -0.08, 0.0, 0.0),
            edit(true, true, 0.12, -0.08, 0.0, 0.0),
            edit(true, true, 0.12, -0.08, 0.35, -0.15),
        ]
    }

    #[test]
    fn fused_matches_split_sequence() {
        for e in cases() {
            let src = buffer(64 * 3 * 4);
            let mut a = src.clone();
            let mut b = src.clone();
            apply_color(&mut a, &e);
            apply_color_split(&mut b, &e);
            assert_eq!(a, b, "unmasked mismatch for {e:?}");
        }
    }

    #[test]
    fn fused_masked_matches_split_sequence() {
        let mut x: u32 = 0x9e37_79b9;
        let mask: Vec<u8> = (0..64 * 3)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                if i % 7 == 0 { 0 } else { (x & 0xff) as u8 }
            })
            .collect();
        for e in cases() {
            let src = buffer(64 * 3 * 4);
            let mut a = src.clone();
            let mut b = src.clone();
            apply_color_masked(&mut a, &mask, &e);
            apply_color_split_masked(&mut b, &mask, &e);
            assert_eq!(a, b, "masked mismatch for {e:?}");
        }
    }

    #[test]
    fn transparent_pixels_skip_luts_but_take_adjust() {
        let src = vec![10u8, 20, 30, 0, 40, 50, 60, 255];
        for e in [
            edit(false, false, 0.2, 0.0, 0.0, 0.0),
            edit(true, true, 0.2, 0.1, 0.0, 0.0),
            edit(true, true, 0.0, 0.0, 0.3, 0.0),
        ] {
            let mut a = src.clone();
            let mut b = src.clone();
            apply_color(&mut a, &e);
            apply_color_split(&mut b, &e);
            assert_eq!(a, b, "transparent mismatch for {e:?}");
        }
    }
}
