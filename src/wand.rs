//! Magic wand selection, ported from Compositor (MIT, (c) Robbie Tilton):
//! Compositor/Rendering/WandPixels.c — wand_mask() only. Our selection model
//! is a raster mask, so the outline tracer (wand_trace) has no counterpart
//! here. The scanline flood fill and reference-color sampling are identical
//! to the C, including alpha participating in the match.

/// Flood-select pixels similar to the seed pixel.
///
/// `radius` averages the reference color over the (2r+1)^2 box around the
/// seed (0 = point sample). `tolerance` is the max per-channel difference
/// (0..=255). `contiguous` restricts the result to pixels connected to the
/// seed; otherwise every matching pixel in the image is selected.
///
/// Returns (mask, selected_count); mask is width*height bytes, 255 = selected.
pub fn wand_mask(
    rgba: &[u8],
    width: usize,
    height: usize,
    seed_x: usize,
    seed_y: usize,
    radius: usize,
    tolerance: i32,
    contiguous: bool,
) -> (Vec<u8>, usize) {
    let mut mask = vec![0u8; width * height];
    if width == 0 || height == 0 || seed_x >= width || seed_y >= height {
        return (mask, 0);
    }
    let stride = width * 4;
    let x0 = seed_x.saturating_sub(radius);
    let x1 = (seed_x + radius).min(width - 1);
    let y0 = seed_y.saturating_sub(radius);
    let y1 = (seed_y + radius).min(height - 1);
    let mut sums = [0u64; 4];
    let mut samples = 0u64;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let base = y * stride + x * 4;
            for c in 0..4 {
                sums[c] += rgba[base + c] as u64;
            }
            samples += 1;
        }
    }
    let mut reference = [0i32; 4];
    for c in 0..4 {
        reference[c] = ((sums[c] + samples / 2) / samples) as i32;
    }

    let matches = |px: &[u8]| -> bool {
        for c in 0..4 {
            let d = px[c] as i32 - reference[c];
            if d < -tolerance || d > tolerance {
                return false;
            }
        }
        true
    };

    if !contiguous {
        let mut count = 0;
        for y in 0..height {
            for x in 0..width {
                if matches(&rgba[y * stride + x * 4..]) {
                    mask[y * width + x] = 255;
                    count += 1;
                }
            }
        }
        return (mask, count);
    }

    // Scanline flood fill: each popped seed fills its whole horizontal run,
    // then pushes one seed per matching run in the rows above and below.
    let mut count = 0usize;
    let mut stack = vec![(seed_x, seed_y)];
    while let Some((x, y)) = stack.pop() {
        let row = y * stride;
        let out = y * width;
        if mask[out + x] != 0 || !matches(&rgba[row + x * 4..]) {
            continue;
        }
        let mut left = x;
        let mut right = x;
        while left > 0 && mask[out + left - 1] == 0 && matches(&rgba[row + (left - 1) * 4..]) {
            left -= 1;
        }
        while right + 1 < width && mask[out + right + 1] == 0 && matches(&rgba[row + (right + 1) * 4..]) {
            right += 1;
        }
        for m in &mut mask[out + left..=out + right] {
            *m = 255;
        }
        count += right - left + 1;
        for side in 0..2 {
            if (side == 0 && y == 0) || (side == 1 && y + 1 >= height) {
                continue;
            }
            let ny = if side == 0 { y - 1 } else { y + 1 };
            let nrow = ny * stride;
            let nout = ny * width;
            let mut in_run = false;
            for nx in left..=right {
                let candidate = mask[nout + nx] == 0 && matches(&rgba[nrow + nx * 4..]);
                if candidate && !in_run {
                    stack.push((nx, ny));
                }
                in_run = candidate;
            }
        }
    }
    (mask, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, rgba: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            v.extend_from_slice(&rgba);
        }
        v
    }

    #[test]
    fn solid_image_selects_everything() {
        let img = solid(8, 8, [100, 150, 200, 255]);
        let (mask, count) = wand_mask(&img, 8, 8, 3, 3, 0, 32, true);
        assert_eq!(count, 64);
        assert!(mask.iter().all(|&m| m == 255));
    }

    #[test]
    fn out_of_bounds_seed_selects_nothing() {
        let img = solid(4, 4, [0, 0, 0, 255]);
        let (_, count) = wand_mask(&img, 4, 4, 9, 9, 0, 32, true);
        assert_eq!(count, 0);
    }

    #[test]
    fn contiguous_stops_at_barrier() {
        // 8x4: left half red, right half blue.
        let mut img = Vec::new();
        for _y in 0..4 {
            for x in 0..8 {
                if x < 4 {
                    img.extend_from_slice(&[255, 0, 0, 255]);
                } else {
                    img.extend_from_slice(&[0, 0, 255, 255]);
                }
            }
        }
        let (mask, count) = wand_mask(&img, 8, 4, 1, 1, 0, 32, true);
        assert_eq!(count, 16);
        assert!(mask[..4 * 8].iter().enumerate().all(|(i, &m)| (m == 255) == (i % 8 < 4)));
    }

    #[test]
    fn non_contiguous_finds_disconnected_matches() {
        // Red pixels at opposite corners, blue elsewhere.
        let mut img = solid(8, 8, [0, 0, 255, 255]);
        for &(x, y) in &[(0usize, 0usize), (7, 7)] {
            let b = (y * 8 + x) * 4;
            img[b..b + 4].copy_from_slice(&[255, 0, 0, 255]);
        }
        let (mask, count) = wand_mask(&img, 8, 8, 0, 0, 0, 32, false);
        assert_eq!(count, 2);
        assert_eq!(mask[0], 255);
        assert_eq!(mask[7 * 8 + 7], 255);
        // Contiguous from the same seed finds only one.
        let (_, count) = wand_mask(&img, 8, 8, 0, 0, 0, 32, true);
        assert_eq!(count, 1);
    }

    #[test]
    fn tolerance_boundary_is_inclusive() {
        // Reference 100; pixel at 100+32 matches at tolerance 32 but not 31.
        let mut img = solid(2, 1, [100, 100, 100, 255]);
        img[4] = 132;
        img[5] = 100;
        img[6] = 100;
        let (_, count) = wand_mask(&img, 2, 1, 0, 0, 0, 32, true);
        assert_eq!(count, 2);
        let (_, count) = wand_mask(&img, 2, 1, 0, 0, 0, 31, true);
        assert_eq!(count, 1);
    }

    #[test]
    fn alpha_participates_in_match() {
        // Same RGB, alpha differs by more than tolerance: no match.
        let mut img = solid(2, 1, [50, 50, 50, 255]);
        img[7] = 200;
        let (_, count) = wand_mask(&img, 2, 1, 0, 0, 0, 32, true);
        assert_eq!(count, 1);
    }

    #[test]
    fn sample_radius_averages_reference() {
        // Seed on a black pixel whose 3x3 neighborhood is mostly white:
        // the averaged reference (~227) is far from the seed itself (0),
        // so a moderate tolerance selects the white neighbors, not the seed.
        let mut img = solid(3, 3, [255, 255, 255, 255]);
        img[16..20].copy_from_slice(&[0, 0, 0, 255]); // center pixel
        let (mask, _) = wand_mask(&img, 3, 3, 1, 1, 1, 40, false);
        assert_eq!(mask[4], 0); // seed itself no longer matches
        assert_eq!(mask[0], 255); // far corner still matches the average
    }
}
