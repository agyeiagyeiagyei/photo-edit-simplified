//! Separable RGBA8 resampling for the render pipeline.
//!
//! Downscales of 2× or more average the exact source area, so fine texture
//! (feathers, 1-px stripes) cannot alias into moiré; smaller scales use a
//! Lanczos3 kernel normalised per destination pixel. Canvas `drawImage` is
//! only bilinear and drops that detail, so proxies, previews and worker
//! renders all use this instead.

/// Destination rows per strip in the two-pass loop. Bounds the intermediate
/// buffer to the source rows one band needs, not the whole frame.
const STRIP_ROWS: usize = 32;

struct Axis {
    starts: Vec<usize>,
    lens: Vec<u16>,
    offs: Vec<usize>,
    weights: Vec<f32>,
}

impl Axis {
    fn new(src: usize, dst: usize) -> Self {
        assert!(src > 0 && dst > 0, "resample: zero-sized axis");
        let f = src as f64 / dst as f64;
        let mut ax = Axis {
            starts: vec![0; dst],
            lens: vec![0; dst],
            offs: vec![0; dst],
            weights: Vec::with_capacity(src + dst),
        };
        for i in 0..dst {
            let (start, ws) = if f >= 2.0 {
                area_weights(src, dst, i)
            } else {
                lanczos_weights(src, dst, i)
            };
            ax.starts[i] = start;
            ax.lens[i] = ws.len() as u16;
            ax.offs[i] = ax.weights.len();
            ax.weights.extend_from_slice(&ws);
        }
        ax
    }
}

/// Box filter: weight of source pixel `j` is its overlap with the destination
/// pixel's source interval, normalised by the interval length.
fn area_weights(src: usize, dst: usize, i: usize) -> (usize, Vec<f32>) {
    let scale = src as f64 / dst as f64;
    let a = i as f64 * scale;
    let b = (i + 1) as f64 * scale;
    let j0 = a.floor() as usize;
    let j1 = (b.ceil() as usize).min(src);
    let span = b - a;
    let mut ws = Vec::with_capacity(j1 - j0);
    for j in j0..j1 {
        let lo = (j as f64).max(a);
        let hi = ((j + 1) as f64).min(b);
        ws.push(((hi - lo) / span) as f32);
    }
    (j0, ws)
}

/// Lanczos3, widened by the downscale factor so the kernel still integrates
/// the source area.
fn lanczos_weights(src: usize, dst: usize, i: usize) -> (usize, Vec<f32>) {
    let f = src as f64 / dst as f64;
    let stretch = f.max(1.0);
    let support = 3.0 * stretch;
    let center = (i as f64 + 0.5) * f - 0.5;
    let lo = ((center - support).ceil() as isize).max(0) as usize;
    let hi = ((center + support).floor() as isize).min(src as isize - 1);
    if hi < lo as isize {
        let j = center.round().clamp(0.0, src as f64 - 1.0) as usize;
        return (j, vec![1.0]);
    }
    let hi = hi as usize;
    let mut ws = Vec::with_capacity(hi - lo + 1);
    let mut sum = 0.0;
    for j in lo..=hi {
        let w = lanczos3((j as f64 - center) / stretch);
        ws.push(w as f32);
        sum += w;
    }
    if sum.abs() < 1e-6 {
        let j = center.round().clamp(0.0, src as f64 - 1.0) as usize;
        return (j, vec![1.0]);
    }
    let inv = (1.0 / sum) as f32;
    for w in &mut ws {
        *w *= inv;
    }
    (lo, ws)
}

fn lanczos3(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let ax = x.abs();
    if ax >= 3.0 {
        return 0.0;
    }
    let px = std::f64::consts::PI * x;
    let s = (px.sin() / px) * ((px / 3.0).sin() / (px / 3.0));
    s
}

/// Resample RGBA8 pixels from `sw`×`sh` to `dw`×`dh`.
pub fn resample(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    assert_eq!(src.len(), sw * sh * 4, "resample: source size mismatch");
    assert!(dw > 0 && dh > 0, "resample: zero-sized target");
    if (sw, sh) == (dw, dh) {
        return src.to_vec();
    }
    let ax = Axis::new(sw, dw);
    let ay = Axis::new(sh, dh);
    let mut out = vec![0u8; dw * dh * 4];
    let strip = STRIP_ROWS.min(dh);
    let mut tmp: Vec<f32> = Vec::new();
    let mut y0 = 0;
    while y0 < dh {
        let y1 = (y0 + strip).min(dh);
        let sy0 = ay.starts[y0];
        let sy1 = ay.starts[y1 - 1] + ay.lens[y1 - 1] as usize;
        let rows = sy1 - sy0;
        tmp.clear();
        tmp.resize(dw * rows * 4, 0.0);
        for sy in sy0..sy1 {
            let src_row = &src[sy * sw * 4..(sy + 1) * sw * 4];
            let dst_row = &mut tmp[(sy - sy0) * dw * 4..(sy - sy0 + 1) * dw * 4];
            for dx in 0..dw {
                let start = ax.starts[dx];
                let len = ax.lens[dx] as usize;
                let wgt = &ax.weights[ax.offs[dx]..ax.offs[dx] + len];
                let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                for (k, &w) in wgt.iter().enumerate() {
                    let p = (start + k) * 4;
                    r += src_row[p] as f32 * w;
                    g += src_row[p + 1] as f32 * w;
                    b += src_row[p + 2] as f32 * w;
                    a += src_row[p + 3] as f32 * w;
                }
                let o = dx * 4;
                dst_row[o] = r;
                dst_row[o + 1] = g;
                dst_row[o + 2] = b;
                dst_row[o + 3] = a;
            }
        }
        for dy in y0..y1 {
            let start = ay.starts[dy] - sy0;
            let len = ay.lens[dy] as usize;
            let wgt = &ay.weights[ay.offs[dy]..ay.offs[dy] + len];
            let dst_row = &mut out[dy * dw * 4..(dy + 1) * dw * 4];
            for dx in 0..dw {
                let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                for (k, &w) in wgt.iter().enumerate() {
                    let p = ((start + k) * dw + dx) * 4;
                    r += tmp[p] * w;
                    g += tmp[p + 1] * w;
                    b += tmp[p + 2] * w;
                    a += tmp[p + 3] * w;
                }
                let o = dx * 4;
                dst_row[o] = (r + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[o + 1] = (g + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[o + 2] = (b + 0.5).clamp(0.0, 255.0) as u8;
                dst_row[o + 3] = (a + 0.5).clamp(0.0, 255.0) as u8;
            }
        }
        y0 = y1;
    }
    out
}

/// Downscale so the long edge fits `long_edge`; a no-op (copy) when it
/// already does. Used for display proxies and destructive-op previews.
pub fn fit_long_edge(src: &[u8], w: usize, h: usize, long_edge: u32) -> (Vec<u8>, usize, usize) {
    let le = w.max(h) as u32;
    if le <= long_edge {
        return (src.to_vec(), w, h);
    }
    let scale = long_edge as f64 / le as f64;
    let tw = ((w as f64 * scale).round() as usize).max(1);
    let th = ((h as f64 * scale).round() as usize).max(1);
    (resample(src, w, h, tw, th), tw, th)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, v: [u8; 4]) -> Vec<u8> {
        v.iter().copied().cycle().take(w * h * 4).collect()
    }

    #[test]
    fn identity_returns_a_copy() {
        let px = solid(7, 5, [10, 20, 30, 255]);
        assert_eq!(resample(&px, 7, 5, 7, 5), px);
    }

    #[test]
    fn area_average_kills_one_pixel_stripes() {
        // Alternating 0/255 columns downscaled 4× must average to one flat
        // tone: any residual column-to-column swing is aliasing.
        let (w, h) = (400usize, 4usize);
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = if x % 2 == 0 { 0 } else { 255 };
                let i = (y * w + x) * 4;
                px[i] = v;
                px[i + 1] = v;
                px[i + 2] = v;
                px[i + 3] = 255;
            }
        }
        let out = resample(&px, w, h, 100, 1);
        let values: Vec<u8> = (0..100).map(|x| out[x * 4]).collect();
        let min = *values.iter().min().unwrap();
        let max = *values.iter().max().unwrap();
        assert!(
            max - min <= 1,
            "stripes aliased after 4× box downscale: {min}..={max}"
        );
        assert!((127..=128).contains(&values[0]), "mean {} not 50% grey", values[0]);
    }

    #[test]
    fn uniform_tone_survives_fractional_downscale() {
        // 1.25× is below the area-filter threshold, so this exercises the
        // Lanczos weight normalisation at the frame edges.
        let px = solid(100, 80, [200, 100, 50, 255]);
        let out = resample(&px, 100, 80, 80, 64);
        for i in 0..out.len() / 4 {
            assert!((out[i * 4] as i16 - 200).abs() <= 1);
            assert!((out[i * 4 + 1] as i16 - 100).abs() <= 1);
            assert!((out[i * 4 + 2] as i16 - 50).abs() <= 1);
            assert_eq!(out[i * 4 + 3], 255);
        }
    }

    #[test]
    fn gradient_stays_monotonic() {
        let (w, h) = (64usize, 8usize);
        let mut px = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = (x * 255 / (w - 1)) as u8;
                let i = (y * w + x) * 4;
                px[i] = v;
                px[i + 1] = v;
                px[i + 2] = v;
            }
        }
        let out = resample(&px, w, h, 16, 8);
        for x in 1..16 {
            assert!(out[x * 4] >= out[(x - 1) * 4], "gradient reversed at {x}");
        }
        // 4× box cells average four gradient steps, so the ends sit a few
        // levels in from the source extremes.
        assert!(out[0] <= 8);
        assert!(out[15 * 4] >= 247);
    }

    #[test]
    fn fit_long_edge_never_upscales() {
        let px = solid(40, 20, [1, 2, 3, 255]);
        let (out, w, h) = fit_long_edge(&px, 40, 20, 1024);
        assert_eq!((w, h), (40, 20));
        assert_eq!(out, px);

        let (out, w, h) = fit_long_edge(&px, 40, 20, 10);
        assert_eq!(w, 10);
        assert_eq!(h, 5);
        assert_eq!(out.len(), 10 * 5 * 4);
    }
}
