//! Gaussian blur, reimplemented in Rust (Compositor's is CoreImage-backed on
//! Mac, so there is nothing to port — this is the standard algorithm):
//! separable two-pass convolution with a sampled gaussian kernel, edge-clamped
//! taps. `amount` is 0..=100 mapping to sigma 0..8 px on a 2048 px long edge,
//! scaled to the render through [`Place`] so the preview, a window and a
//! full-size export blur the same fraction of the image (#47). Sigmas wider
//! than [`DIRECT_MAX_SIGMA`] run on a box-downsampled frame and come back up
//! bilinearly, which keeps a full-size export's cost bounded. Alpha is blurred
//! along with color like any other channel.

use crate::place::Place;

/// Widest sigma the direct kernel runs. The GPU's kernel holds radius 24,
/// which is 3 sigma at 8, so wider blurs stay on the CPU.
pub const DIRECT_MAX_SIGMA: f32 = 8.0;

fn kernel(sigma: f32) -> Vec<f32> {
    let radius = (sigma * 3.0).ceil() as i32;
    let mut k = Vec::with_capacity((radius * 2 + 1) as usize);
    let mut sum = 0.0;
    for i in -radius..=radius {
        let v = (-0.5 * (i as f32 / sigma).powi(2)).exp();
        k.push(v);
        sum += v;
    }
    for v in &mut k {
        *v /= sum;
    }
    k
}

fn blur_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, k: &[f32], horizontal: bool) {
    let radius = (k.len() / 2) as i32;
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 4];
            for (ki, &kv) in k.iter().enumerate() {
                let off = ki as i32 - radius;
                let (sx, sy) = if horizontal {
                    ((x as i32 + off).clamp(0, w as i32 - 1), y as i32)
                } else {
                    (x as i32, (y as i32 + off).clamp(0, h as i32 - 1))
                };
                let p = &src[(sy as usize * w + sx as usize) * 4..];
                for c in 0..4 {
                    acc[c] += p[c] as f32 * kv;
                }
            }
            let p = &mut dst[(y * w + x) * 4..];
            for c in 0..4 {
                p[c] = acc[c].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// Blur in place. amount 0..=100; 0 is a no-op.
pub fn blur_apply(rgba: &mut [u8], width: usize, height: usize, amount: f32, place: Place) {
    if amount <= 0.0 {
        return;
    }
    let sigma = place.blur_sigma(amount);
    if sigma > DIRECT_MAX_SIGMA {
        large(rgba, width, height, sigma, |v| v as f32, |v| v.round().clamp(0.0, 255.0) as u8);
        return;
    }
    let k = kernel(sigma);
    let mut tmp = rgba.to_vec();
    blur_pass(rgba, &mut tmp, width, height, &k, true);
    blur_pass(&tmp, rgba, width, height, &k, false);
}

fn blur_pass_wide(src: &[u16], dst: &mut [u16], w: usize, h: usize, k: &[f32], horizontal: bool) {
    let radius = (k.len() / 2) as i32;
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 4];
            for (ki, &kv) in k.iter().enumerate() {
                let off = ki as i32 - radius;
                let (sx, sy) = if horizontal {
                    ((x as i32 + off).clamp(0, w as i32 - 1), y as i32)
                } else {
                    (x as i32, (y as i32 + off).clamp(0, h as i32 - 1))
                };
                let p = &src[(sy as usize * w + sx as usize) * 4..];
                for c in 0..4 {
                    acc[c] += p[c] as f32 * kv;
                }
            }
            let p = &mut dst[(y * w + x) * 4..];
            for c in 0..4 {
                p[c] = acc[c].round().clamp(0.0, 65535.0) as u16;
            }
        }
    }
}

/// 16-bit variant used by the wide (develop-active) colour stage.
pub fn blur_apply_wide(rgba: &mut [u16], width: usize, height: usize, amount: f32, place: Place) {
    if amount <= 0.0 {
        return;
    }
    let sigma = place.blur_sigma(amount);
    if sigma > DIRECT_MAX_SIGMA {
        large(rgba, width, height, sigma, |v| v as f32, |v| v.round().clamp(0.0, 65535.0) as u16);
        return;
    }
    let k = kernel(sigma);
    let mut tmp = rgba.to_vec();
    blur_pass_wide(rgba, &mut tmp, width, height, &k, true);
    blur_pass_wide(&tmp, rgba, width, height, &k, false);
}

/// 16-bit masked blur: blends the blurred value by mask/255.
pub fn blur_apply_wide_masked(
    rgba: &mut [u16],
    mask: &[u8],
    width: usize,
    height: usize,
    amount: f32,
    place: Place,
) {
    if amount <= 0.0 {
        return;
    }
    let mut blurred = rgba.to_vec();
    blur_apply_wide(&mut blurred, width, height, amount, place);
    for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
        let m = mask[i] as u32;
        if m == 0 {
            continue;
        }
        for c in 0..4 {
            px[c] = ((px[c] as u32 * (255 - m) + blurred[i * 4 + c] as u32 * m + 127) / 255) as u16;
        }
    }
}

/// Masked variant: blends the blurred value by mask/255.
pub fn blur_apply_masked(rgba: &mut [u8], mask: &[u8], width: usize, height: usize, amount: f32, place: Place) {
    if amount <= 0.0 {
        return;
    }
    let mut blurred = rgba.to_vec();
    blur_apply(&mut blurred, width, height, amount, place);
    for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
        let m = mask[i] as u32;
        if m == 0 {
            continue;
        }
        for c in 0..4 {
            px[c] = ((px[c] as u32 * (255 - m) + blurred[i * 4 + c] as u32 * m + 127) / 255) as u8;
        }
    }
}

fn blur_pass_f32(src: &[f32], dst: &mut [f32], w: usize, h: usize, k: &[f32], horizontal: bool) {
    let radius = (k.len() / 2) as i32;
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 4];
            for (ki, &kv) in k.iter().enumerate() {
                let off = ki as i32 - radius;
                let (sx, sy) = if horizontal {
                    ((x as i32 + off).clamp(0, w as i32 - 1), y as i32)
                } else {
                    (x as i32, (y as i32 + off).clamp(0, h as i32 - 1))
                };
                let p = (sy as usize * w + sx as usize) * 4;
                for c in 0..4 {
                    acc[c] += src[p + c] * kv;
                }
            }
            dst[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&acc);
        }
    }
}

/// A sigma wider than the direct kernel: box-downsample by `k`, blur the
/// small frame, then bilinear back up. The three stages' variances add up to
/// `sigma^2` — the box contributes (k^2-1)/12, the bilinear tent about k^2/6
/// — so the result tracks a direct Gaussian, at 1/k^2 of the taps.
fn large<T: Copy>(buf: &mut [T], w: usize, h: usize, sigma: f32, get: impl Fn(T) -> f32, put: impl Fn(f32) -> T) {
    if w == 0 || h == 0 {
        return;
    }
    let k = ((sigma / 4.0).ceil() as usize).max(2);
    let (sw, sh) = (w.div_ceil(k), h.div_ceil(k));
    let mut small = vec![0f32; sw * sh * 4];
    for sy in 0..sh {
        let (y0, y1) = (sy * k, (sy * k + k).min(h));
        for sx in 0..sw {
            let (x0, x1) = (sx * k, (sx * k + k).min(w));
            let mut acc = [0f32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = (y * w + x) * 4;
                    for c in 0..4 {
                        acc[c] += get(buf[p + c]);
                    }
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as f32;
            let q = (sy * sw + sx) * 4;
            for c in 0..4 {
                small[q + c] = acc[c] / n;
            }
        }
    }
    let kf = k as f32;
    let var = sigma * sigma - (kf * kf - 1.0) / 12.0 - kf * kf / 6.0;
    let s = var.max(0.25 * kf * kf).sqrt() / kf;
    let kern = kernel(s);
    let mut tmp = vec![0f32; small.len()];
    blur_pass_f32(&small, &mut tmp, sw, sh, &kern, true);
    blur_pass_f32(&tmp, &mut small, sw, sh, &kern, false);
    let (mx, my) = ((sw - 1) as f32, (sh - 1) as f32);
    for y in 0..h {
        let fy = ((y as f32 + 0.5) / kf - 0.5).clamp(0.0, my);
        let y0 = fy as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let ty = fy - y0 as f32;
        for x in 0..w {
            let fx = ((x as f32 + 0.5) / kf - 0.5).clamp(0.0, mx);
            let x0 = fx as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let tx = fx - x0 as f32;
            let (a, b) = ((y0 * sw + x0) * 4, (y0 * sw + x1) * 4);
            let (c2, d) = ((y1 * sw + x0) * 4, (y1 * sw + x1) * 4);
            let p = (y * w + x) * 4;
            for c in 0..4 {
                let top = small[a + c] + (small[b + c] - small[a + c]) * tx;
                let bot = small[c2 + c] + (small[d + c] - small[c2 + c]) * tx;
                buf[p + c] = put(top + (bot - top) * ty);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, v: u8) -> Vec<u8> {
        let mut d = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            d.extend_from_slice(&[v, v, v, 255]);
        }
        d
    }

    #[test]
    fn zero_amount_is_noop() {
        let mut img = solid(8, 8, 128);
        let orig = img.clone();
        blur_apply(&mut img, 8, 8, 0.0, Place::PIXEL);
        assert_eq!(img, orig);
    }

    #[test]
    fn flat_image_unchanged() {
        let mut img = solid(16, 16, 77);
        blur_apply(&mut img, 16, 16, 50.0, Place::PIXEL);
        assert!(img.chunks_exact(4).all(|p| (p[0] as i32 - 77).abs() <= 1));
    }

    #[test]
    fn impulse_spreads_to_neighbors() {
        let mut img = solid(21, 21, 0);
        let c = (10 * 21 + 10) * 4;
        img[c] = 255;
        img[c + 1] = 255;
        img[c + 2] = 255;
        blur_apply(&mut img, 21, 21, 40.0, Place::PIXEL);
        let at = |x: usize, y: usize| img[(y * 21 + x) * 4];
        assert!(at(10, 10) < 255 && at(10, 10) > at(8, 10));
        assert!(at(8, 10) > at(5, 10));
        assert!(at(5, 10) > 0, "blur reaches 5px out");
        // Transpose symmetry, off by at most 1: the horizontal pass rounds to
        // u8 before the vertical pass runs.
        assert!((at(8, 10) as i32 - at(10, 8) as i32).abs() <= 1, "transpose symmetric");
    }

    #[test]
    fn preserves_total_brightness() {
        let mut img = Vec::with_capacity(32 * 32 * 4);
        for y in 0..32 {
            for x in 0..32 {
                img.extend_from_slice(&[((x * 7 + y * 13) % 256) as u8, 128, 64, 255]);
            }
        }
        let before: u64 = img.chunks_exact(4).map(|p| p[0] as u64).sum();
        blur_apply(&mut img, 32, 32, 60.0, Place::PIXEL);
        let after: u64 = img.chunks_exact(4).map(|p| p[0] as u64).sum();
        let drift = (after as i64 - before as i64).abs() as f64 / before as f64;
        assert!(drift < 0.02, "brightness drift {drift}");
    }

    #[test]
    fn masked_blends_toward_blurred() {
        let mut img = solid(16, 16, 0);
        for i in 0..16 * 16 / 2 {
            img[i * 4] = 200;
            img[i * 4 + 1] = 200;
            img[i * 4 + 2] = 200;
        }
        let mut full = img.clone();
        blur_apply(&mut full, 16, 16, 50.0, Place::PIXEL);
        let mask: Vec<u8> = (0..256).map(|i| if i % 2 == 0 { 255 } else { 0 }).collect();
        blur_apply_masked(&mut img, &mask, 16, 16, 50.0, Place::PIXEL);
        for i in 0..256 {
            let got = &img[i * 4..i * 4 + 3];
            if i % 2 == 0 {
                assert_eq!(got, &full[i * 4..i * 4 + 3], "pixel {i} fully masked");
            }
        }
    }

    #[test]
    fn wide_flat_image_unchanged() {
        let v = 77u16 * 257;
        let mut img = vec![v; 16 * 16 * 4];
        blur_apply_wide(&mut img, 16, 16, 50.0, Place::PIXEL);
        assert!(img.chunks_exact(4).all(|p| (p[0] as i32 - v as i32).abs() <= 257));
    }

    #[test]
    fn wide_impulse_spreads_to_neighbors() {
        let mut img = vec![0u16; 21 * 21 * 4];
        let c = (10 * 21 + 10) * 4;
        img[c..c + 3].fill(65535);
        img[c + 3] = 65535;
        blur_apply_wide(&mut img, 21, 21, 40.0, Place::PIXEL);
        let at = |x: usize, y: usize| img[(y * 21 + x) * 4];
        assert!(at(10, 10) < 65535 && at(10, 10) > at(8, 10));
        assert!(at(8, 10) > at(5, 10));
        assert!(at(5, 10) > 0, "blur reaches 5px out");
    }

    #[test]
    fn checkerboard_converges_to_mean() {
        let mut img = Vec::with_capacity(24 * 24 * 4);
        for y in 0..24 {
            for x in 0..24 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                img.extend_from_slice(&[v, v, v, 255]);
            }
        }
        blur_apply(&mut img, 24, 24, 100.0, Place::PIXEL);
        let center = &img[(12 * 24 + 12) * 4..];
        assert!((center[0] as i32 - 128).abs() < 24, "center {}", center[0]);
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mut se = 0.0f64;
        let mut n = 0.0f64;
        for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
            for c in 0..3 {
                let d = pa[c] as f64 - pb[c] as f64;
                se += d * d;
                n += 1.0;
            }
        }
        if se == 0.0 { 99.0 } else { 10.0 * (65025.0 / (se / n)).log10() }
    }

    /// Hard-edged discs and stripes, sampled at the centre of each pixel of a
    /// `w` x `h` render: the same scene at any resolution.
    fn scene(w: usize, h: usize) -> Vec<u8> {
        let mut d = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let u = (x as f64 + 0.5) / w as f64;
                let v = (y as f64 + 0.5) / w as f64;
                let disc = ((u - 0.3).powi(2) + (v - 0.3).powi(2)).sqrt() < 0.12;
                let stripe = ((u * 24.0) as i32) % 2 == 0 && v > 0.45;
                let r = if disc { 230 } else { 30 };
                let g = if stripe { 200 } else { 60 };
                d.extend_from_slice(&[r, g, ((u * 255.0) as u8), 255]);
            }
        }
        d
    }

    fn box_down(src: &[u8], w: usize, h: usize, k: usize) -> Vec<u8> {
        let (sw, sh) = (w / k, h / k);
        let mut out = Vec::with_capacity(sw * sh * 4);
        for y in 0..sh {
            for x in 0..sw {
                let mut acc = [0u32; 4];
                for yy in 0..k {
                    for xx in 0..k {
                        let p = ((y * k + yy) * w + x * k + xx) * 4;
                        for c in 0..4 {
                            acc[c] += src[p + c] as u32;
                        }
                    }
                }
                let n = (k * k) as u32;
                out.extend(acc.iter().map(|v| ((v + n / 2) / n) as u8));
            }
        }
        out
    }

    #[test]
    fn wide_sigma_path_tracks_a_direct_gaussian() {
        let (w, h) = (360, 240);
        let src = scene(w, h);
        for sigma in [9.0f32, 14.0, 23.0] {
            let mut direct = src.clone();
            let k = kernel(sigma);
            let mut tmp = direct.clone();
            blur_pass(&direct, &mut tmp, w, h, &k, true);
            blur_pass(&tmp, &mut direct, w, h, &k, false);
            let mut fast = src.clone();
            large(&mut fast, w, h, sigma, |v| v as f32, |v| v.round().clamp(0.0, 255.0) as u8);
            let db = psnr(&direct, &fast);
            assert!(db >= 38.0, "sigma {sigma}: {db:.2} dB");
        }
    }

    #[test]
    fn the_same_blur_amount_looks_the_same_at_every_render_size() {
        // A fit-sized preview against a 4x export downscaled to the same size.
        let (w, h) = (512, 340);
        for amount in [20.0f32, 60.0, 100.0] {
            let mut preview = scene(w, h);
            blur_apply(&mut preview, w, h, amount, Place::whole(w, h));
            let mut export = scene(w * 4, h * 4);
            blur_apply(&mut export, w * 4, h * 4, amount, Place::whole(w * 4, h * 4));
            let shown = box_down(&export, w * 4, h * 4, 4);
            let db = psnr(&preview, &shown);
            assert!(db >= 33.0, "amount {amount}: preview vs export {db:.2} dB");
        }
    }
}
