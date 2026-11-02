//! 16-bit working precision between the develop and colour stages, and the
//! single 8-bit quantisation at the end.
//!
//! Develop runs in linear f32 and used to write u8 sRGB, which colour then
//! re-quantised again. A strong exposure or shadow lift therefore produced
//! visible plateaus in skies. The pipeline now carries a u16 sRGB buffer
//! through colour and narrows once, with an ordered dither, so a slow gradient
//! becomes texture instead of bands. Alpha is an edge mask, not a tone, and is
//! narrowed exactly.

/// 8x8 Bayer matrix; each row is a low-discrepancy set of thresholds, which
/// keeps the dither pattern local while breaking up 8-bit plateaus.
const BAYER8: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// Widen u8 sRGB to the 16-bit working lattice. Exact: v*257 narrows back to v
/// under any dither threshold below 257.
pub fn widen(rgba: &[u8]) -> Vec<u16> {
    rgba.iter().map(|&v| v as u16 * 257).collect()
}

/// Expand a 256-entry (0..=255 domain) curve into a 16-bit input table by
/// linear interpolation between the 8-bit knots.
pub fn widen_table(t: &[f32; 256]) -> Vec<u16> {
    let mut out = vec![0u16; 65536];
    for (x, slot) in out.iter_mut().enumerate() {
        let f = x as f32 * (255.0 / 65535.0);
        let i = f as usize;
        let frac = f - i as f32;
        let lo = t[i].clamp(0.0, 255.0);
        let hi = t[(i + 1).min(255)].clamp(0.0, 255.0);
        *slot = ((lo + (hi - lo) * frac) * 257.0).round().clamp(0.0, 65535.0) as u16;
    }
    out
}

/// Round to the nearest 8-bit code, no dither. Used where the result is an
/// intermediate value (the legacy u8 entry points, tests), never for output.
pub fn narrow_plain(wide: &[u16], out: &mut [u8]) {
    for (o, &v) in out.iter_mut().zip(wide) {
        *o = ((v as u32 + 128) / 257) as u8;
    }
}

#[inline]
fn bayer_offset(x: usize, y: usize) -> u32 {
    (BAYER8[y & 7][x & 7] as u32 * 257 + 129) / 64
}

/// The final 16 -> 8-bit quantisation: ordered dither of +-0.5 LSB on RGB,
/// exact on alpha. `wide` and `out` are RGBA at `w`x`h`.
pub fn narrow_dithered(wide: &[u16], out: &mut [u8], w: usize, h: usize) {
    debug_assert_eq!(wide.len(), w * h * 4, "wide: buffer size mismatch");
    debug_assert_eq!(out.len(), wide.len(), "wide: buffer size mismatch");
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let off = bayer_offset(x, y);
            for c in 0..3 {
                out[i + c] = ((wide[i + c] as u32 + off) / 257).min(255) as u8;
            }
            out[i + 3] = ((wide[i + 3] as u32 + 128) / 257) as u8;
        }
    }
}

/// Blend two 16-bit channel values by mask/255 (0 = keep `a`, 255 = take `b`).
#[inline]
pub fn blend16(a: u16, b: u16, m: u32) -> u16 {
    ((a as u32 * (255 - m) + b as u32 * m + 127) / 255) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_exact() {
        for v in 0..=255u8 {
            let mut wide = vec![v as u16 * 257; 16 * 16 * 4];
            for px in wide.chunks_exact_mut(4) {
                px[3] = 255 * 257;
            }
            let mut out = vec![0u8; 16 * 16 * 4];
            narrow_dithered(&wide, &mut out, 16, 16);
            assert!(out.chunks_exact(4).all(|p| p[0] == v && p[3] == 255), "v={v}");
        }
    }

    #[test]
    fn dither_splits_a_half_step() {
        // 100.5 in 8-bit units: the 8x8 tile must contain both neighbours and
        // average to the 16-bit value.
        let v = 100u16 * 257 + 128;
        let (w, h) = (8, 8);
        let wide = vec![v; w * h * 4];
        let mut out = vec![0u8; w * h * 4];
        narrow_dithered(&wide, &mut out, w, h);
        let vals: Vec<u8> = out.chunks_exact(4).map(|p| p[0]).collect();
        assert!(vals.contains(&100) && vals.contains(&101), "values {vals:?}");
        let mean = vals.iter().map(|&x| x as f32).sum::<f32>() / vals.len() as f32;
        assert!((mean - 100.5).abs() <= 0.1, "mean {mean}");
    }

    #[test]
    fn dither_breaks_flat_bands() {
        // Constant plateau: without dither every pixel is the same code.
        let v = 70u16 * 257 + 128;
        let (w, h) = (64, 8);
        let wide = vec![v; w * h * 4];
        let mut out = vec![0u8; w * h * 4];
        narrow_dithered(&wide, &mut out, w, h);
        let row: Vec<u8> = out[..w * 4].chunks_exact(4).map(|p| p[0]).collect();
        let (mut longest, mut run) = (1usize, 1usize);
        for i in 1..row.len() {
            if row[i] == row[i - 1] {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 1;
            }
        }
        assert!(longest <= 4, "row still flat (run {longest}): {row:?}");
    }

    #[test]
    fn alpha_is_exact_under_dither() {
        for a in 0..=255u8 {
            let wide = vec![a as u16 * 257; 8 * 8 * 4];
            let mut out = vec![0u8; 8 * 8 * 4];
            narrow_dithered(&wide, &mut out, 8, 8);
            assert!(out.chunks_exact(4).all(|p| p[3] == a));
        }
    }

    #[test]
    fn widen_table_interpolates_knots() {
        // Identity ramp stays a ramp, and the 16-bit midpoint lands between
        // the two 8-bit codes.
        let mut t = [0.0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            *v = i as f32;
        }
        let w = widen_table(&t);
        assert_eq!(w[0], 0);
        assert_eq!(w[65535], 255 * 257);
        let mid = w[100 * 257 + 128];
        assert!(mid > 100 * 257 && mid < 101 * 257, "mid {mid}");
    }

    #[test]
    fn plain_narrow_rounds() {
        let wide = [0u16, 128, 129, 385, 386, 65535];
        let mut out = [0u8; 6];
        narrow_plain(&wide, &mut out);
        assert_eq!(out, [0, 0, 1, 1, 2, 255]);
    }
}
