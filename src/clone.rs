//! Clone stamp, reimplemented from Compositor's CloneStamp.swift semantics
//! (MIT, (c) Robbie Tilton) — the Mac original stamps through Metal, so this
//! is a pure-pixel equivalent: soft-disc coverage along a stroke, copying from
//! a fixed whole-pixel offset (aligned strokes keep the first stroke's offset,
//! per the Swift `cloneStrokeOffset`). Source samples clamp at the image edge.

/// Stamp through `coverage` (255 = full copy), blending each covered pixel
/// toward the pixel at `(x + dx, y + dy)`. Offset is in whole pixels.
pub fn clone_stamp(rgba: &mut [u8], width: usize, height: usize, coverage: &[u8], dx: i32, dy: i32) {
    let src = rgba.to_vec();
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let a = coverage[i] as u32;
            if a == 0 {
                continue;
            }
            let sx = (x as i32 + dx).clamp(0, width as i32 - 1) as usize;
            let sy = (y as i32 + dy).clamp(0, height as i32 - 1) as usize;
            let s = &src[(sy * width + sx) * 4..];
            let p = &mut rgba[i * 4..];
            for c in 0..4 {
                p[c] = ((p[c] as u32 * (255 - a) + s[c] as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> Vec<u8> {
        let mut d = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                d.extend_from_slice(&[x as u8, y as u8, 100, 255]);
            }
        }
        d
    }

    fn disc(w: usize, h: usize, cx: usize, cy: usize, r: usize) -> Vec<u8> {
        let mut cov = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let d2 = (x as i64 - cx as i64).pow(2) + (y as i64 - cy as i64).pow(2);
                if (d2 as f64) <= (r as f64).powi(2) {
                    cov[y * w + x] = 255;
                }
            }
        }
        cov
    }

    #[test]
    fn copies_from_offset() {
        let mut img = gradient(32, 32);
        let cov = disc(32, 32, 20, 20, 3);
        clone_stamp(&mut img, 32, 32, &cov, -10, -10);
        // Center of the disc copies (10,10) = [10,10,100,255].
        let p = &img[(20 * 32 + 20) * 4..];
        assert_eq!(&p[..4], &[10, 10, 100, 255]);
        // Outside the disc is untouched.
        let q = &img[(5 * 32 + 20) * 4..];
        assert_eq!(&q[..4], &[20, 5, 100, 255]);
    }

    #[test]
    fn zero_coverage_is_noop() {
        let mut img = gradient(16, 16);
        let orig = img.clone();
        clone_stamp(&mut img, 16, 16, &vec![0u8; 256], 5, 5);
        assert_eq!(img, orig);
    }

    #[test]
    fn partial_coverage_blends() {
        let mut img = gradient(16, 16);
        let mut cov = vec![0u8; 256];
        cov[8 * 16 + 8] = 128;
        clone_stamp(&mut img, 16, 16, &cov, 2, 0);
        let p = &img[(8 * 16 + 8) * 4..];
        let want = ((8u32 * 127 + 10u32 * 128 + 127) / 255) as u8;
        assert_eq!(p[0], want);
    }

    #[test]
    fn clamps_source_at_edges() {
        let mut img = gradient(16, 16);
        let cov = disc(16, 16, 2, 2, 2);
        clone_stamp(&mut img, 16, 16, &cov, -50, -50);
        // Every covered pixel clamps to source (0,0) = [0,0,100,255].
        let p = &img[(2 * 16 + 2) * 4..];
        assert_eq!(&p[..4], &[0, 0, 100, 255]);
    }

    #[test]
    fn overlapping_stamps_read_original() {
        // A stamp whose source area overlaps its own coverage must copy the
        // pre-stroke pixels, not progressively cloned ones.
        let mut img = gradient(32, 32);
        let cov = disc(32, 32, 16, 16, 8);
        clone_stamp(&mut img, 32, 32, &cov, 2, 0);
        // (18,16) is inside the disc AND is the source for (16,16): both must
        // read the original gradient, so (16,16) = original (18,16) = [18,16,..].
        let p = &img[(16 * 32 + 16) * 4..];
        assert_eq!(&p[..4], &[18, 16, 100, 255]);
    }
}
