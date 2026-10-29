//! Edge-aware denoiser: separable bilateral filter over a YCbCr split.
//!
//! Chroma gets a wide window and its own range weight plus a luma-guided one,
//! so colour blotches smooth out while real colour and luminance edges hold.
//! Chroma is filtered at half resolution — the cheaper, standard split, and
//! chroma noise is low-frequency anyway — and bilinearly upsampled. Luma uses
//! a narrower window and a tighter range sigma so texture survives. Radii
//! scale with the image's long edge, so the 3200px preview proxy and a
//! full-res export smooth at the same relative scale.

const MAX_LUMA_R: usize = 6;
const MAX_CHROMA_R: usize = 8;

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

fn luma_horizontal(y: &[u8], w: usize, h: usize, r: usize, sw: &[f32], lut: &[f32]) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    for yy in 0..h {
        let row = yy * w;
        for x in 0..w {
            let c = y[row + x] as i32;
            let x0 = x.saturating_sub(r);
            let x1 = (x + r).min(w - 1);
            let mut acc = 0.0f32;
            let mut wsum = 0.0f32;
            for xx in x0..=x1 {
                let s = y[row + xx] as i32;
                let wt = sw[xx + r - x] * lut[(s - c).unsigned_abs() as usize];
                acc += wt * s as f32;
                wsum += wt;
            }
            out[row + x] = (acc / wsum).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn luma_vertical(tmp: &[u8], w: usize, h: usize, r: usize, sw: &[f32], lut: &[f32]) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        let y0 = y.saturating_sub(r);
        let y1 = (y + r).min(h - 1);
        for x in 0..w {
            let c = tmp[y * w + x] as i32;
            let mut acc = 0.0f32;
            let mut wsum = 0.0f32;
            for yy in y0..=y1 {
                let s = tmp[yy * w + x] as i32;
                let wt = sw[yy + r - y] * lut[(s - c).unsigned_abs() as usize];
                acc += wt * s as f32;
                wsum += wt;
            }
            out[y * w + x] = (acc / wsum).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn chroma_horizontal(
    c: &[i16],
    guide: &[u8],
    w: usize,
    h: usize,
    r: usize,
    sw: &[f32],
    guide_lut: &[f32],
    chroma_lut: &[f32],
) -> Vec<i16> {
    let mut out = vec![0i16; w * h];
    for yy in 0..h {
        let row = yy * w;
        for x in 0..w {
            let g = guide[row + x] as i32;
            let cv = c[row + x] as i32;
            let x0 = x.saturating_sub(r);
            let x1 = (x + r).min(w - 1);
            let mut acc = 0.0f32;
            let mut wsum = 0.0f32;
            for xx in x0..=x1 {
                let s = c[row + xx] as i32;
                let gd = guide[row + xx] as i32;
                let wt = sw[xx + r - x]
                    * guide_lut[(gd - g).unsigned_abs() as usize]
                    * chroma_lut[(s - cv).unsigned_abs() as usize];
                acc += wt * s as f32;
                wsum += wt;
            }
            out[row + x] = (acc / wsum).round().clamp(-1024.0, 1023.0) as i16;
        }
    }
    out
}

fn chroma_vertical(
    tmp: &[i16],
    guide: &[u8],
    w: usize,
    h: usize,
    r: usize,
    sw: &[f32],
    guide_lut: &[f32],
    chroma_lut: &[f32],
) -> Vec<i16> {
    let mut out = vec![0i16; w * h];
    for y in 0..h {
        let y0 = y.saturating_sub(r);
        let y1 = (y + r).min(h - 1);
        for x in 0..w {
            let g = guide[y * w + x] as i32;
            let cv = tmp[y * w + x] as i32;
            let mut acc = 0.0f32;
            let mut wsum = 0.0f32;
            for yy in y0..=y1 {
                let s = tmp[yy * w + x] as i32;
                let gd = guide[yy * w + x] as i32;
                let wt = sw[yy + r - y]
                    * guide_lut[(gd - g).unsigned_abs() as usize]
                    * chroma_lut[(s - cv).unsigned_abs() as usize];
                acc += wt * s as f32;
                wsum += wt;
            }
            out[y * w + x] = (acc / wsum).round().clamp(-1024.0, 1023.0) as i16;
        }
    }
    out
}

/// 2x2 box downsample of an i16 plane.
fn half_down(src: &[i16], w: usize, h: usize) -> (Vec<i16>, usize, usize) {
    let hw = (w + 1) / 2;
    let hh = (h + 1) / 2;
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
            out[y * hw + x] = ((s + 2) / 4) as i16;
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

/// Bilinear upsample of an i16 plane back to w x h.
fn half_up(src: &[i16], sw: usize, sh: usize, w: usize, h: usize) -> Vec<i16> {
    let mut out = vec![0i16; w * h];
    let sx = sw as f32 / w as f32;
    let sy = sh as f32 / h as f32;
    for y in 0..h {
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = fy - y0 as f32;
        for x in 0..w {
            let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let wx = fx - x0 as f32;
            let a = src[y0 * sw + x0] as f32;
            let b = src[y0 * sw + x1] as f32;
            let c = src[y1 * sw + x0] as f32;
            let d = src[y1 * sw + x1] as f32;
            let top = a + (b - a) * wx;
            let bot = c + (d - c) * wx;
            out[y * w + x] = (top + (bot - top) * wy).round() as i16;
        }
    }
    out
}

/// Denoise `buf` (RGBA, `w`×`h`) in place. `strength` 0..=1; 0 is a no-op.
pub fn smart_denoise(buf: &mut [u8], w: usize, h: usize, strength: f32) {
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

    let scale = w.max(h) as f32 / 1000.0;
    let r_y = ((0.9 * t * scale).round() as usize).clamp(1, MAX_LUMA_R);
    // Chroma runs at half resolution, so its radius is in half-res pixels.
    let r_c = ((1.0 * t * scale).round() as usize).clamp(1, MAX_CHROMA_R);
    let sy = 4.0 + 14.0 * t;
    let sg = 8.0 + 24.0 * t;
    let sc = 10.0 + 20.0 * t;

    let sw_y = spatial(r_y);
    let lut_y = range(sy);
    let tmp_y = luma_horizontal(&y, w, h, r_y, &sw_y, &lut_y);
    let filt_y = luma_vertical(&tmp_y, w, h, r_y, &sw_y, &lut_y);

    let sw_c = spatial(r_c);
    let lut_g = range(sg);
    let lut_c = range(sc);
    let (cb_h, hw, hh) = half_down(&cb, w, h);
    let (cr_h, _, _) = half_down(&cr, w, h);
    let y_h = half_down_luma(&y, w, h, hw, hh);
    let tmp_cb = chroma_horizontal(&cb_h, &y_h, hw, hh, r_c, &sw_c, &lut_g, &lut_c);
    let v_cb = chroma_vertical(&tmp_cb, &y_h, hw, hh, r_c, &sw_c, &lut_g, &lut_c);
    let filt_cb = half_up(&v_cb, hw, hh, w, h);
    let tmp_cr = chroma_horizontal(&cr_h, &y_h, hw, hh, r_c, &sw_c, &lut_g, &lut_c);
    let v_cr = chroma_vertical(&tmp_cr, &y_h, hw, hh, r_c, &sw_c, &lut_g, &lut_c);
    let filt_cr = half_up(&v_cr, hw, hh, w, h);

    // Radii and sigmas already carry the strength, so the filtered result is
    // used whole; blending again would square the slider's effect.
    for i in 0..n {
        let yd = filt_y[i] as f32;
        let cbd = filt_cb[i] as f32;
        let crd = filt_cr[i] as f32;
        let p = i * 4;
        buf[p] = (yd + 1.402 * crd).round().clamp(0.0, 255.0) as u8;
        buf[p + 1] = (yd - 0.344136 * cbd - 0.714136 * crd)
            .round()
            .clamp(0.0, 255.0) as u8;
        buf[p + 2] = (yd + 1.772 * cbd).round().clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn zero_strength_is_a_noop() {
        let orig = gray_noisy(64, 64, 120, 10, 7);
        let mut v = orig.clone();
        smart_denoise(&mut v, 64, 64, 0.0);
        assert_eq!(v, orig);
    }

    #[test]
    fn flat_noise_is_smoothed() {
        let orig = gray_noisy(96, 96, 120, 12, 3);
        let mut v = orig.clone();
        // 96px image → tiny radii, so use a strong local check instead of
        // absolute numbers: neighbour differences must shrink a lot.
        smart_denoise(&mut v, 96, 96, 1.0);
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
        smart_denoise(&mut d, w, h, 1.0);
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
        smart_denoise(&mut d, w, h, 1.0);
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
