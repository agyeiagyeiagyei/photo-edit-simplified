//! Edge-aware denoiser: separable bilateral filter over a YCbCr split.
//!
//! Chroma first: the lower half of the slider mostly removes colour noise,
//! the blotchy part of high-ISO noise. Luma is mixed in on a quadratic ramp
//! that reaches the full filter at strength 0.5, because fine luminance
//! grain is often real texture (feathers, foliage). Above 0.5 the behaviour
//! is unchanged.
//!
//! Chroma gets a wide window and its own range weight plus a luma-guided one,
//! so colour blotches smooth out while real colour and luminance edges hold.
//! Chroma is filtered at half resolution — the cheaper, standard split, and
//! chroma noise is low-frequency anyway — and bilinearly upsampled. Luma uses
//! a narrower window and a tighter range sigma so texture survives.
//!
//! Luma is the Rec.601 luma of the sRGB codes, the same value the YCbCr split
//! carries, so filtering it leaves tone alone: an unfiltered pixel is returned
//! exactly and a flat colour of any saturation does not move (#15).
//!
//! Window and range parameters are defined in sensor pixels and sensor-referred
//! noise, then scaled into the render: a proxy at 1/p the sensor's linear
//! resolution carries ~1/p the noise, so its sigmas shrink by p and its radius
//! is the sensor radius divided by p. Without that, the proxy would denoise
//! relatively harder than the export and the two would not match.

use std::sync::OnceLock;

const MAX_LUMA_R: usize = 6;
const MAX_CHROMA_R: usize = 8;

/// u16 luma (code x 257) back to an 8-bit code, rounded.
fn luma_lut() -> &'static [u8; 65536] {
    static LUT: OnceLock<[u8; 65536]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u8; 65536];
        for (v, out) in t.iter_mut().enumerate() {
            *out = ((v as u32 + 128) / 257).min(255) as u8;
        }
        t
    })
}


/// Normalised spatial Gaussian over the window offsets -r..=r.
fn spatial(r: usize) -> Vec<f32> {
    let sigma = (r as f32 / 2.0).max(0.7);
    let mut w: Vec<f32> = (-(r as i32)..=(r as i32))
        .map(|d| (-(d * d) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = w.iter().sum();
    for v in &mut w {
        *v /= sum;
    }
    w
}

/// Range weights indexed by an absolute sample difference of 0..=255.
fn range(sigma: f32) -> Vec<f32> {
    (0..=255)
        .map(|d| {
            let d = d as f32;
            (-(d * d) / (2.0 * sigma * sigma)).exp()
        })
        .collect()
}

fn luma_horizontal(
    y: &[u8],
    w: usize,
    h: usize,
    r: usize,
    sw: &[f32],
    lut: &[f32],
) -> Vec<u16> {
    let mut out = vec![0u16; w * h];
    for yy in 0..h {
        let row = &y[yy * w..(yy + 1) * w];
        let orow = &mut out[yy * w..(yy + 1) * w];
        for (x, o) in orow.iter_mut().enumerate() {
            let c = row[x] as i32;
            let x0 = x.saturating_sub(r);
            let x1 = (x + r).min(w - 1);
            // Same taps in the same order as the full window; at the edges the
            // window is clipped and the spatial weights offset to match.
            let win = &row[x0..=x1];
            let swin = &sw[x0 + r - x..=x1 + r - x];
            let mut acc = 0.0f32;
            let mut wsum = 0.0f32;
            for (&g, &s) in win.iter().zip(swin) {
                let wt = s * lut[(g as i32 - c).unsigned_abs() as usize];
                // The 16-bit luma (code x 257), computed inline: exact in f32.
                acc += wt * (g as f32 * 257.0);
                wsum += wt;
            }
            *o = (acc / wsum).round().clamp(0.0, 65535.0) as u16;
        }
    }
    out
}

fn luma_vertical(
    tmp: &[u16],
    y: &[u8],
    w: usize,
    h: usize,
    r: usize,
    sw: &[f32],
    lut: &[f32],
    srgb: &[u8; 65536],
) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    let mut acc = vec![0.0f32; w];
    let mut wsum = vec![0.0f32; w];
    for yy in 0..h {
        let y0 = yy.saturating_sub(r);
        let y1 = (yy + r).min(h - 1);
        let centre = &y[yy * w..(yy + 1) * w];
        acc.iter_mut().for_each(|v| *v = 0.0);
        wsum.iter_mut().for_each(|v| *v = 0.0);
        // Row-major: each tap row is read sequentially, and every pixel still
        // accumulates its taps in ascending row order, so the sums are
        // bit-identical to the column walk.
        for yyy in y0..=y1 {
            let s = sw[yyy + r - yy];
            let grow = &y[yyy * w..(yyy + 1) * w];
            let trow = &tmp[yyy * w..(yyy + 1) * w];
            for x in 0..w {
                let wt = s * lut[(grow[x] as i32 - centre[x] as i32).unsigned_abs() as usize];
                acc[x] += wt * trow[x] as f32;
                wsum[x] += wt;
            }
        }
        for (x, o) in out[yy * w..(yy + 1) * w].iter_mut().enumerate() {
            let v = (acc[x] / wsum[x]).round().clamp(0.0, 65535.0) as usize;
            *o = srgb[v];
        }
    }
    out
}

/// Horizontal pass of the chroma bilateral, on Cb and Cr together: the
/// spatial and luma-guide weights are the same for both planes, so each tap
/// computes `spatial * guide` once and multiplies in each plane's own range
/// weight, in the same order as filtering the planes separately (bit-exact).
#[allow(clippy::too_many_arguments)]
fn chroma_horizontal2(
    cb: &[i16],
    cr: &[i16],
    guide: &[u8],
    w: usize,
    h: usize,
    r: usize,
    sw: &[f32],
    guide_lut: &[f32],
    chroma_lut: &[f32],
) -> (Vec<i16>, Vec<i16>) {
    let mut ob = vec![0i16; w * h];
    let mut or = vec![0i16; w * h];
    for yy in 0..h {
        let rb = &cb[yy * w..(yy + 1) * w];
        let rr = &cr[yy * w..(yy + 1) * w];
        let grow = &guide[yy * w..(yy + 1) * w];
        let (obr, orr) = (&mut ob[yy * w..(yy + 1) * w], &mut or[yy * w..(yy + 1) * w]);
        for x in 0..w {
            let g = grow[x] as i32;
            let (vb, vr) = (rb[x] as i32, rr[x] as i32);
            let x0 = x.saturating_sub(r);
            let x1 = (x + r).min(w - 1);
            let swin = &sw[x0 + r - x..=x1 + r - x];
            let (mut ab, mut wb, mut ar, mut wr) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for (k, &sp) in swin.iter().enumerate() {
                let xx = x0 + k;
                let gw = sp * guide_lut[(grow[xx] as i32 - g).unsigned_abs() as usize];
                let sb = rb[xx] as i32;
                let sr = rr[xx] as i32;
                let wtb = gw * chroma_lut[(sb - vb).unsigned_abs() as usize];
                let wtr = gw * chroma_lut[(sr - vr).unsigned_abs() as usize];
                ab += wtb * sb as f32;
                wb += wtb;
                ar += wtr * sr as f32;
                wr += wtr;
            }
            obr[x] = (ab / wb).round().clamp(-1024.0, 1023.0) as i16;
            orr[x] = (ar / wr).round().clamp(-1024.0, 1023.0) as i16;
        }
    }
    (ob, or)
}

/// Vertical pass of the paired chroma bilateral, row-major: every pixel
/// accumulates its taps in ascending row order (bit-exact).
#[allow(clippy::too_many_arguments)]
fn chroma_vertical2(
    tb: &[i16],
    tr: &[i16],
    guide: &[u8],
    w: usize,
    h: usize,
    r: usize,
    sw: &[f32],
    guide_lut: &[f32],
    chroma_lut: &[f32],
) -> (Vec<i16>, Vec<i16>) {
    let mut ob = vec![0i16; w * h];
    let mut or = vec![0i16; w * h];
    let (mut ab, mut wb) = (vec![0.0f32; w], vec![0.0f32; w]);
    let (mut ar, mut wr) = (vec![0.0f32; w], vec![0.0f32; w]);
    for y in 0..h {
        let y0 = y.saturating_sub(r);
        let y1 = (y + r).min(h - 1);
        let gc = &guide[y * w..(y + 1) * w];
        let (cb, cr) = (&tb[y * w..(y + 1) * w], &tr[y * w..(y + 1) * w]);
        for v in [&mut ab, &mut wb, &mut ar, &mut wr] {
            v.iter_mut().for_each(|x| *x = 0.0);
        }
        for yy in y0..=y1 {
            let sp = sw[yy + r - y];
            let gr = &guide[yy * w..(yy + 1) * w];
            let (rb, rr) = (&tb[yy * w..(yy + 1) * w], &tr[yy * w..(yy + 1) * w]);
            for x in 0..w {
                let gw = sp * guide_lut[(gr[x] as i32 - gc[x] as i32).unsigned_abs() as usize];
                let sb = rb[x] as i32;
                let sr = rr[x] as i32;
                let wtb = gw * chroma_lut[(sb - cb[x] as i32).unsigned_abs() as usize];
                let wtr = gw * chroma_lut[(sr - cr[x] as i32).unsigned_abs() as usize];
                ab[x] += wtb * sb as f32;
                wb[x] += wtb;
                ar[x] += wtr * sr as f32;
                wr[x] += wtr;
            }
        }
        for x in 0..w {
            ob[y * w + x] = (ab[x] / wb[x]).round().clamp(-1024.0, 1023.0) as i16;
            or[y * w + x] = (ar[x] / wr[x]).round().clamp(-1024.0, 1023.0) as i16;
        }
    }
    (ob, or)
}

/// 2x2 box downsample of an i16 plane.
fn half_down(src: &[i16], w: usize, h: usize) -> (Vec<i16>, usize, usize) {
    let hw = w.div_ceil(2);
    let hh = h.div_ceil(2);
    let mut out = vec![0i16; hw * hh];
    for y in 0..hh {
        let y0 = y * 2;
        let y1 = (y0 + 1).min(h - 1);
        for x in 0..hw {
            let x0 = x * 2;
            let x1 = (x0 + 1).min(w - 1);
            let s = src[y0 * w + x0] as i32
                + src[y0 * w + x1] as i32
                + src[y1 * w + x0] as i32
                + src[y1 * w + x1] as i32;
            // Round half up for both signs: `/` truncates toward zero, which
            // nudged every negative chroma (all greens) one code toward zero.
            out[y * hw + x] = ((s + 2) >> 2) as i16;
        }
    }
    (out, hw, hh)
}

/// Matching 2x2 box downsample for the u8 luma guide.
fn half_down_luma(src: &[u8], w: usize, h: usize, hw: usize, hh: usize) -> Vec<u8> {
    let mut out = vec![0u8; hw * hh];
    for y in 0..hh {
        let y0 = y * 2;
        let y1 = (y0 + 1).min(h - 1);
        for x in 0..hw {
            let x0 = x * 2;
            let x1 = (x0 + 1).min(w - 1);
            let s = src[y0 * w + x0] as u32
                + src[y0 * w + x1] as u32
                + src[y1 * w + x0] as u32
                + src[y1 * w + x1] as u32;
            out[y * hw + x] = ((s + 2) / 4) as u8;
        }
    }
    out
}

/// Denoise a 16-bit RGBA frame in place: narrow to 8 bits, filter, and add
/// the 8-bit change back in steps of 257, so the 16-bit precision of
/// unfiltered pixels is kept. The pre-filter 8-bit value is recomputed from
/// the untouched u16 rather than kept in a second copy.
pub fn smart_denoise_wide(buf: &mut [u16], w: usize, h: usize, strength: f32, px_scale: f32) {
    if strength <= 0.0 || w == 0 || h == 0 || buf.len() < w * h * 4 {
        return;
    }
    // Alpha is never filtered, so only the colour channels are narrowed and
    // widened back.
    let mut narrow = vec![255u8; w * h * 4];
    for (n, v) in narrow.chunks_exact_mut(4).zip(buf.chunks_exact(4)) {
        n[0] = to8(v[0]) as u8;
        n[1] = to8(v[1]) as u8;
        n[2] = to8(v[2]) as u8;
    }
    smart_denoise(&mut narrow, w, h, strength, px_scale);
    for (dst, n) in buf.chunks_exact_mut(4).zip(narrow.chunks_exact(4)) {
        for c in 0..3 {
            let step = (n[c] as i32 - to8(dst[c])) * 257;
            if step != 0 {
                dst[c] = (dst[c] as i32 + step).clamp(0, 65535) as u16;
            }
        }
    }
}

/// `(v + 128) / 257` without the division; exact for every u16 (checked
/// exhaustively, and by `to8_matches_division`).
#[inline]
fn to8(v: u16) -> i32 {
    ((v as u32 * 255 + 32895) >> 16) as i32
}

/// Taps for the 2x bilinear upsample of a half-resolution plane back to
/// `w` x `h`: per output column (x0, x1, wx) and per output row (y0, y1, wy).
struct UpTaps {
    cols: Vec<(usize, usize, f32)>,
    rows: Vec<(usize, usize, f32)>,
}

impl UpTaps {
    fn new(sw: usize, sh: usize, w: usize, h: usize) -> Self {
        let taps = |n: usize, sn: usize| -> Vec<(usize, usize, f32)> {
            let s = sn as f32 / n as f32;
            (0..n)
                .map(|i| {
                    let f = ((i as f32 + 0.5) * s - 0.5).clamp(0.0, (sn - 1) as f32);
                    let i0 = f.floor() as usize;
                    (i0, (i0 + 1).min(sn - 1), f - i0 as f32)
                })
                .collect()
        };
        UpTaps { cols: taps(w, sw), rows: taps(h, sh) }
    }
}

#[inline]
fn bilerp(a: i16, b: i16, c: i16, d: i16, wx: f32, wy: f32) -> i16 {
    let (a, b, c, d) = (a as f32, b as f32, c as f32, d as f32);
    let top = a + (b - a) * wx;
    let bot = c + (d - c) * wx;
    (top + (bot - top) * wy).round() as i16
}

/// Share of the luma filter applied at slider strength `t` (0..=1):
/// `(t / LUMA_FULL_AT)²`, capped at 1. 0.1 → 0.04, 0.25 → 0.25, ≥0.5 → 1.
const LUMA_FULL_AT: f32 = 0.5;
/// Luma mixes below this are skipped outright (slider strength < ~0.11).
const LUMA_SKIP_MIX: f32 = 0.05;
fn luma_mix(t: f32) -> f32 {
    let r = (t / LUMA_FULL_AT).clamp(0.0, 1.0);
    r * r
}

/// Denoise `buf` (RGBA, `w`×`h`) in place. `strength` 0..=1; 0 is a no-op.
/// `px_scale` is sensor pixels per buffer pixel — 1.0 for a full-res export,
/// larger for a downscaled proxy or preview.
pub fn smart_denoise(buf: &mut [u8], w: usize, h: usize, strength: f32, px_scale: f32) {
    let t = strength.clamp(0.0, 1.0);
    if t <= 0.0 || w == 0 || h == 0 || buf.len() < w * h * 4 {
        return;
    }
    let n = w * h;
    let mut y = vec![0u8; n];
    let mut cb = vec![0i16; n];
    let mut cr = vec![0i16; n];
    for i in 0..n {
        let p = i * 4;
        let r = buf[p] as i32;
        let g = buf[p + 1] as i32;
        let b = buf[p + 2] as i32;
        let luma = (77 * r + 150 * g + 29 * b) >> 8;
        y[i] = luma as u8;
        cb[i] = ((-43 * r - 85 * g + 128 * b) >> 8) as i16;
        cr[i] = ((128 * r - 107 * g - 21 * b) >> 8) as i16;
    }

    // Sensor-space long edge in kilopixels. Radii are chosen there, clamped
    // there, then divided back into render pixels, so a high-MP sensor's cap
    // does not shrink the preview's relative window.
    let p = px_scale.max(1.0);
    let sensor_kpx = w.max(h) as f32 / 1000.0 * p;
    let r_y = (((0.9 * t * sensor_kpx).round().clamp(1.0, MAX_LUMA_R as f32)) / p)
        .round()
        .max(1.0) as usize;
    // Chroma runs at half resolution, so its radius is in half-res pixels.
    let r_c = (((1.0 * t * sensor_kpx)
        .round()
        .clamp(1.0, MAX_CHROMA_R as f32))
        / p)
        .round()
        .max(1.0) as usize;
    // Range sigmas are in 8-bit codes of this render, and the noise they must
    // reject shrinks with the downscale factor.
    let sy = (4.0 + 14.0 * t) / p;
    let sg = (8.0 + 24.0 * t) / p;
    let sc = (10.0 + 20.0 * t) / p;

    let sw_y = spatial(r_y);
    let lut_y = range(sy);
    // Luma is filtered as the same Rec.601 luma of sRGB codes that the YCbCr
    // split below carries. 189d5e5 filtered linear Rec.709 luminance instead
    // and substituted it for this luma; the two agree only for greys, so every
    // coloured pixel moved by a fixed amount whenever denoise was on (#15),
    // and pure-chroma noise leaked into luma. The bilateral range gate keeps
    // the averaged samples within a few codes of each other, so the sRGB-
    // averaging bias is far below one code (measured: mean -0.6 codes at
    // denoise 50 on the owner's ARW before 189d5e5).
    // Below `LUMA_SKIP_MIX` the luma pass changes nothing measurable (Smart
    // denoise 10 on the owner's ARW: luma detail +0.05 % at 100 %), so the
    // low range pays for chroma only.
    let luma_mix = luma_mix(t);
    let filt_y: Option<Vec<u8>> = (luma_mix >= LUMA_SKIP_MIX).then(|| {
        let tmp_y = luma_horizontal(&y, w, h, r_y, &sw_y, &lut_y);
        luma_vertical(&tmp_y, &y, w, h, r_y, &sw_y, &lut_y, luma_lut())
    });

    let sw_c = spatial(r_c);
    let lut_g = range(sg);
    let lut_c = range(sc);
    let (cb_h, hw, hh) = half_down(&cb, w, h);
    let (cr_h, _, _) = half_down(&cr, w, h);
    let y_h = half_down_luma(&y, w, h, hw, hh);
    let (tmp_cb, tmp_cr) = chroma_horizontal2(&cb_h, &cr_h, &y_h, hw, hh, r_c, &sw_c, &lut_g, &lut_c);
    let (v_cb, v_cr) = chroma_vertical2(&tmp_cb, &tmp_cr, &y_h, hw, hh, r_c, &sw_c, &lut_g, &lut_c);

    // Radii and sigmas already carry the strength. Chroma uses the filtered
    // result whole. Luma is mixed by `luma_mix`, which deliberately squares
    // the slider over its lower half (see the module docs), so the low range
    // leaves luminance texture nearly untouched.
    // The bilinear upsample of the filtered chroma is folded into this pass
    // (same arithmetic as a separate upsample, so bit-identical) instead of
    // writing and re-reading two full-resolution planes.
    let up = UpTaps::new(hw, hh, w, h);
    for yy in 0..h {
        let (y0, y1, wy) = up.rows[yy];
        let (cb0, cb1) = (&v_cb[y0 * hw..(y0 + 1) * hw], &v_cb[y1 * hw..(y1 + 1) * hw]);
        let (cr0, cr1) = (&v_cr[y0 * hw..(y0 + 1) * hw], &v_cr[y1 * hw..(y1 + 1) * hw]);
        for (x, &(x0, x1, wx)) in up.cols.iter().enumerate() {
            let i = yy * w + x;
            // The integer forward split and this float inverse are not exact
            // inverses (up to 2 codes on saturated blues), so apply only the
            // change the filter made: an unfiltered pixel comes back exactly.
            let to_rgb = |yy: f32, cb: f32, cr: f32| {
                [yy + 1.402 * cr, yy - 0.344136 * cb - 0.714136 * cr, yy + 1.772 * cb]
            };
            let a = to_rgb(y[i] as f32, cb[i] as f32, cr[i] as f32);
            let fy = match &filt_y {
                Some(f) => y[i] as f32 + luma_mix * (f[i] as f32 - y[i] as f32),
                None => y[i] as f32,
            };
            let fcb = bilerp(cb0[x0], cb0[x1], cb1[x0], cb1[x1], wx, wy) as f32;
            let fcr = bilerp(cr0[x0], cr0[x1], cr1[x0], cr1[x1], wx, wy) as f32;
            let b = to_rgb(fy, fcb, fcr);
            let p = i * 4;
            for c in 0..3 {
                buf[p + c] = (buf[p + c] as f32 + (b[c] - a[c])).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colour_noisy(w: usize, h: usize, base: [u8; 3], amp: i32, seed: u32) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        let mut s = seed;
        for i in 0..w * h {
            for c in 0..3 {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                let d = if amp == 0 { 0 } else { ((s >> 16) as i32 % (2 * amp + 1)) - amp };
                v[i * 4 + c] = (base[c] as i32 + d).clamp(0, 255) as u8;
            }
            v[i * 4 + 3] = 255;
        }
        v
    }

    fn channel_means(v: &[u8]) -> [f64; 3] {
        let n = (v.len() / 4) as f64;
        let mut m = [0f64; 3];
        for p in v.chunks_exact(4) {
            for c in 0..3 {
                m[c] += p[c] as f64;
            }
        }
        m.map(|x| x / n)
    }

    #[test]
    fn flat_saturated_colour_is_unchanged() {
        // #15: a flat foliage green came back ~+9 codes brighter at any
        // strength because filtered luminance replaced Rec.601 luma.
        for base in [[60u8, 140, 40], [200, 60, 50], [40, 70, 180], [128, 128, 128]] {
            for strength in [0.1f32, 0.5, 1.0] {
                let src = colour_noisy(48, 32, base, 0, 1);
                let mut out = src.clone();
                smart_denoise(&mut out, 48, 32, strength, 1.0);
                let max = out.iter().zip(&src).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
                assert!(max <= 1, "flat {base:?} at {strength} moved by {max}");
            }
        }
    }

    #[test]
    fn noisy_saturated_colour_keeps_its_mean() {
        for base in [[60u8, 140, 40], [200, 60, 50], [40, 70, 180]] {
            for strength in [0.1f32, 0.5] {
                let src = colour_noisy(96, 64, base, 8, 7);
                let mut out = src.clone();
                smart_denoise(&mut out, 96, 64, strength, 1.0);
                let (a, b) = (channel_means(&src), channel_means(&out));
                for c in 0..3 {
                    assert!(
                        (a[c] - b[c]).abs() <= 1.0,
                        "{base:?} at {strength}: channel {c} mean {:.2} -> {:.2}",
                        a[c],
                        b[c]
                    );
                }
            }
        }
    }

    /// Standard deviation of Rec.601 luma and of the two chroma planes.
    fn ycc_std(v: &[u8]) -> (f32, f32) {
        let n = (v.len() / 4) as f32;
        let mut sums = [0f32; 3];
        let mut sq = [0f32; 3];
        for p in v.chunks_exact(4) {
            let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
            let c = [
                0.299 * r + 0.587 * g + 0.114 * b,
                -0.168736 * r - 0.331264 * g + 0.5 * b,
                0.5 * r - 0.418688 * g - 0.081312 * b,
            ];
            for k in 0..3 {
                sums[k] += c[k];
                sq[k] += c[k] * c[k];
            }
        }
        let sd = |k: usize| (sq[k] / n - (sums[k] / n).powi(2)).max(0.0).sqrt();
        (sd(0), (sd(1) + sd(2)) / 2.0)
    }

    #[test]
    fn low_strength_is_chroma_first() {
        // Independent per-channel noise carries both luma and chroma noise.
        let src = colour_noisy(128, 96, [120, 110, 100], 10, 5);
        let (y0, c0) = ycc_std(&src);
        let mut low = src.clone();
        smart_denoise(&mut low, 128, 96, 0.1, 1.0);
        let (y1, c1) = ycc_std(&low);
        assert!(c1 < c0 * 0.6, "chroma noise {c0:.2} -> {c1:.2} at 0.1 (want -40% or more)");
        assert!(y1 > y0 * 0.95, "luma noise {y0:.2} -> {y1:.2} at 0.1 (want -5% at most)");
        let mut full = src.clone();
        smart_denoise(&mut full, 128, 96, 1.0, 1.0);
        let (y2, _) = ycc_std(&full);
        assert!(y2 < y0 * 0.8, "luma noise {y0:.2} -> {y2:.2} at 1.0 (want real smoothing)");
    }

    #[test]
    fn luma_ramp_reaches_full_at_half() {
        assert_eq!(luma_mix(0.0), 0.0);
        assert!((luma_mix(0.1) - 0.04).abs() < 1e-6);
        assert!((luma_mix(0.25) - 0.25).abs() < 1e-6);
        assert_eq!(luma_mix(0.5), 1.0);
        assert_eq!(luma_mix(1.0), 1.0);
    }

    fn gray_noisy(w: usize, h: usize, base: u8, amp: i32, seed: u32) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        let mut s = seed;
        for i in 0..w * h {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let d = ((s >> 16) as i32 % (2 * amp + 1)) - amp;
            let val = (base as i32 + d).clamp(0, 255) as u8;
            let p = i * 4;
            v[p] = val;
            v[p + 1] = val;
            v[p + 2] = val;
            v[p + 3] = 255;
        }
        v
    }

    fn luma_std(v: &[u8], w: usize, h: usize) -> f32 {
        let vals: Vec<f32> = (0..w * h)
            .map(|i| {
                let p = i * 4;
                (77.0 * v[p] as f32 + 150.0 * v[p + 1] as f32 + 29.0 * v[p + 2] as f32) / 256.0
            })
            .collect();
        let m = vals.iter().sum::<f32>() / vals.len() as f32;
        (vals.iter().map(|v| (v - m) * (v - m)).sum::<f32>() / vals.len() as f32).sqrt()
    }

    #[test]
    fn to8_matches_division() {
        for v in 0..=u16::MAX {
            assert_eq!(to8(v), ((v as u32 + 128) / 257) as i32, "v = {v}");
        }
    }

    #[test]
    fn wide_matches_narrow_denoise_widen() {
        // smart_denoise_wide must equal the old path: narrow to 8 bits,
        // denoise, add the 8-bit change back in steps of 257.
        let (w, h) = (96, 64);
        let src8 = colour_noisy(w, h, [90, 140, 60], 9, 11);
        let wide: Vec<u16> = src8
            .iter()
            .enumerate()
            .map(|(i, &v)| (v as u16 * 257).saturating_add(i as u16 % 200))
            .collect();
        for strength in [0.05f32, 0.3, 1.0] {
            let mut expect = wide.clone();
            let before: Vec<u8> = wide.iter().map(|&v| ((v as u32 + 128) / 257) as u8).collect();
            let mut after = before.clone();
            smart_denoise(&mut after, w, h, strength, 1.0);
            for ((d, &a), &b) in expect.iter_mut().zip(&after).zip(&before) {
                let step = (a as i32 - b as i32) * 257;
                if step != 0 {
                    *d = (*d as i32 + step).clamp(0, 65535) as u16;
                }
            }
            let mut got = wide.clone();
            smart_denoise_wide(&mut got, w, h, strength, 1.0);
            assert_eq!(got, expect, "strength {strength}");
        }
    }

    #[test]
    fn zero_strength_is_a_noop() {
        let orig = gray_noisy(64, 64, 120, 10, 7);
        let mut v = orig.clone();
        smart_denoise(&mut v, 64, 64, 0.0, 1.0);
        assert_eq!(v, orig);
    }

    #[test]
    fn flat_noise_is_smoothed() {
        let orig = gray_noisy(96, 96, 120, 12, 3);
        let mut v = orig.clone();
        // 96px image → tiny radii, so use a strong local check instead of
        // absolute numbers: neighbour differences must shrink a lot.
        smart_denoise(&mut v, 96, 96, 1.0, 1.0);
        let noise_before = luma_std(&orig, 96, 96);
        let noise_after = luma_std(&v, 96, 96);
        assert!(
            noise_after < noise_before * 0.7,
            "before {noise_before}, after {noise_after}"
        );
    }

    #[test]
    fn edges_are_preserved() {
        let w = 80;
        let h = 40;
        let mut v = gray_noisy(w, h, 30, 4, 11);
        // Right half becomes bright (edge at x=40).
        for yy in 0..h {
            for x in w / 2..w {
                let p = (yy * w + x) * 4;
                v[p] = 210;
                v[p + 1] = 210;
                v[p + 2] = 210;
            }
        }
        let mut d = v.clone();
        smart_denoise(&mut d, w, h, 1.0, 1.0);
        let dark = |v: &[u8], x: usize| v[(20 * w + x) * 4] as i32;
        let bright = |v: &[u8], x: usize| v[(20 * w + x) * 4] as i32;
        // Just inside the edge, the step must survive.
        let step_before = bright(&v, 41) - dark(&v, 38);
        let step_after = bright(&d, 41) - dark(&d, 38);
        assert!(
            step_after as f32 > step_before as f32 * 0.8,
            "before {step_before}, after {step_after}"
        );
        // Far from the edge the noise should be reduced.
        let flat_before = luma_std(&v[..], 80, 40);
        let flat_after = luma_std(&d[..], 80, 40);
        assert!(flat_after < flat_before);
    }

    #[test]
    fn chroma_blotches_are_suppressed() {
        // Two colours with the same luma (128) but opposite Cr: pure chroma
        // noise, no luminance signal for the filter to latch onto.
        let (w, h) = (64, 64);
        let mut v = vec![0u8; w * h * 4];
        for i in 0..w * h {
            let p = i * 4;
            let (r, g, b) = if i % 2 == 0 {
                (170u8, 107u8, 128u8)
            } else {
                (86u8, 149u8, 128u8)
            };
            v[p] = r;
            v[p + 1] = g;
            v[p + 2] = b;
            v[p + 3] = 255;
        }
        let luma_before = luma_std(&v, w, h);
        let mut d = v.clone();
        smart_denoise(&mut d, w, h, 1.0, 1.0);
        let spread = |v: &[u8]| -> f32 {
            let vals: Vec<f32> = (0..w * h)
                .map(|i| v[i * 4] as f32 - v[i * 4 + 2] as f32)
                .collect();
            let m = vals.iter().sum::<f32>() / vals.len() as f32;
            (vals.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / vals.len() as f32).sqrt()
        };
        assert!(spread(&d) < spread(&v) * 0.85, "chroma not smoothed");
        assert!(luma_std(&d, w, h) < luma_before + 1.5, "luma disturbed");
    }
}
