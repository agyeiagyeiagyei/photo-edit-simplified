//! Pure-Rust pixel operations on RGBA buffers.

use crate::state::{Selection, SelectionKind};

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
            for y in 0..h {
                for x in 0..w {
                    let nx = (x as f32 + 0.5) / w as f32;
                    let ny = (y as f32 + 0.5) / h as f32;
                    if point_in_polygon(nx, ny, poly) {
                        mask[y * w + x] = 255;
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

fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if ((yi > y) != (yj > y))
            && (x < (xj - xi) * (y - yi) / (yj - yi).max(f32::EPSILON) + xi)
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Approximate gaussian feather on a grayscale mask. radius is a fraction of
/// the image diagonal and is clamped to a reasonable pixel range.
pub fn feather_mask(mask: &mut [u8], w: usize, h: usize, radius: f32) {
    let diag = ((w * w + h * h) as f32).sqrt();
    let r = (radius * diag * 0.5).max(1.0).min(64.0) as usize;
    if r <= 1 {
        return;
    }
    let mut tmp = mask.to_vec();
    box_blur_gray(&mut tmp, mask, w, h, r);
}

fn box_blur_gray(src: &[u8], dst: &mut [u8], w: usize, h: usize, radius: usize) {
    let mut temp = vec![0u32; w * h];
    let r = radius as i32;
    // horizontal pass
    for y in 0..h {
        let mut sum = 0u32;
        for x in 0..(r + 1).min(w as i32) {
            sum += src[y * w + x as usize] as u32;
        }
        for x in 0..w {
            let left = (x as i32 - r - 1).max(0) as usize;
            let right = (x as i32 + r).min(w as i32 - 1) as usize;
            if x as i32 - r - 1 >= 0 {
                sum -= src[y * w + left] as u32;
            }
            if x as i32 + r < w as i32 {
                sum += src[y * w + right] as u32;
            }
            let count = (right - left + 1) as u32;
            temp[y * w + x] = sum / count.max(1);
        }
    }
    // vertical pass
    for x in 0..w {
        let mut sum = 0u32;
        for y in 0..(r + 1).min(h as i32) {
            sum += temp[(y as usize) * w + x];
        }
        for y in 0..h {
            let top = (y as i32 - r - 1).max(0) as usize;
            let bot = (y as i32 + r).min(h as i32 - 1) as usize;
            if y as i32 - r - 1 >= 0 {
                sum -= temp[top * w + x];
            }
            if y as i32 + r < h as i32 {
                sum += temp[bot * w + x];
            }
            let count = (bot - top + 1) as u32;
            dst[y * w + x] = (sum / count.max(1)).min(255) as u8;
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
            if x as i32 - r - 1 >= 0 {
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
            if y as i32 - r - 1 >= 0 {
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
}
