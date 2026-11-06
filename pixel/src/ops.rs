//! Pure-Rust pixel operations on RGBA buffers.

use crate::types::{Selection, SelectionKind};

/// Apply brightness / contrast / saturation / warmth in place.
/// All parameters in [-1.0, 1.0].
pub fn adjust(data: &mut [u8], brightness: f32, contrast: f32, saturation: f32, warmth: f32) {
    let b = brightness * 255.0;
    let c = contrast;
    let s = 1.0 + saturation;
    let wr = warmth * 40.0;
    let wb = -warmth * 40.0;
    for px in data.chunks_exact_mut(4) {
        let r = px[0] as f32;
        let g = px[1] as f32;
        let bl = px[2] as f32;

        // contrast around mid-gray, then brightness
        let mut r = (r - 128.0) * (1.0 + c) + 128.0 + b;
        let mut g = (g - 128.0) * (1.0 + c) + 128.0 + b;
        let mut bl = (bl - 128.0) * (1.0 + c) + 128.0 + b;

        // saturation via luma
        let luma = 0.299 * r + 0.587 * g + 0.114 * bl;
        r = luma + (r - luma) * s;
        g = luma + (g - luma) * s;
        bl = luma + (bl - luma) * s;

        // warmth
        r += wr;
        bl += wb;

        px[0] = r.clamp(0.0, 255.0) as u8;
        px[1] = g.clamp(0.0, 255.0) as u8;
        px[2] = bl.clamp(0.0, 255.0) as u8;
    }
}

/// Rotate by an arbitrary angle (degrees, clockwise) with bilinear sampling,
/// then return the largest same-aspect rectangle fully inside the rotated
/// image (no empty corners). Returns (pixels, width, height).
pub fn rotate_auto_crop(src: &[u8], w: usize, h: usize, angle_deg: f32) -> (Vec<u8>, usize, usize) {
    let a = angle_deg.to_radians();
    let (sin, cos) = (a.sin().abs(), a.cos().abs());

    // inscribed axis-aligned rect with same aspect as source
    let bw = w as f32 * cos + h as f32 * sin; // rotated bounding box
    let bh = w as f32 * sin + h as f32 * cos;
    let x1 = (w * w) as f32 / (2.0 * bw);
    let x2 = (w * h) as f32 / (2.0 * bh);
    let half_w = x1.min(x2);
    let half_h = half_w * h as f32 / w as f32;
    let out_w = (half_w * 2.0).floor().max(1.0) as usize;
    let out_h = (half_h * 2.0).floor().max(1.0) as usize;

    let a = -angle_deg.to_radians(); // inverse rotation for sampling
    let (sn, cs) = a.sin_cos();
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;

    let mut out = vec![0u8; out_w * out_h * 4];
    for oy in 0..out_h {
        for ox in 0..out_w {
            // position relative to center, mapped back into source space
            let dx = ox as f32 + 0.5 - out_w as f32 / 2.0;
            let dy = oy as f32 + 0.5 - out_h as f32 / 2.0;
            let sx = cx + dx * cs - dy * sn;
            let sy = cy + dx * sn + dy * cs;
            let di = (oy * out_w + ox) * 4;
            if sx >= 0.0 && sy >= 0.0 && sx < (w - 1) as f32 && sy < (h - 1) as f32 {
                let (x0, y0) = (sx.floor() as usize, sy.floor() as usize);
                let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
                for ch in 0..4 {
                    let i00 = src[(y0 * w + x0) * 4 + ch] as f32;
                    let i10 = src[(y0 * w + x0 + 1) * 4 + ch] as f32;
                    let i01 = src[((y0 + 1) * w + x0) * 4 + ch] as f32;
                    let i11 = src[((y0 + 1) * w + x0 + 1) * 4 + ch] as f32;
                    let top = i00 + (i10 - i00) * fx;
                    let bot = i01 + (i11 - i01) * fx;
                    out[di + ch] = (top + (bot - top) * fy) as u8;
                }
            }
        }
    }
    (out, out_w, out_h)
}

/// Rotate 90/180/270 degrees clockwise. (w, h) swap as needed.
pub fn rotate_90(src: &[u8], w: usize, h: usize, turns: u8) -> (Vec<u8>, usize, usize) {
    match turns % 4 {
        0 => (src.to_vec(), w, h),
        1 => {
            let (ow, oh) = (h, w);
            let mut out = vec![0u8; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let (nx, ny) = (h - 1 - y, x);
                    out[(ny * ow + nx) * 4..(ny * ow + nx) * 4 + 4]
                        .copy_from_slice(&src[(y * w + x) * 4..(y * w + x) * 4 + 4]);
                }
            }
            (out, ow, oh)
        }
        2 => {
            let mut out = vec![0u8; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let (nx, ny) = (w - 1 - x, h - 1 - y);
                    out[(ny * w + nx) * 4..(ny * w + nx) * 4 + 4]
                        .copy_from_slice(&src[(y * w + x) * 4..(y * w + x) * 4 + 4]);
                }
            }
            (out, w, h)
        }
        _ => {
            let (ow, oh) = (h, w);
            let mut out = vec![0u8; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let (nx, ny) = (y, w - 1 - x);
                    out[(ny * ow + nx) * 4..(ny * ow + nx) * 4 + 4]
                        .copy_from_slice(&src[(y * w + x) * 4..(y * w + x) * 4 + 4]);
                }
            }
            (out, ow, oh)
        }
    }
}

/// Crop a rectangle (clamped to bounds).
pub fn crop(src: &[u8], w: usize, x: usize, y: usize, cw: usize, ch: usize) -> Vec<u8> {
    let mut out = vec![0u8; cw * ch * 4];
    for row in 0..ch {
        let s = ((y + row) * w + x) * 4;
        out[row * cw * 4..(row + 1) * cw * 4].copy_from_slice(&src[s..s + cw * 4]);
    }
    out
}

/// 16-bit counterpart of [`rotate_90`], for the neutral RAW frame.
pub fn rotate_90_wide(src: &[u16], w: usize, h: usize, turns: u8) -> (Vec<u16>, usize, usize) {
    match turns % 4 {
        0 => (src.to_vec(), w, h),
        1 => {
            let (ow, oh) = (h, w);
            let mut out = vec![0u16; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let (nx, ny) = (h - 1 - y, x);
                    out[(ny * ow + nx) * 4..(ny * ow + nx) * 4 + 4]
                        .copy_from_slice(&src[(y * w + x) * 4..(y * w + x) * 4 + 4]);
                }
            }
            (out, ow, oh)
        }
        2 => {
            let mut out = vec![0u16; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let (nx, ny) = (w - 1 - x, h - 1 - y);
                    out[(ny * w + nx) * 4..(ny * w + nx) * 4 + 4]
                        .copy_from_slice(&src[(y * w + x) * 4..(y * w + x) * 4 + 4]);
                }
            }
            (out, w, h)
        }
        _ => {
            let (ow, oh) = (h, w);
            let mut out = vec![0u16; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let (nx, ny) = (y, w - 1 - x);
                    out[(ny * ow + nx) * 4..(ny * ow + nx) * 4 + 4]
                        .copy_from_slice(&src[(y * w + x) * 4..(y * w + x) * 4 + 4]);
                }
            }
            (out, ow, oh)
        }
    }
}

/// 16-bit counterpart of [`rotate_auto_crop`], sampling with rounding.
pub fn rotate_auto_crop_wide(
    src: &[u16],
    w: usize,
    h: usize,
    angle_deg: f32,
) -> (Vec<u16>, usize, usize) {
    let a = angle_deg.to_radians();
    let (sin, cos) = (a.sin().abs(), a.cos().abs());
    let bw = w as f32 * cos + h as f32 * sin;
    let bh = w as f32 * sin + h as f32 * cos;
    let x1 = (w * w) as f32 / (2.0 * bw);
    let x2 = (w * h) as f32 / (2.0 * bh);
    let half_w = x1.min(x2);
    let half_h = half_w * h as f32 / w as f32;
    let out_w = (half_w * 2.0).floor().max(1.0) as usize;
    let out_h = (half_h * 2.0).floor().max(1.0) as usize;

    let a = -angle_deg.to_radians();
    let (sn, cs) = a.sin_cos();
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;

    let mut out = vec![0u16; out_w * out_h * 4];
    for oy in 0..out_h {
        for ox in 0..out_w {
            let dx = ox as f32 + 0.5 - out_w as f32 / 2.0;
            let dy = oy as f32 + 0.5 - out_h as f32 / 2.0;
            let sx = cx + dx * cs - dy * sn;
            let sy = cy + dx * sn + dy * cs;
            let di = (oy * out_w + ox) * 4;
            if sx >= 0.0 && sy >= 0.0 && sx < (w - 1) as f32 && sy < (h - 1) as f32 {
                let (x0, y0) = (sx.floor() as usize, sy.floor() as usize);
                let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
                for ch in 0..4 {
                    let i00 = src[(y0 * w + x0) * 4 + ch] as f32;
                    let i10 = src[(y0 * w + x0 + 1) * 4 + ch] as f32;
                    let i01 = src[((y0 + 1) * w + x0) * 4 + ch] as f32;
                    let i11 = src[((y0 + 1) * w + x0 + 1) * 4 + ch] as f32;
                    let top = i00 + (i10 - i00) * fx;
                    let bot = i01 + (i11 - i01) * fx;
                    out[di + ch] = (top + (bot - top) * fy + 0.5).clamp(0.0, 65535.0) as u16;
                }
            }
        }
    }
    (out, out_w, out_h)
}

// --- selection / mask ops -----------------------------------------------------

/// Generate an 8-bit alpha mask from a normalized selection.
/// Result has length w*h; 255 = fully selected, 0 = unselected.
pub fn selection_mask(sel: &Selection, w: usize, h: usize) -> Vec<u8> {
    let mut mask = vec![0u8; w * h];
    match &sel.kind {
        SelectionKind::Rect { x, y, w: rw, h: rh } => {
            let x0 = (x * w as f32).floor().max(0.0) as usize;
            let y0 = (y * h as f32).floor().max(0.0) as usize;
            let x1 = ((x + rw) * w as f32).ceil().min(w as f32) as usize;
            let y1 = ((y + rh) * h as f32).ceil().min(h as f32) as usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    mask[y * w + x] = 255;
                }
            }
        }
        SelectionKind::Lasso(poly) => {
            if poly.len() < 3 {
                return mask;
            }
            lasso_fill(&mut mask, w, h, poly);
        }
        SelectionKind::Mask { data, width: mw, height: mh } => {
            if *mw == w && *mh == h && data.len() == w * h {
                mask.copy_from_slice(data);
            } else if *mw > 0 && *mh > 0 && data.len() == mw * mh {
                if *mw <= w && *mh <= h {
                    resample_bilinear(data, *mw, *mh, &mut mask, w, h);
                } else {
                    for y in 0..h {
                        let sy = (y * *mh / h).min(mh - 1);
                        for x in 0..w {
                            let sx = (x * *mw / w).min(mw - 1);
                            mask[y * w + x] = data[sy * mw + sx];
                        }
                    }
                }
            }
        }
    }
    if sel.feather > 0.001 {
        feather_mask(&mut mask, w, h, sel.feather);
    }
    mask
}

/// Bilinear upsample of a raster mask, with pixel centres aligned and edge
/// samples clamped. A wand mask made on the 1024 px preview would otherwise
/// become a ~6 px block per cell at export size; this turns each step into a
/// one-cell ramp without changing the stored mask.
fn resample_bilinear(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], w: usize, h: usize) {
    let axis = |n: usize, sn: usize| -> Vec<(usize, f32)> {
        (0..n)
            .map(|i| {
                if sn <= 1 {
                    return (0, 0.0);
                }
                let s = ((i as f32 + 0.5) * sn as f32 / n as f32 - 0.5)
                    .clamp(0.0, (sn - 1) as f32);
                let s0 = (s.floor() as usize).min(sn - 2);
                (s0, s - s0 as f32)
            })
            .collect()
    };
    let xs = axis(w, sw);
    let ys = axis(h, sh);
    for (y, &(y0, fy)) in ys.iter().enumerate() {
        let y1 = (y0 + 1).min(sh - 1);
        let row0 = &src[y0 * sw..(y0 + 1) * sw];
        let row1 = &src[y1 * sw..(y1 + 1) * sw];
        let out = &mut dst[y * w..(y + 1) * w];
        for (x, &(x0, fx)) in xs.iter().enumerate() {
            let x1 = (x0 + 1).min(sw - 1);
            let top = row0[x0] as f32 * (1.0 - fx) + row0[x1] as f32 * fx;
            let bottom = row1[x0] as f32 * (1.0 - fx) + row1[x1] as f32 * fx;
            out[x] = (top * (1.0 - fy) + bottom * fy + 0.5) as u8;
        }
    }
}

/// Even-odd scanline fill of a normalized polygon, sampled at pixel centres.
/// Per row it collects the x of every edge crossing (the same straddle test
/// and intersection expression as [`point_in_polygon`]), sorts them, and marks
/// a pixel inside when an odd number of crossings lie strictly to its right —
/// so it selects exactly the pixels the per-pixel test does, in O(rows × edges
/// + pixels) instead of O(pixels × edges).
fn lasso_fill(mask: &mut [u8], w: usize, h: usize, poly: &[(f32, f32)]) {
    let n = poly.len();
    let mut xs: Vec<f32> = Vec::with_capacity(n);
    for y in 0..h {
        let ny = (y as f32 + 0.5) / h as f32;
        xs.clear();
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = poly[i];
            let (xj, yj) = poly[j];
            if (yi > ny) != (yj > ny) {
                xs.push((xj - xi) * (ny - yi) / (yj - yi) + xi);
            }
            j = i;
        }
        if xs.is_empty() {
            continue;
        }
        xs.sort_unstable_by(f32::total_cmp);
        let row = &mut mask[y * w..(y + 1) * w];
        // `k` counts crossings at or left of the pixel centre; the pixel is
        // inside when the rest (those strictly right of it) are odd in number.
        let mut k = 0;
        for (x, px) in row.iter_mut().enumerate() {
            let nx = (x as f32 + 0.5) / w as f32;
            while k < xs.len() && xs[k] <= nx {
                k += 1;
            }
            if (xs.len() - k) % 2 == 1 {
                *px = 255;
            }
        }
    }
}

/// Reference even-odd test the scanline fill must agree with pixel for pixel.
#[cfg(test)]
fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        // The straddle test above guarantees yi != yj, so no zero divisor; the
        // sign of yj - yi must be preserved (edges running upward and downward
        // both count as crossings).
        if ((yi > y) != (yj > y))
            && (x < (xj - xi) * (y - yi) / (yj - yi) + xi)
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Box feather on a grayscale mask. `radius` is a fraction of the image
/// diagonal, so one selection feathers the same at every resolution — the
/// preview, a windowed render and the full-size export see the same edge.
/// (A pixel cap here made the export's edge a fraction of the preview's on
/// large photos.) The running-sum blur costs the same at any radius.
pub fn feather_mask(mask: &mut [u8], w: usize, h: usize, radius: f32) {
    let diag = ((w * w + h * h) as f32).sqrt();
    let r = ((radius * diag * 0.5).max(1.0) as usize).min(w.max(h));
    if r <= 1 {
        return;
    }
    box_blur_gray(mask, w, h, r);
}

/// In-place mean over the window [i - radius, i + radius] (clipped to the
/// frame) along x, then along y. The horizontal pass reads the whole mask into
/// a u16 buffer with 8 fractional bits before the vertical pass writes it back,
/// so no copy of the input is needed and the two passes round once; a uniform
/// region stays exactly at its value.
fn box_blur_gray(mask: &mut [u8], w: usize, h: usize, radius: usize) {
    let mut temp = vec![0u16; w * h];
    for y in 0..h {
        let row = &mask[y * w..(y + 1) * w];
        let out = &mut temp[y * w..(y + 1) * w];
        let mut sum: u32 = row[..(radius + 1).min(w)].iter().map(|&v| v as u32).sum();
        for x in 0..w {
            let count = ((x + radius).min(w - 1) - x.saturating_sub(radius) + 1) as u32;
            out[x] = (((sum << 8) + count / 2) / count) as u16;
            if x + 1 + radius < w {
                sum += row[x + 1 + radius] as u32;
            }
            if x >= radius {
                sum -= row[x - radius] as u32;
            }
        }
    }
    let mut col_sum = vec![0u32; w];
    for y in 0..(radius + 1).min(h) {
        for (s, &v) in col_sum.iter_mut().zip(&temp[y * w..(y + 1) * w]) {
            *s += v as u32;
        }
    }
    for y in 0..h {
        let count = ((y + radius).min(h - 1) - y.saturating_sub(radius) + 1) as u32;
        let div = count << 8;
        for (d, &s) in mask[y * w..(y + 1) * w].iter_mut().zip(&col_sum) {
            *d = ((s + div / 2) / div).min(255) as u8;
        }
        if y + 1 + radius < h {
            let add = (y + 1 + radius) * w;
            for (s, &v) in col_sum.iter_mut().zip(&temp[add..add + w]) {
                *s += v as u32;
            }
        }
        if y >= radius {
            let sub = (y - radius) * w;
            for (s, &v) in col_sum.iter_mut().zip(&temp[sub..sub + w]) {
                *s -= v as u32;
            }
        }
    }
}

/// Apply color adjustments only where mask > 0. mask length must equal w*h.
pub fn adjust_masked(
    data: &mut [u8],
    mask: &[u8],
    brightness: f32,
    contrast: f32,
    saturation: f32,
    warmth: f32,
) {
    let b = brightness * 255.0;
    let c = contrast;
    let s = 1.0 + saturation;
    let wr = warmth * 40.0;
    let wb = -warmth * 40.0;
    for (px, m) in data.chunks_exact_mut(4).zip(mask.iter()) {
        if *m == 0 {
            continue;
        }
        let a = *m as f32 / 255.0;
        let r = px[0] as f32;
        let g = px[1] as f32;
        let bl = px[2] as f32;

        let mut r = (r - 128.0) * (1.0 + c) + 128.0 + b;
        let mut g = (g - 128.0) * (1.0 + c) + 128.0 + b;
        let mut bl = (bl - 128.0) * (1.0 + c) + 128.0 + b;

        let luma = 0.299 * r + 0.587 * g + 0.114 * bl;
        r = luma + (r - luma) * s;
        g = luma + (g - luma) * s;
        bl = luma + (bl - luma) * s;

        r += wr;
        bl += wb;

        px[0] = lerp(px[0] as f32, r.clamp(0.0, 255.0), a) as u8;
        px[1] = lerp(px[1] as f32, g.clamp(0.0, 255.0), a) as u8;
        px[2] = lerp(px[2] as f32, bl.clamp(0.0, 255.0), a) as u8;
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Extract `src` RGBA multiplied by `mask` into a new RGBA buffer.
/// Mask values 0..255 are treated as alpha. The returned buffer has the
/// same dimensions as the input.
pub fn extract_masked(src: &[u8], mask: &[u8]) -> Vec<u8> {
    let mut out = src.to_vec();
    for (px, m) in out.chunks_exact_mut(4).zip(mask.iter()) {
        let a = *m as f32 / 255.0;
        px[3] = (px[3] as f32 * a).min(255.0) as u8;
    }
    out
}

/// Simple box blur on RGBA (used for background blur behind isolated subject).
pub fn box_blur_rgba(data: &mut [u8], w: usize, h: usize, radius: usize) {
    if radius == 0 {
        return;
    }
    let mut temp = vec![0u32; w * h * 4];
    let r = radius as i32;
    // horizontal
    for y in 0..h {
        let mut sums = [0u32; 4];
        for x in 0..(r + 1).min(w as i32) {
            let i = (y * w + x as usize) * 4;
            for c in 0..4 {
                sums[c] += data[i + c] as u32;
            }
        }
        for x in 0..w {
            let left = (x as i32 - r - 1).max(0) as usize;
            let right = (x as i32 + r).min(w as i32 - 1) as usize;
            let li = (y * w + left) * 4;
            let ri = (y * w + right) * 4;
            let oi = (y * w + x) * 4;
            if x as i32 - r > 0 {
                for c in 0..4 {
                    sums[c] -= data[li + c] as u32;
                }
            }
            if x as i32 + r < w as i32 {
                for c in 0..4 {
                    sums[c] += data[ri + c] as u32;
                }
            }
            let count = (right - left + 1) as u32;
            for c in 0..4 {
                temp[oi + c] = sums[c] / count.max(1);
            }
        }
    }
    // vertical
    for x in 0..w {
        let mut sums = [0u32; 4];
        for y in 0..(r + 1).min(h as i32) {
            let i = ((y as usize) * w + x) * 4;
            for c in 0..4 {
                sums[c] += temp[i + c];
            }
        }
        for y in 0..h {
            let top = (y as i32 - r - 1).max(0) as usize;
            let bot = (y as i32 + r).min(h as i32 - 1) as usize;
            let ti = (top * w + x) * 4;
            let bi = (bot * w + x) * 4;
            let oi = (y * w + x) * 4;
            if y as i32 - r > 0 {
                for c in 0..4 {
                    sums[c] -= temp[ti + c];
                }
            }
            if y as i32 + r < h as i32 {
                for c in 0..4 {
                    sums[c] += temp[bi + c];
                }
            }
            let count = (bot - top + 1) as u32;
            for c in 0..4 {
                data[oi + c] = (sums[c] / count.max(1)).min(255) as u8;
            }
        }
    }
}

/// Darken an RGBA buffer in place.
pub fn darken(data: &mut [u8], amount: f32) {
    let factor = 1.0 - amount.clamp(0.0, 1.0);
    for px in data.chunks_exact_mut(4) {
        px[0] = (px[0] as f32 * factor).min(255.0) as u8;
        px[1] = (px[1] as f32 * factor).min(255.0) as u8;
        px[2] = (px[2] as f32 * factor).min(255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(r: u8, g: u8, b: u8) -> [u8; 4] {
        [r, g, b, 255]
    }

    #[test]
    fn adjust_identity_is_noop() {
        let mut data = vec![10, 128, 250, 255, 0, 64, 200, 255];
        let orig = data.clone();
        adjust(&mut data, 0.0, 0.0, 0.0, 0.0);
        assert_eq!(data, orig);
    }

    #[test]
    fn adjust_brightness_shifts_all_channels() {
        let mut data = vec![100, 100, 100, 255];
        adjust(&mut data, 0.2, 0.0, 0.0, 0.0);
        assert!(data[0] > 100 && data[1] > 100 && data[2] > 100);
    }

    #[test]
    fn lasso_mask_handles_slanted_edges() {
        // A diamond: every edge is slanted. Before the divisor fix a downward
        // edge counted as a crossing on every row, so this selected a quarter
        // of the frame and missed its own centre.
        let sel = Selection {
            kind: SelectionKind::Lasso(vec![(0.5, 0.25), (0.75, 0.5), (0.5, 0.75), (0.25, 0.5)]),
            feather: 0.0,
        };
        let (w, h) = (200, 200);
        let m = selection_mask(&sel, w, h);
        let area = m.iter().filter(|&&v| v > 0).count() as f64 / (w * h) as f64;
        assert!((area - 0.125).abs() < 0.005, "diamond covers {area:.3} of the frame, want 0.125");
        assert_eq!(m[100 * w + 100], 255, "centre must be selected");
        assert_eq!(m[60 * w + 60], 0, "bounding-box corner is outside the diamond");
    }

    #[test]
    fn raster_mask_upsamples_with_a_bilinear_ramp() {
        // Nearest-neighbour turns a 2x1 step into four identical blocky cells;
        // the export upsample must instead ramp across one source cell.
        let sel = Selection {
            kind: SelectionKind::Mask { data: vec![0u8, 255].into(), width: 2, height: 1 },
            feather: 0.0,
        };
        let m = selection_mask(&sel, 8, 1);
        assert_eq!(&m[..2], &[0, 0], "left stays fully outside: {m:?}");
        assert_eq!(&m[6..], &[255, 255], "right stays fully inside: {m:?}");
        assert!(m[2] > 0 && m[2] < 128, "ramp starts inside the cell: {m:?}");
        assert!(m[3] > m[2] && m[3] < m[4], "ramp is monotone: {m:?}");
        assert!(m[4] > 128 && m[4] < 255, "ramp ends inside the cell: {m:?}");
        for pair in m.windows(2) {
            assert!(pair[0] <= pair[1], "not monotone: {m:?}");
        }
    }

    #[test]
    fn upscaled_wand_mask_has_no_wide_stair_steps() {
        // 10x mirrors a 1024 px wand mask on a 24 MP export. Nearest-neighbour
        // repeats each source row ten times, so the 50% edge sits at the same x
        // for ten rows; the bilinear ramp must keep every flat run to 2 rows.
        let (sw, sh) = (64usize, 64usize);
        let mut data = vec![0u8; sw * sh];
        for y in 0..sh {
            for x in 0..sw {
                if x + y < 32 {
                    data[y * sw + x] = 255;
                }
            }
        }
        let sel = Selection {
            kind: SelectionKind::Mask { data: data.clone().into(), width: sw, height: sh },
            feather: 0.0,
        };
        let (w, h) = (sw * 10, sh * 10);
        let m = selection_mask(&sel, w, h);
        // Skip the clamped top rows; the diagonal must be measured in the
        // interior, where the source edge actually moves.
        let flat_run = |mask: &[u8]| -> usize {
            let mut prev: Option<usize> = None;
            let mut run = 0usize;
            let mut max_run = 0usize;
            for y in h / 8..h / 2 {
                let Some(edge) = mask[y * w..(y + 1) * w].iter().rposition(|&v| v >= 128) else {
                    prev = None;
                    continue;
                };
                if prev == Some(edge) {
                    run += 1;
                } else {
                    run = 1;
                    prev = Some(edge);
                }
                max_run = max_run.max(run);
            }
            max_run
        };
        let mut nearest = vec![0u8; w * h];
        for y in 0..h {
            let sy = (y * sh / h).min(sh - 1);
            for x in 0..w {
                nearest[y * w + x] = data[sy * sw + (x * sw / w).min(sw - 1)];
            }
        }
        assert_eq!(flat_run(&nearest), 10, "nearest baseline steps once per source row");
        let bilinear = flat_run(&m);
        assert!(bilinear <= 2, "longest flat edge run is {bilinear} rows");
    }

    #[test]
    fn scanline_lasso_matches_the_per_pixel_test() {
        // Random (often self-intersecting) polygons at awkward sizes: the
        // scanline fill must select exactly the pixels point_in_polygon does.
        let mut seed = 0x2545_f491_u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32) / (u32::MAX as f32)
        };
        for case in 0..40 {
            let n = 3 + (case * 7) % 60;
            let poly: Vec<(f32, f32)> = (0..n).map(|_| (rnd() * 1.2 - 0.1, rnd() * 1.2 - 0.1)).collect();
            let (w, h) = (37 + case * 11, 23 + case * 5);
            let mut fast = vec![0u8; w * h];
            lasso_fill(&mut fast, w, h, &poly);
            for y in 0..h {
                for x in 0..w {
                    let nx = (x as f32 + 0.5) / w as f32;
                    let ny = (y as f32 + 0.5) / h as f32;
                    let want = if point_in_polygon(nx, ny, &poly) { 255 } else { 0 };
                    assert_eq!(fast[y * w + x], want, "case {case} ({n} pts, {w}x{h}) pixel ({x},{y})");
                }
            }
        }
    }

    /// 10%..90% rise of the mask along the middle row, as a fraction of width.
    fn ramp_fraction(m: &[u8], w: usize, h: usize) -> f64 {
        let row = &m[(h / 2) * w..(h / 2) * w + w / 2];
        let a = row.iter().position(|&v| v >= 26).unwrap();
        let b = row.iter().position(|&v| v >= 230).unwrap();
        (b - a) as f64 / w as f64
    }

    #[test]
    fn feather_is_resolution_independent() {
        // The same feathered selection at preview and full-size export
        // resolution must give the same edge relative to the frame.
        for feather in [0.01f32, 0.05, 0.15] {
            let sel = Selection {
                kind: SelectionKind::Rect { x: 0.3, y: 0.25, w: 0.4, h: 0.5 },
                feather,
            };
            let small = selection_mask(&sel, 1200, 800);
            let large = selection_mask(&sel, 6000, 4000);
            let (rs, rl) = (ramp_fraction(&small, 1200, 800), ramp_fraction(&large, 6000, 4000));
            assert!(
                (rs - rl).abs() <= 0.002 + 0.03 * rs,
                "feather {feather}: ramp {rs:.4} of width at 1200 px vs {rl:.4} at 6000 px"
            );
        }
    }

    #[test]
    fn feathered_mask_keeps_full_strength_inside() {
        // The blur averages over the clipped window, so the inside of a
        // feathered selection stays at 255 and the far outside at 0.
        let sel = Selection {
            kind: SelectionKind::Rect { x: 0.25, y: 0.25, w: 0.5, h: 0.5 },
            feather: 0.02,
        };
        for (w, h) in [(300usize, 200usize), (1024, 683), (3000, 2000)] {
            let m = selection_mask(&sel, w, h);
            assert_eq!(m[(h / 2) * w + w / 2], 255, "{w}x{h}: centre not at full strength");
            assert_eq!(m[5 * w + 5], 0, "{w}x{h}: far outside not zero");
        }
        let mut flat = vec![255u8; 64 * 48];
        feather_mask(&mut flat, 64, 48, 0.2);
        assert!(flat.iter().all(|&v| v == 255), "a fully selected frame must stay fully selected");
    }

    #[test]
    fn lasso_mask_fills_only_the_polygon() {
        // Both upward and downward edges: clamping the divisor to a positive
        // epsilon used to make every row inside a downward-bounded span read
        // as fully selected.
        let sel = Selection {
            kind: SelectionKind::Lasso(vec![
                (0.25, 0.25),
                (0.75, 0.25),
                (0.75, 0.75),
                (0.25, 0.75),
            ]),
            feather: 0.0,
        };
        let m = selection_mask(&sel, 100, 100);
        assert_eq!(m[50 * 100 + 50], 255, "center must be selected");
        assert_eq!(m[10 * 100 + 10], 0, "top-left corner outside");
        assert_eq!(m[10 * 100 + 50], 0, "above the polygon");
        assert_eq!(m[50 * 100 + 10], 0, "left of the polygon");
        for y in 0..100 {
            let count = m[y * 100..(y + 1) * 100].iter().filter(|&&v| v > 0).count();
            assert!(count <= 51, "row {y} filled {count} px");
        }
    }

    #[test]
    fn rotate_90_four_turns_is_identity() {
        let src: Vec<u8> = (0..2 * 3 * 4).map(|i| i as u8).collect();
        let (out, w, h) = rotate_90(&src, 2, 3, 4);
        assert_eq!((w, h), (2, 3));
        assert_eq!(out, src);
    }

    #[test]
    fn rotate_90_one_turn_swaps_dims_and_maps_corners() {
        // 2x1 image: [red, green] -> 1x2 column, red on top (clockwise)
        let src = [px(255, 0, 0), px(0, 255, 0)].concat();
        let (out, w, h) = rotate_90(&src, 2, 1, 1);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[0..4], &px(255, 0, 0));
        assert_eq!(&out[4..8], &px(0, 255, 0));
    }

    #[test]
    fn rotate_auto_crop_zero_angle_keeps_dims() {
        let src = vec![128u8; 100 * 50 * 4];
        let (_, w, h) = rotate_auto_crop(&src, 100, 50, 0.0);
        assert_eq!((w, h), (100, 50));
    }

    #[test]
    fn rotate_auto_crop_square_45deg_inscribes_half_area() {
        let src = vec![200u8; 100 * 100 * 4];
        let (out, w, h) = rotate_auto_crop(&src, 100, 100, 45.0);
        assert_eq!(w, h);
        // inscribed square side = 100/sqrt(2) ~= 70.7
        assert!((70..=71).contains(&w));
        // center pixel must be sampled from source (not black corner)
        let c = ((h / 2) * w + w / 2) * 4;
        assert_eq!(out[c], 200);
    }

    #[test]
    fn rotate_auto_crop_preserves_aspect() {
        let src = vec![64u8; 200 * 100 * 4];
        let (_, w, h) = rotate_auto_crop(&src, 200, 100, 12.0);
        let aspect = w as f32 / h as f32;
        assert!((aspect - 2.0).abs() < 0.05);
        assert!(w < 200 && h < 100);
    }

    #[test]
    fn crop_extracts_region() {
        let mut src = vec![0u8; 4 * 4 * 4];
        src[(1 * 4 + 2) * 4] = 99; // pixel at (x=2, y=1)
        let out = crop(&src, 4, 2, 1, 1, 1);
        assert_eq!(out[0], 99);
    }

    #[test]
    fn rotate_90_wide_matches_the_u8_rotation() {
        let src: Vec<u8> = (0..4 * 3 * 4).map(|i| (i * 7 % 256) as u8).collect();
        let wide: Vec<u16> = src.iter().map(|&v| v as u16 * 257).collect();
        for turns in 0..4 {
            let (a, aw, ah) = rotate_90(&src, 4, 3, turns);
            let (b, bw, bh) = rotate_90_wide(&wide, 4, 3, turns);
            assert_eq!((aw, ah), (bw, bh));
            for (x, y) in a.iter().zip(b.iter()) {
                assert_eq!(*y, *x as u16 * 257);
            }
        }
    }

    #[test]
    fn rotate_auto_crop_wide_tracks_the_u8_output() {
        let (w, h) = (40usize, 30usize);
        let src: Vec<u8> = (0..w * h * 4).map(|i| ((i * 3) % 200) as u8 + 20).collect();
        let wide: Vec<u16> = src.iter().map(|&v| v as u16 * 257).collect();
        let (a, aw, ah) = rotate_auto_crop(&src, w, h, 12.0);
        let (b, bw, bh) = rotate_auto_crop_wide(&wide, w, h, 12.0);
        assert_eq!((aw, ah), (bw, bh));
        for (x, y) in a.iter().zip(b.iter()) {
            assert!((*y as i32 - *x as i32 * 257).abs() <= 257, "{y} vs {x}");
        }
    }
}
