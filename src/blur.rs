//! Gaussian blur, reimplemented in Rust (Compositor's is CoreImage-backed on
//! Mac, so there is nothing to port — this is the standard algorithm):
//! separable two-pass convolution with a sampled gaussian kernel, edge-clamped
//! taps. `amount` is 0..=100 mapping to sigma 0..8 px. Alpha is blurred along
//! with color like any other channel.

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
pub fn blur_apply(rgba: &mut [u8], width: usize, height: usize, amount: f32) {
    if amount <= 0.0 {
        return;
    }
    let sigma = amount / 100.0 * 8.0;
    let k = kernel(sigma);
    let mut tmp = rgba.to_vec();
    blur_pass(rgba, &mut tmp, width, height, &k, true);
    blur_pass(&tmp, rgba, width, height, &k, false);
}

/// Masked variant: blends the blurred value by mask/255.
pub fn blur_apply_masked(rgba: &mut [u8], mask: &[u8], width: usize, height: usize, amount: f32) {
    if amount <= 0.0 {
        return;
    }
    let mut blurred = rgba.to_vec();
    blur_apply(&mut blurred, width, height, amount);
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
        blur_apply(&mut img, 8, 8, 0.0);
        assert_eq!(img, orig);
    }

    #[test]
    fn flat_image_unchanged() {
        let mut img = solid(16, 16, 77);
        blur_apply(&mut img, 16, 16, 50.0);
        assert!(img.chunks_exact(4).all(|p| (p[0] as i32 - 77).abs() <= 1));
    }

    #[test]
    fn impulse_spreads_to_neighbors() {
        let mut img = solid(21, 21, 0);
        let c = (10 * 21 + 10) * 4;
        img[c] = 255;
        img[c + 1] = 255;
        img[c + 2] = 255;
        blur_apply(&mut img, 21, 21, 40.0);
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
        blur_apply(&mut img, 32, 32, 60.0);
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
        blur_apply(&mut full, 16, 16, 50.0);
        let mask: Vec<u8> = (0..256).map(|i| if i % 2 == 0 { 255 } else { 0 }).collect();
        blur_apply_masked(&mut img, &mask, 16, 16, 50.0);
        for i in 0..256 {
            let got = &img[i * 4..i * 4 + 3];
            if i % 2 == 0 {
                assert_eq!(got, &full[i * 4..i * 4 + 3], "pixel {i} fully masked");
            }
        }
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
        blur_apply(&mut img, 24, 24, 100.0);
        let center = &img[(12 * 24 + 12) * 4..];
        assert!((center[0] as i32 - 128).abs() < 24, "center {}", center[0]);
    }
}
