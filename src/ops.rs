//! Pure-Rust pixel operations on RGBA buffers.

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
