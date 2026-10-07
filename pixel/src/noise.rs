//! Film grain / additive noise, ported from Compositor (MIT, (c) Robbie Tilton):
//! Compositor/Rendering/NoisePixels.c. The C works on premultiplied pixels
//! (unpremultiply, add noise, re-premultiply); our buffers are
//! non-premultiplied, so the alpha round-trip drops out and alpha is left
//! untouched. The hash, Box–Muller gaussian, and channel decorrelation are
//! identical to the C, including the deterministic per-pixel seeding so
//! preview and export produce the same grain.

fn noise_hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846ca68b);
    x ^= x >> 16;
    x
}

/// Uniform in [0, 1).
fn noise_unit(key: u32) -> f32 {
    (noise_hash(key) >> 8) as f32 * (1.0 / 16777216.0)
}

/// Add noise in place. `amount` is 0..=100 (0 = off). `gaussian` switches from
/// uniform to normally distributed grain; `monochromatic` gives all channels
/// the same noise value (luma grain) instead of per-channel color noise.
pub fn noise_add(rgba: &mut [u8], width: usize, height: usize, amount: f32, gaussian: bool, monochromatic: bool, seed: u32) {
    if amount <= 0.0 {
        return;
    }
    let spread = amount / 100.0 * 127.5;
    for y in 0..height {
        for x in 0..width {
            let p = &mut rgba[(y * width + x) * 4..];
            let base = noise_hash(seed ^ noise_hash((y * width + x) as u32));
            for c in 0..3 {
                let key = if monochromatic { base } else { base.wrapping_add((c as u32).wrapping_mul(0x9e3779b9)) };
                let n = if gaussian {
                    let u1 = noise_unit(key);
                    let u2 = noise_unit(key ^ 0x68e31da4);
                    (-2.0 * (1.0 - u1).ln()).sqrt() * (std::f32::consts::TAU * u2).cos() * spread * (2.0 / 3.0)
                } else {
                    (noise_unit(key) * 2.0 - 1.0) * spread
                };
                p[c] = (p[c] as f32 + n).clamp(0.0, 255.0).round() as u8;
            }
        }
    }
}

/// Masked variant: blends the noisy value by mask/255. Mask length must be
/// width*height, 255 = fully affected.
pub fn noise_add_masked(rgba: &mut [u8], mask: &[u8], width: usize, height: usize, amount: f32, gaussian: bool, monochromatic: bool, seed: u32) {
    if amount <= 0.0 {
        return;
    }
    // Apply to a copy, then blend — cheaper than threading the mask through
    // the hash math, and identical in result.
    let mut noisy = rgba.to_vec();
    noise_add(&mut noisy, width, height, amount, gaussian, monochromatic, seed);
    for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
        let m = mask[i] as u32;
        if m == 0 {
            continue;
        }
        for c in 0..3 {
            px[c] = ((px[c] as u32 * (255 - m) + noisy[i * 4 + c] as u32 * m + 127) / 255) as u8;
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
        noise_add(&mut img, 8, 8, 0.0, false, false, 0);
        assert_eq!(img, orig);
    }

    #[test]
    fn deterministic_same_seed() {
        let mut a = solid(16, 16, 128);
        let mut b = a.clone();
        noise_add(&mut a, 16, 16, 30.0, true, false, 7);
        noise_add(&mut b, 16, 16, 30.0, true, false, 7);
        assert_eq!(a, b);
    }

    #[test]
    fn different_seed_differs() {
        let mut a = solid(16, 16, 128);
        let mut b = a.clone();
        noise_add(&mut a, 16, 16, 30.0, false, false, 1);
        noise_add(&mut b, 16, 16, 30.0, false, false, 2);
        assert_ne!(a, b);
    }

    #[test]
    fn noise_centers_on_zero_mean() {
        let mut img = solid(64, 64, 128);
        noise_add(&mut img, 64, 64, 50.0, true, false, 0);
        let sum: i64 = img.chunks_exact(4).map(|p| p[0] as i64 - 128).sum();
        let mean = sum as f32 / (64.0 * 64.0);
        assert!(mean.abs() < 2.0, "mean drift {mean}");
    }

    #[test]
    fn gaussian_scales_variance_by_four_thirds_vs_uniform() {
        // Uniform on [-s, s] has variance s²/3; Box–Muller gaussian scaled by
        // 2/3 has variance (2s/3)² = 4s²/9. Ratio = 4/3.
        let mut g = solid(64, 64, 128);
        let mut u = g.clone();
        noise_add(&mut g, 64, 64, 60.0, true, false, 3);
        noise_add(&mut u, 64, 64, 60.0, false, false, 3);
        let var = |img: &[u8]| -> f32 {
            img.chunks_exact(4).map(|p| (p[0] as f32 - 128.0).powi(2)).sum::<f32>() / (64.0 * 64.0)
        };
        let ratio = var(&g) / var(&u);
        assert!((ratio - 4.0 / 3.0).abs() < 0.25, "variance ratio {ratio}");
    }

    #[test]
    fn monochromatic_keeps_channels_equal() {
        let mut img = solid(16, 16, 100);
        noise_add(&mut img, 16, 16, 50.0, false, true, 0);
        assert!(img.chunks_exact(4).all(|p| p[0] == p[1] && p[1] == p[2]));
    }

    #[test]
    fn color_noise_decorrelates_channels() {
        let mut img = solid(16, 16, 100);
        noise_add(&mut img, 16, 16, 50.0, false, false, 0);
        assert!(img.chunks_exact(4).any(|p| p[0] != p[1] || p[1] != p[2]));
    }

    #[test]
    fn clamps_at_extremes() {
        let mut img = solid(8, 8, 250);
        noise_add(&mut img, 8, 8, 100.0, false, false, 0);
        assert!(img.chunks_exact(4).all(|p| p[0] <= 255 && p[1] <= 255 && p[2] <= 255));
        let mut img = solid(8, 8, 5);
        noise_add(&mut img, 8, 8, 100.0, false, false, 0);
        assert!(img.chunks_exact(4).all(|p| p[0] <= 255));
    }

    #[test]
    fn masked_blends_toward_noisy() {
        let mut img = solid(8, 8, 128);
        let mut full = img.clone();
        noise_add(&mut full, 8, 8, 80.0, false, false, 0);
        let mask: Vec<u8> = (0..64).map(|i| if i < 32 { 255 } else { 0 }).collect();
        noise_add_masked(&mut img, &mask, 8, 8, 80.0, false, false, 0);
        for i in 0..64 {
            let got = &img[i * 4..i * 4 + 3];
            if i < 32 {
                assert_eq!(got, &full[i * 4..i * 4 + 3], "pixel {i} fully masked");
            } else {
                assert_eq!(got, &[128, 128, 128], "pixel {i} unmasked");
            }
        }
    }
}
