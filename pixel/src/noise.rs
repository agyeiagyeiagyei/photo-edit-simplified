//! Film grain / additive noise, ported from Compositor (MIT, (c) Robbie Tilton):
//! Compositor/Rendering/NoisePixels.c. The C works on premultiplied pixels
//! (unpremultiply, add noise, re-premultiply); our buffers are
//! non-premultiplied, so the alpha round-trip drops out and alpha is left
//! untouched. The hash, Box–Muller gaussian, and channel decorrelation are
//! identical to the C. The seeding is not per output pixel as in the C: the
//! noise lives on a fixed lattice in image space ([`Place`], one cell per
//! pixel of a 2048 px long edge), and each render pixel takes the
//! overlap-weighted mean of the cells under it. So the preview, a window of
//! the frame and a full-size export downscaled to the screen show the same
//! grain (#47, #51).

use crate::place::{cell_index, GrainAxis, Place};

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

/// One lattice cell's noise for channel key `key`, before scaling by spread.
#[inline]
fn cell_noise(key: u32, gaussian: bool) -> f32 {
    if gaussian {
        let u1 = noise_unit(key);
        let u2 = noise_unit(key ^ 0x68e31da4);
        (-2.0 * (1.0 - u1).ln()).sqrt() * (std::f32::consts::TAU * u2).cos() * (2.0 / 3.0)
    } else {
        noise_unit(key) * 2.0 - 1.0
    }
}

/// The grain of a `width` x `height` render at `place`, row by row:
/// `emit(y, row)` gets `width * ch` values (ch = 1 for monochromatic grain,
/// else 3 per pixel), each the area sample of the lattice under the pixel
/// before scaling by spread. Separable: each lattice row is resampled across
/// once, and output rows mix the one to a few lattice rows under them.
fn grain_rows(width: usize, height: usize, gaussian: bool, mono: bool, seed: u32, place: Place, mut emit: impl FnMut(usize, &[f32])) {
    if width == 0 || height == 0 {
        return;
    }
    let ch = if mono { 1 } else { 3 };
    let cols: GrainAxis = place.grain_cols(width);
    let rows: GrainAxis = place.grain_rows(height);
    let c0 = cols.first[0];
    let ncell = (cols.end() - c0) as usize;
    let mut cells = vec![0f32; ncell * ch];
    // Lattice rows already resampled across, oldest first.
    let mut across: std::collections::VecDeque<(u32, Vec<f32>)> = std::collections::VecDeque::new();
    let mut out = vec![0f32; width * ch];
    for y in 0..height {
        let (r0, wy) = rows.at(y);
        while across.front().is_some_and(|(j, _)| *j < r0) {
            across.pop_front();
        }
        for j in r0..r0 + wy.len() as u32 {
            if across.iter().any(|(k, _)| *k == j) {
                continue;
            }
            for (i, cell) in cells.chunks_exact_mut(ch).enumerate() {
                let base = noise_hash(seed ^ noise_hash(cell_index(c0 + i as u32, j)));
                for (c, v) in cell.iter_mut().enumerate() {
                    *v = cell_noise(base.wrapping_add((c as u32).wrapping_mul(0x9e3779b9)), gaussian);
                }
            }
            let mut row = vec![0f32; width * ch];
            for (x, px) in row.chunks_exact_mut(ch).enumerate() {
                let (f, wx) = cols.at(x);
                let at = (f - c0) as usize;
                for (k, w) in wx.iter().enumerate() {
                    let cell = &cells[(at + k) * ch..(at + k + 1) * ch];
                    for (v, n) in px.iter_mut().zip(cell) {
                        *v += w * n;
                    }
                }
            }
            across.push_back((j, row));
        }
        out.fill(0.0);
        for (k, w) in wy.iter().enumerate() {
            let j = r0 + k as u32;
            let row = &across.iter().find(|(r, _)| *r == j).expect("resampled lattice row").1;
            for (o, v) in out.iter_mut().zip(row) {
                *o += w * v;
            }
        }
        emit(y, &out);
    }
}

/// The last small render's grain field, before scaling by spread.
struct GrainField {
    key: (u32, bool, bool, u64, u64, u64, usize, usize),
    field: Vec<f32>,
}

thread_local! {
    static GRAIN_FIELD: std::cell::RefCell<Option<GrainField>> = const { std::cell::RefCell::new(None) };
}

/// Largest field kept (values; 18 MB). It covers the renders whose cells are
/// smaller than their pixels, up to about 1.4 MP of colour grain.
const GRAIN_FIELD_MAX: usize = 4_500_000;

/// [`grain_rows`], cached for renders smaller than the lattice. Their cost is
/// the whole lattice (up to 2.8M cells) however few pixels they have, while
/// the field doesn't depend on the amount or on any other edit, so a slider
/// drag on the CPU preview reuses it and only scales and adds it.
fn grain_field(width: usize, height: usize, gaussian: bool, mono: bool, seed: u32, place: Place, mut emit: impl FnMut(usize, &[f32])) {
    let ch = if mono { 1 } else { 3 };
    let n = width * height * ch;
    if place.unit() >= 1.0 || n > GRAIN_FIELD_MAX {
        return grain_rows(width, height, gaussian, mono, seed, place, emit);
    }
    let key = (seed, gaussian, mono, place.long.to_bits(), place.ox.to_bits(), place.oy.to_bits(), width, height);
    GRAIN_FIELD.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.as_ref().map(|g| g.key) != Some(key) {
            *cache = None;
            let mut field = Vec::with_capacity(n);
            grain_rows(width, height, gaussian, mono, seed, place, |_, row| field.extend_from_slice(row));
            *cache = Some(GrainField { key, field });
        }
        let field = &cache.as_ref().expect("grain field").field;
        for (y, row) in field.chunks_exact(width * ch).enumerate() {
            emit(y, row);
        }
    });
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
    let ch = if monochromatic { 1 } else { 3 };
    grain_field(width, height, gaussian, monochromatic, seed, place, |y, g| {
        for (x, p) in rgba[y * width * 4..(y + 1) * width * 4].chunks_exact_mut(4).enumerate() {
            for (c, px) in p.iter_mut().take(3).enumerate() {
                let n = g[x * ch + c % ch] * spread;
                *px = (*px as f32 + n).clamp(0.0, 255.0).round() as u8;
            }
        }
    });
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
    let ch = if monochromatic { 1 } else { 3 };
    grain_field(width, height, gaussian, monochromatic, seed, place, |y, g| {
        for (x, p) in rgba[y * width * 4..(y + 1) * width * 4].chunks_exact_mut(4).enumerate() {
            for (c, px) in p.iter_mut().take(3).enumerate() {
                let n = g[x * ch + c % ch] * spread;
                *px = (*px as f32 + n).clamp(0.0, 65535.0).round() as u16;
            }
        }
    });
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

    /// Mean |Laplacian| of the luma: how much fine texture the grain has.
    fn luma_detail(img: &[u8], w: usize, h: usize) -> f64 {
        let l = |x: usize, y: usize| img[(y * w + x) * 4] as f64;
        let mut s = 0.0;
        for y in 1..h - 1 {
            for x in 1..w - 1 {
                s += (4.0 * l(x, y) - l(x - 1, y) - l(x + 1, y) - l(x, y - 1) - l(x, y + 1)).abs();
            }
        }
        s / ((w - 2) * (h - 2)) as f64
    }

    #[test]
    fn grain_matches_a_downscaled_export_off_the_lattice_grid() {
        // #51: previews whose pixel edges don't fall on cell edges, against
        // exports box-averaged to them: a 723 px preview (2.8 cells per
        // pixel) against 2892 and 5784 px exports, and a 2891 px hi-dpi
        // preview (cells 1.4 px wide) against a 5782 px export. Point
        // sampling showed the preview's grain up to 20% stronger and up to
        // 35% crisper than the export's; area sampling matches both.
        for (w, h, ks) in [(723usize, 241usize, &[4usize, 8][..]), (2891, 48, &[2][..])] {
            for (amount, gaussian, mono) in [(40.0f32, false, true), (60.0, true, true), (50.0, true, false)] {
                let mut preview = solid(w, h, 128);
                noise_add(&mut preview, w, h, amount, gaussian, mono, 9, Place::whole(w, h));
                for &k in ks {
                    let mut export = solid(w * k, h * k, 128);
                    noise_add(&mut export, w * k, h * k, amount, gaussian, mono, 9, Place::whole(w * k, h * k));
                    let down = box_down(&export, w * k, h * k, k);
                    let sd = luma_sd(&down) / luma_sd(&preview);
                    let detail = luma_detail(&down, w, h) / luma_detail(&preview, w, h);
                    let at = format!("{w} px, amount {amount}, x{k}");
                    assert!((0.95..=1.07).contains(&sd), "{at}: strength ratio {sd:.3}");
                    assert!((0.95..=1.07).contains(&detail), "{at}: texture ratio {detail:.3}");
                }
            }
        }
    }

    /// Exact area average of the luma down to `w` x `h` (fractional box).
    fn area_down(src: &[u8], sw: usize, sh: usize, w: usize, h: usize) -> Vec<u8> {
        let taps = |n: usize, m: usize| -> Vec<Vec<(usize, f64)>> {
            let k = n as f64 / m as f64;
            (0..m)
                .map(|o| {
                    let (a, b) = (o as f64 * k, (o + 1) as f64 * k);
                    (a.floor() as usize..(b.ceil() as usize).min(n))
                        .map(|i| (i, ((i + 1) as f64).min(b) - (i as f64).max(a)))
                        .filter(|&(_, v)| v > 1e-9)
                        .collect()
                })
                .collect()
        };
        let (tx, ty) = (taps(sw, w), taps(sh, h));
        let mut out = Vec::with_capacity(w * h * 4);
        for row in &ty {
            for col in &tx {
                let (mut s, mut n) = (0.0, 0.0);
                for &(y, wy) in row {
                    for &(x, wx) in col {
                        s += src[(y * sw + x) * 4] as f64 * wx * wy;
                        n += wx * wy;
                    }
                }
                let v = (s / n).round() as u8;
                out.extend_from_slice(&[v, v, v, 255]);
            }
        }
        out
    }

    #[test]
    fn a_1024_px_preview_matches_a_6000_px_export() {
        // #51: the CPU preview is 1024 px, exactly two cells per pixel. With
        // cell edges on its pixel edges it averaged whole cells, crisper than
        // the 6000 px export area-averaged to it (0.91 strength, 0.85
        // texture); the quarter-cell lattice phase shares cells between
        // neighbours as the downscale does.
        let (w, h, ew, eh) = (1024usize, 60usize, 6000usize, 352usize);
        for (amount, gaussian, mono) in [(50.0f32, true, true), (40.0, false, false)] {
            let mut preview = solid(w, h, 128);
            noise_add(&mut preview, w, h, amount, gaussian, mono, 4, Place::whole(w, w * 2 / 3));
            let mut export = solid(ew, eh, 128);
            noise_add(&mut export, ew, eh, amount, gaussian, mono, 4, Place::whole(ew, ew * 2 / 3));
            let down = area_down(&export, ew, eh, w, h);
            let sd = luma_sd(&down) / luma_sd(&preview);
            let detail = luma_detail(&down, w, h) / luma_detail(&preview, w, h);
            assert!((0.93..=1.07).contains(&sd), "amount {amount}: strength ratio {sd:.3}");
            assert!((0.93..=1.07).contains(&detail), "amount {amount}: texture ratio {detail:.3}");
        }
    }

    #[test]
    fn grain_strength_is_even_across_the_frame() {
        // A 2000 px render drifts slowly against the 2048-cell lattice. A
        // plain area sample would grow and fade the grain in bands about 40 px
        // apart; the per-pixel normalisation keeps every block alike.
        let (w, h) = (2000usize, 320usize);
        let mut img = solid(w, h, 128);
        noise_add(&mut img, w, h, 50.0, true, true, 3, Place::whole(w, w * 2 / 3));
        let b = 16;
        let sds: Vec<f64> = (0..h / b)
            .flat_map(|by| (0..w / b).map(move |bx| (bx, by)))
            .map(|(bx, by)| {
                let v: Vec<f64> = (0..b * b).map(|i| img[((by * b + i / b) * w + bx * b + i % b) * 4] as f64).collect();
                let m = v.iter().sum::<f64>() / v.len() as f64;
                (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
            })
            .collect();
        let mean = sds.iter().sum::<f64>() / sds.len() as f64;
        let spread = (sds.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / sds.len() as f64).sqrt() / mean;
        assert!(spread < 0.07, "block strength spread {spread:.3}");
    }
}
