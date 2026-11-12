//! Film grain / additive noise, ported from Compositor (MIT, (c) Robbie Tilton):
//! Compositor/Rendering/NoisePixels.c. The C works on premultiplied pixels
//! (unpremultiply, add noise, re-premultiply); our buffers are
//! non-premultiplied, so the alpha round-trip drops out and alpha is left
//! untouched. The hash, Box–Muller gaussian, and channel decorrelation are
//! identical to the C. The seeding is not per output pixel as in the C: each
//! pixel takes the cell of a fixed lattice in image space ([`Place`], one
//! cell per pixel of a 2048 px long edge), and cells smaller than a render
//! pixel scale the amplitude down as averaging would. So the preview, a
//! window of the frame and a full-size export show the same grain (#47).

use crate::place::{cell_index, Place};

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
pub fn noise_add(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    amount: f32,
    gaussian: bool,
    monochromatic: bool,
    seed: u32,
    place: Place,
) {
    if amount <= 0.0 {
        return;
    }
    let spread = amount / 100.0 * 127.5 * place.grain_gain();
    let cols: Vec<u32> = (0..width).map(|x| place.cell_x(x)).collect();
    for y in 0..height {
        let row = place.cell_y(y);
        for x in 0..width {
            let p = &mut rgba[(y * width + x) * 4..];
            let base = noise_hash(seed ^ noise_hash(cell_index(cols[x], row)));
            for (c, px) in p.iter_mut().take(3).enumerate() {
                let key = if monochromatic { base } else { base.wrapping_add((c as u32).wrapping_mul(0x9e3779b9)) };
                let n = if gaussian {
                    let u1 = noise_unit(key);
                    let u2 = noise_unit(key ^ 0x68e31da4);
                    (-2.0 * (1.0 - u1).ln()).sqrt() * (std::f32::consts::TAU * u2).cos() * spread * (2.0 / 3.0)
                } else {
                    (noise_unit(key) * 2.0 - 1.0) * spread
                };
                *px = (*px as f32 + n).clamp(0.0, 255.0).round() as u8;
            }
        }
    }
}

/// Masked variant: blends the noisy value by mask/255. Mask length must be
/// width*height, 255 = fully affected.
pub fn noise_add_masked(
    rgba: &mut [u8],
    mask: &[u8],
    width: usize,
    height: usize,
    amount: f32,
    gaussian: bool,
    monochromatic: bool,
    seed: u32,
    place: Place,
) {
    if amount <= 0.0 {
        return;
    }
    // Apply to a copy, then blend — cheaper than threading the mask through
    // the hash math, and identical in result.
    let mut noisy = rgba.to_vec();
    noise_add(&mut noisy, width, height, amount, gaussian, monochromatic, seed, place);
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

/// 16-bit variant used by the wide (develop-active) colour stage. The amount
/// is still in 8-bit levels; one level is 257 wide units.
pub fn noise_add_wide(
    rgba: &mut [u16],
    width: usize,
    height: usize,
    amount: f32,
    gaussian: bool,
    monochromatic: bool,
    seed: u32,
    place: Place,
) {
    if amount <= 0.0 {
        return;
    }
    let spread = amount / 100.0 * 127.5 * place.grain_gain() * 257.0;
    let cols: Vec<u32> = (0..width).map(|x| place.cell_x(x)).collect();
    for y in 0..height {
        let row = place.cell_y(y);
        for x in 0..width {
            let p = &mut rgba[(y * width + x) * 4..];
            let base = noise_hash(seed ^ noise_hash(cell_index(cols[x], row)));
            for (c, px) in p.iter_mut().take(3).enumerate() {
                let key = if monochromatic { base } else { base.wrapping_add((c as u32).wrapping_mul(0x9e3779b9)) };
                let n = if gaussian {
                    let u1 = noise_unit(key);
                    let u2 = noise_unit(key ^ 0x68e31da4);
                    (-2.0 * (1.0 - u1).ln()).sqrt() * (std::f32::consts::TAU * u2).cos() * spread * (2.0 / 3.0)
                } else {
                    (noise_unit(key) * 2.0 - 1.0) * spread
                };
                *px = (*px as f32 + n).clamp(0.0, 65535.0).round() as u16;
            }
        }
    }
}

/// Masked 16-bit noise: blends the noisy value by mask/255.
pub fn noise_add_wide_masked(
    rgba: &mut [u16],
    mask: &[u8],
    width: usize,
    height: usize,
    amount: f32,
    gaussian: bool,
    monochromatic: bool,
    seed: u32,
    place: Place,
) {
    if amount <= 0.0 {
        return;
    }
    let mut noisy = rgba.to_vec();
    noise_add_wide(&mut noisy, width, height, amount, gaussian, monochromatic, seed, place);
    for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
        let m = mask[i] as u32;
        if m == 0 {
            continue;
        }
        for c in 0..3 {
            px[c] = ((px[c] as u32 * (255 - m) + noisy[i * 4 + c] as u32 * m + 127) / 255) as u16;
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
        noise_add(&mut img, 8, 8, 0.0, false, false, 0, Place::PIXEL);
        assert_eq!(img, orig);
    }

    #[test]
    fn deterministic_same_seed() {
        let mut a = solid(16, 16, 128);
        let mut b = a.clone();
        noise_add(&mut a, 16, 16, 30.0, true, false, 7, Place::PIXEL);
        noise_add(&mut b, 16, 16, 30.0, true, false, 7, Place::PIXEL);
        assert_eq!(a, b);
    }

    #[test]
    fn different_seed_differs() {
        let mut a = solid(16, 16, 128);
        let mut b = a.clone();
        noise_add(&mut a, 16, 16, 30.0, false, false, 1, Place::PIXEL);
        noise_add(&mut b, 16, 16, 30.0, false, false, 2, Place::PIXEL);
        assert_ne!(a, b);
    }

    #[test]
    fn noise_centers_on_zero_mean() {
        let mut img = solid(64, 64, 128);
        noise_add(&mut img, 64, 64, 50.0, true, false, 0, Place::PIXEL);
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
        noise_add(&mut g, 64, 64, 60.0, true, false, 3, Place::PIXEL);
        noise_add(&mut u, 64, 64, 60.0, false, false, 3, Place::PIXEL);
        let var = |img: &[u8]| -> f32 {
            img.chunks_exact(4).map(|p| (p[0] as f32 - 128.0).powi(2)).sum::<f32>() / (64.0 * 64.0)
        };
        let ratio = var(&g) / var(&u);
        assert!((ratio - 4.0 / 3.0).abs() < 0.25, "variance ratio {ratio}");
    }

    #[test]
    fn monochromatic_keeps_channels_equal() {
        let mut img = solid(16, 16, 100);
        noise_add(&mut img, 16, 16, 50.0, false, true, 0, Place::PIXEL);
        assert!(img.chunks_exact(4).all(|p| p[0] == p[1] && p[1] == p[2]));
    }

    #[test]
    fn color_noise_decorrelates_channels() {
        let mut img = solid(16, 16, 100);
        noise_add(&mut img, 16, 16, 50.0, false, false, 0, Place::PIXEL);
        assert!(img.chunks_exact(4).any(|p| p[0] != p[1] || p[1] != p[2]));
    }

    #[test]
    fn clamps_at_extremes() {
        let mut img = solid(8, 8, 250);
        noise_add(&mut img, 8, 8, 100.0, false, false, 0, Place::PIXEL);
        assert!(img.chunks_exact(4).all(|p| p[0] <= 255 && p[1] <= 255 && p[2] <= 255));
        let mut img = solid(8, 8, 5);
        noise_add(&mut img, 8, 8, 100.0, false, false, 0, Place::PIXEL);
        assert!(img.chunks_exact(4).all(|p| p[0] <= 255));
    }

    #[test]
    fn wide_noise_is_deterministic_and_scales_with_levels() {
        let base = 128u16 * 257;
        let mut a = vec![base; 16 * 16 * 4];
        let mut b = a.clone();
        noise_add_wide(&mut a, 16, 16, 30.0, true, false, 7, Place::PIXEL);
        noise_add_wide(&mut b, 16, 16, 30.0, true, false, 7, Place::PIXEL);
        assert_eq!(a, b);
        let mean = a.iter().step_by(4).map(|&v| v as f32 / 257.0 - 128.0).sum::<f32>() / 256.0;
        assert!(mean.abs() < 2.0, "mean drift {mean}");
    }

    #[test]
    fn masked_blends_toward_noisy() {
        let mut img = solid(8, 8, 128);
        let mut full = img.clone();
        noise_add(&mut full, 8, 8, 80.0, false, false, 0, Place::PIXEL);
        let mask: Vec<u8> = (0..64).map(|i| if i < 32 { 255 } else { 0 }).collect();
        noise_add_masked(&mut img, &mask, 8, 8, 80.0, false, false, 0, Place::PIXEL);
        for i in 0..64 {
            let got = &img[i * 4..i * 4 + 3];
            if i < 32 {
                assert_eq!(got, &full[i * 4..i * 4 + 3], "pixel {i} fully masked");
            } else {
                assert_eq!(got, &[128, 128, 128], "pixel {i} unmasked");
            }
        }
    }

    fn luma_sd(img: &[u8]) -> f64 {
        let n = (img.len() / 4) as f64;
        let mean = img.chunks_exact(4).map(|p| p[0] as f64).sum::<f64>() / n;
        (img.chunks_exact(4).map(|p| (p[0] as f64 - mean).powi(2)).sum::<f64>() / n).sqrt()
    }

    fn box_down(src: &[u8], w: usize, h: usize, k: usize) -> Vec<u8> {
        let (sw, sh) = (w / k, h / k);
        let mut out = Vec::with_capacity(sw * sh * 4);
        for y in 0..sh {
            for x in 0..sw {
                let mut acc = 0u32;
                for yy in 0..k {
                    for xx in 0..k {
                        acc += src[((y * k + yy) * w + x * k + xx) * 4] as u32;
                    }
                }
                let v = ((acc as f64) / (k * k) as f64).round() as u8;
                out.extend_from_slice(&[v, v, v, 255]);
            }
        }
        out
    }

    #[test]
    fn grain_looks_as_strong_at_every_render_size() {
        // A 512 px preview against 2048 and 4096 px exports downscaled to it:
        // the grain's visible strength agrees (#47). Before, the preview
        // showed 4-8x the export's grain.
        let (w, h) = (512usize, 256usize);
        for (amount, gaussian) in [(30.0f32, false), (60.0, true)] {
            let mut preview = solid(w, h, 128);
            noise_add(&mut preview, w, h, amount, gaussian, true, 5, Place::whole(w, h));
            let want = luma_sd(&preview);
            for k in [4usize, 8] {
                let mut export = solid(w * k, h * k, 128);
                noise_add(&mut export, w * k, h * k, amount, gaussian, true, 5, Place::whole(w * k, h * k));
                let got = luma_sd(&box_down(&export, w * k, h * k, k));
                let ratio = got / want;
                assert!((0.85..=1.15).contains(&ratio), "amount {amount} x{k}: export {got:.2} vs preview {want:.2}");
            }
        }
    }
}
