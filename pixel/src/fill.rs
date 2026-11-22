//! Content-aware fill of a selection, ported from Compositor (MIT, (c) Robbie
//! Tilton): Compositor/Rendering/ContentFill.c. PatchMatch-style: BFS inward
//! from the selection boundary, each pixel tries offsets propagated from
//! already-filled neighbors plus random donors, scores by 5x5 patch match over
//! known pixels, refines with shrinking random search, then copies the donor
//! pixel. The C works on premultiplied pixels but only copies whole pixels, so
//! our non-premultiplied buffers behave identically. Deterministic: same LCG
//! seed as the C.
//!
//! Generic over the sample type, so a RAW is filled on its 16-bit frame
//! (#69). Scores are exact integer sums, and a donor is dropped as soon as
//! its sum reaches the best one: the result is the one the C gives.

use crate::resample::Sample;

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        self.0
    }
}

/// Squared difference between the patch around `p` and the one around `q`,
/// summed over `offs`: the known pixels around `p`, as index offsets. Every
/// donor has its whole patch inside the image, so the pixels compared are the
/// same for each donor of one `p`, and the sum ranks donors as the mean does.
/// `u64::MAX` when there is nothing to compare, or once the sum reaches
/// `limit`: the caller only asks whether this donor beats its best one.
fn patch_match<T: Sample>(pixels: &[T], offs: &[isize], p: usize, q: usize, limit: u64) -> u64 {
    if offs.is_empty() {
        return u64::MAX;
    }
    let mut sum = 0u64;
    for &o in offs {
        let a = &pixels[(p as isize + o) as usize * 4..][..4];
        let b = &pixels[(q as isize + o) as usize * 4..][..4];
        for c in 0..4 {
            let d = a[c].to_u32() as i64 - b[c].to_u32() as i64;
            sum += (d * d) as u64;
        }
        if sum >= limit {
            return u64::MAX;
        }
    }
    sum
}

/// Fill `mask != 0` pixels from the surrounding image. Unselected opaque
/// pixels are the source; unselected transparent pixels are neither filled nor
/// sampled. Returns false when there is nothing to fill or no usable donor.
pub fn content_fill<T: Sample>(pixels: &mut [T], mask: &[u8], w: usize, h: usize) -> bool {
    const NONE: u32 = u32::MAX;
    let n = w * h;
    let opaque = T::nearest(f64::MAX).to_u32();
    let mut known = vec![0u8; n];
    let mut target = vec![0u8; n];
    let mut valid = vec![0u8; n];
    let mut queued = vec![0u8; n];
    let mut chosen = vec![NONE; n];
    let mut donors: Vec<u32> = Vec::new();
    let mut queue: Vec<usize> = Vec::new();
    let radius: i32 = if w >= 5 && h >= 5 { 2 } else { 0 };

    let mut missing = 0usize;
    for p in 0..n {
        target[p] = (mask[p] != 0) as u8;
        known[p] = (target[p] == 0 && pixels[p * 4 + 3].to_u32() == opaque) as u8;
        missing += target[p] as usize;
    }
    if missing == 0 {
        return true;
    }
    // Donors need their whole match patch inside known pixels.
    for y in 0..h {
        for x in 0..w {
            let p = y * w + x;
            if known[p] == 0 {
                continue;
            }
            let mut ok = true;
            'patch: for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let sx = x as i32 + dx;
                    let sy = y as i32 + dy;
                    if sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32
                        || known[sy as usize * w + sx as usize] == 0
                    {
                        ok = false;
                        break 'patch;
                    }
                }
            }
            if ok {
                valid[p] = 1;
                donors.push(p as u32);
            }
        }
    }
    if donors.is_empty() {
        return false;
    }
    // Seed the queue with target pixels touching known ones.
    for y in 0..h {
        for x in 0..w {
            let p = y * w + x;
            if target[p] != 0
                && ((x > 0 && known[p - 1] != 0)
                    || (x + 1 < w && known[p + 1] != 0)
                    || (y > 0 && known[p - w] != 0)
                    || (y + 1 < h && known[p + w] != 0))
            {
                queue.push(p);
                queued[p] = 1;
            }
        }
    }
    let mut rng = Rng(0x6d2b79f5);
    let mut head = 0usize;
    let mut scan = 0usize;
    let mut offs: Vec<isize> = Vec::with_capacity(25);
    loop {
        while head < queue.len() {
            let p = queue[head];
            head += 1;
            let (x, y) = (p % w, p / w);
            let neighbors = [
                if x > 0 { Some(p - 1) } else { None },
                if x + 1 < w { Some(p + 1) } else { None },
                if y > 0 { Some(p - w) } else { None },
                if y + 1 < h { Some(p + w) } else { None },
            ];
            // The known pixels around p: what each donor is scored on.
            offs.clear();
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    if sx >= 0 && sy >= 0 && sx < w as i32 && sy < h as i32
                        && known[sy as usize * w + sx as usize] != 0
                    {
                        offs.push(dy as isize * w as isize + dx as isize);
                    }
                }
            }
            // Propagate coherent source offsets, then refine with randomized
            // patch search.
            let mut best: i64 = -1;
            let mut score = u64::MAX;
            // k < 4 indexes the neighbour array; the rest are random donors.
            #[allow(clippy::needless_range_loop)]
            for k in 0..28 {
                let q: i64 = if k < 4 {
                    match neighbors[k] {
                        Some(t) => {
                            let base = if chosen[t] != NONE { chosen[t] as i64 } else { t as i64 };
                            base + p as i64 - t as i64
                        }
                        None => -1,
                    }
                } else {
                    donors[(rng.next() as usize) % donors.len()] as i64
                };
                if q < 0 || q >= n as i64 || valid[q as usize] == 0 {
                    continue;
                }
                let s = patch_match(pixels, &offs, p, q as usize, score);
                if best < 0 || s < score {
                    score = s;
                    best = q;
                }
            }
            if best < 0 {
                best = donors[0] as i64;
            }
            let mut r = 64;
            while r >= 1 {
                let qx = best as usize % w;
                let qy = best as usize / w;
                let nx = qx as i64 + (rng.next() % (2 * r + 1)) as i64 - r as i64;
                let ny = qy as i64 + (rng.next() % (2 * r + 1)) as i64 - r as i64;
                r /= 2;
                if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64
                    || valid[ny as usize * w + nx as usize] == 0
                {
                    continue;
                }
                let q = ny as usize * w + nx as usize;
                let s = patch_match(pixels, &offs, p, q, score);
                if s < score {
                    score = s;
                    best = q as i64;
                }
            }
            let src = best as usize * 4;
            let dst = p * 4;
            pixels.copy_within(src..src + 4, dst);
            known[p] = 1;
            chosen[p] = best as u32;
            for q in neighbors.into_iter().flatten() {
                if target[q] != 0 && known[q] == 0 && queued[q] == 0 {
                    queued[q] = 1;
                    queue.push(q);
                }
            }
        }
        // A selected area that only transparency touches starts from the best
        // random donor, then spreads.
        while scan < n && (target[scan] == 0 || known[scan] != 0) {
            scan += 1;
        }
        if scan >= n {
            break;
        }
        queue.push(scan);
        queued[scan] = 1;
    }
    true
}

/// What a fill wrote: the `w`×`h` box at (`x`, `y`) of the frame it ran on,
/// after the fill. `cover` is 1 where a pixel was filled. The box reaches one
/// pixel past the filled ones, so a resample of the patch has the pixels on
/// both sides of its edge. Held 16 bits wide for either frame depth, which
/// keeps an 8-bit value as it is.
pub struct Patch {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u16>,
    pub cover: Vec<u8>,
}

/// [`content_fill`] with its donors taken from around the selection: the
/// mask's bounding box grown by its longer side on each edge. The search
/// then works on that area and not on the whole frame, so its time and
/// memory follow the selection. Returns what it wrote, or None when there
/// was nothing to fill or nothing to fill it from.
pub fn fill_near<T: Sample>(pixels: &mut [T], mask: &[u8], w: usize, h: usize) -> Option<Patch> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (y, row) in mask.chunks_exact(w.max(1)).enumerate() {
        for (x, &m) in row.iter().enumerate() {
            if m != 0 {
                x0 = x0.min(x);
                x1 = x1.max(x + 1);
                y0 = y0.min(y);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let grow = (x1 - x0).max(y1 - y0).max(64);
    let (rx, ry) = (x0.saturating_sub(grow), y0.saturating_sub(grow));
    let (rw, rh) = ((x1 + grow).min(w) - rx, (y1 + grow).min(h) - ry);
    let mut area: Vec<T> = Vec::with_capacity(rw * rh * 4);
    let mut near = Vec::with_capacity(rw * rh);
    for y in ry..ry + rh {
        area.extend_from_slice(&pixels[(y * w + rx) * 4..(y * w + rx + rw) * 4]);
        near.extend_from_slice(&mask[y * w + rx..y * w + rx + rw]);
    }
    if !content_fill(&mut area, &near, rw, rh) {
        return None;
    }
    let (x0, y0) = (x0.saturating_sub(1), y0.saturating_sub(1));
    let (bw, bh) = ((x1 + 1).min(w) - x0, (y1 + 1).min(h) - y0);
    let mut patch = Patch { x: x0, y: y0, w: bw, h: bh, rgba: Vec::with_capacity(bw * bh * 4), cover: Vec::with_capacity(bw * bh) };
    for y in y0..y0 + bh {
        for x in x0..x0 + bw {
            let d = (y * w + x) * 4;
            let filled = mask[y * w + x] != 0;
            if filled {
                let s = ((y - ry) * rw + x - rx) * 4;
                pixels[d..d + 4].copy_from_slice(&area[s..s + 4]);
            }
            patch.cover.push(filled as u8);
            patch.rgba.extend(pixels[d..d + 4].iter().map(|v| v.to_u32() as u16));
        }
    }
    Some(patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RefRng(u32);

    impl RefRng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
            self.0
        }
    }

    /// Mean squared difference over the (2*radius+1)^2 patch around p vs q,
    /// counting only pixels inside the image that are known. DBL_MAX equivalent
    /// when nothing overlaps.
    fn ref_patch_match(pixels: &[u8], known: &[u8], w: usize, h: usize, p: usize, q: usize, radius: i32) -> f64 {
        let (px, py) = (p % w, p / w);
        let (qx, qy) = (q % w, q / w);
        let mut sum = 0.0f64;
        let mut count = 0u32;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let x = px as i32 + dx;
                let y = py as i32 + dy;
                let sx = qx as i32 + dx;
                let sy = qy as i32 + dy;
                if x < 0 || y < 0 || x >= w as i32 || y >= h as i32
                    || sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32
                    || known[y as usize * w + x as usize] == 0
                {
                    continue;
                }
                let a = &pixels[(y as usize * w + x as usize) * 4..];
                let b = &pixels[(sy as usize * w + sx as usize) * 4..];
                for c in 0..4 {
                    let d = a[c] as f64 - b[c] as f64;
                    sum += d * d;
                }
                count += 1;
            }
        }
        if count > 0 { sum / count as f64 } else { f64::MAX }
    }

    /// The fill as it was before the integer scores and the early exit: f64
        /// means over every patch, on 8-bit pixels. Kept to pin the result.
    fn reference_fill(pixels: &mut [u8], mask: &[u8], w: usize, h: usize) -> bool {
        let n = w * h;
        let mut known = vec![0u8; n];
        let mut target = vec![0u8; n];
        let mut valid = vec![0u8; n];
        let mut queued = vec![0u8; n];
        let mut chosen = vec![-1i64; n];
        let mut donors: Vec<usize> = Vec::new();
        let mut queue: Vec<usize> = Vec::new();
        let radius = if w >= 5 && h >= 5 { 2 } else { 0 };

        let mut missing = 0usize;
        for y in 0..h {
            for x in 0..w {
                let p = y * w + x;
                target[p] = (mask[p] != 0) as u8;
                known[p] = (target[p] == 0 && pixels[p * 4 + 3] == 255) as u8;
                if target[p] != 0 {
                    missing += 1;
                }
            }
        }
        if missing == 0 {
            return true;
        }
        // Donors need their whole match patch inside known pixels.
        for y in 0..h {
            for x in 0..w {
                let p = y * w + x;
                if known[p] == 0 {
                    continue;
                }
                let mut ok = true;
                'patch: for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let sx = x as i32 + dx;
                        let sy = y as i32 + dy;
                        if sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32
                            || known[sy as usize * w + sx as usize] == 0
                        {
                            ok = false;
                            break 'patch;
                        }
                    }
                }
                if ok {
                    valid[p] = 1;
                    donors.push(p);
                }
            }
        }
        if donors.is_empty() {
            return false;
        }
        // Seed the queue with target pixels touching known ones.
        for y in 0..h {
            for x in 0..w {
                let p = y * w + x;
                if target[p] != 0
                    && ((x > 0 && known[p - 1] != 0)
                        || (x + 1 < w && known[p + 1] != 0)
                        || (y > 0 && known[p - w] != 0)
                        || (y + 1 < h && known[p + w] != 0))
                {
                    queue.push(p);
                    queued[p] = 1;
                }
            }
        }
        let mut rng = RefRng(0x6d2b79f5);
        let mut head = 0usize;
        let mut scan = 0usize;
        loop {
            while head < queue.len() {
                let p = queue[head];
                head += 1;
                let (x, y) = (p % w, p / w);
                let neighbors = [
                    if x > 0 { Some(p - 1) } else { None },
                    if x + 1 < w { Some(p + 1) } else { None },
                    if y > 0 { Some(p - w) } else { None },
                    if y + 1 < h { Some(p + w) } else { None },
                ];
                // Propagate coherent source offsets, then refine with randomized
                // patch search.
                let mut best: i64 = -1;
                let mut score = f64::MAX;
                // k < 4 indexes the neighbour array; the rest are random donors.
                #[allow(clippy::needless_range_loop)]
                for k in 0..28 {
                    let q: i64 = if k < 4 {
                        match neighbors[k] {
                            Some(t) => {
                                let base = if chosen[t] >= 0 { chosen[t] } else { t as i64 };
                                base + p as i64 - t as i64
                            }
                            None => -1,
                        }
                    } else {
                        donors[(rng.next() as usize) % donors.len()] as i64
                    };
                    if q < 0 || q >= n as i64 || valid[q as usize] == 0 {
                        continue;
                    }
                    let s = ref_patch_match(pixels, &known, w, h, p, q as usize, radius);
                    if best < 0 || s < score {
                        score = s;
                        best = q;
                    }
                }
                if best < 0 {
                    best = donors[0] as i64;
                }
                let mut r = 64;
                while r >= 1 {
                    let qx = best as usize % w;
                    let qy = best as usize / w;
                    let nx = qx as i64 + (rng.next() % (2 * r + 1)) as i64 - r as i64;
                    let ny = qy as i64 + (rng.next() % (2 * r + 1)) as i64 - r as i64;
                    r /= 2;
                    if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64
                        || valid[ny as usize * w + nx as usize] == 0
                    {
                        continue;
                    }
                    let q = ny as usize * w + nx as usize;
                    let s = ref_patch_match(pixels, &known, w, h, p, q, radius);
                    if s < score {
                        score = s;
                        best = q as i64;
                    }
                }
                let src = best as usize * 4;
                let dst = p * 4;
                pixels.copy_within(src..src + 4, dst);
                known[p] = 1;
                chosen[p] = best;
                for q in neighbors.into_iter().flatten() {
                    if target[q] != 0 && known[q] == 0 && queued[q] == 0 {
                        queued[q] = 1;
                        queue.push(q);
                    }
                }
            }
            // A selected area that only transparency touches starts from the best
            // random donor, then spreads.
            while scan < n && (target[scan] == 0 || known[scan] != 0) {
                scan += 1;
            }
            if scan >= n {
                break;
            }
            queue.push(scan);
            queued[scan] = 1;
        }
        true
    }

    fn solid(w: usize, h: usize, px: [u8; 4]) -> Vec<u8> {
        let mut d = Vec::with_capacity(w * h * 4);
        for _ in 0..w * h {
            d.extend_from_slice(&px);
        }
        d
    }

    #[test]
    fn empty_mask_is_noop() {
        let mut img = solid(16, 16, [10, 20, 30, 255]);
        let orig = img.clone();
        assert!(content_fill(&mut img, &vec![0u8; 256], 16, 16));
        assert_eq!(img, orig);
    }

    #[test]
    fn fills_hole_from_surroundings() {
        let mut img = solid(32, 32, [200, 100, 50, 255]);
        let mut mask = vec![0u8; 32 * 32];
        for y in 12..20 {
            for x in 12..20 {
                mask[y * 32 + x] = 255;
                img[(y * 32 + x) * 4] = 0;
                img[(y * 32 + x) * 4 + 1] = 0;
                img[(y * 32 + x) * 4 + 2] = 0;
            }
        }
        assert!(content_fill(&mut img, &mask, 32, 32));
        for y in 12..20 {
            for x in 12..20 {
                let p = &img[(y * 32 + x) * 4..];
                assert_eq!(&p[..3], &[200, 100, 50], "pixel {x},{y} filled");
            }
        }
    }

    #[test]
    fn continues_stripes_across_hole() {
        // Horizontal stripes; an 8-row hole should come back striped.
        let (w, h) = (48, 48);
        let mut img: Vec<u8> = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let v = if (y / 4) % 2 == 0 { 220 } else { 30 };
            for _ in 0..w {
                img.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let mut mask = vec![0u8; w * h];
        for y in 20..28 {
            for x in 8..40 {
                mask[y * w + x] = 255;
            }
        }
        assert!(content_fill(&mut img, &mask, w, h));
        let mut agree = 0;
        let mut total = 0;
        for y in 21..27 {
            let want = if (y / 4) % 2 == 0 { 220 } else { 30 };
            for x in 10..38 {
                total += 1;
                let got = img[(y * w + x) * 4];
                if (got as i32 - want as i32).abs() < 40 {
                    agree += 1;
                }
            }
        }
        assert!(agree * 100 / total > 70, "stripe agreement {agree}/{total}");
    }

    #[test]
    fn deterministic_across_runs() {
        let mut a = solid(32, 32, [128, 128, 128, 255]);
        let mut b = a.clone();
        let mut mask = vec![0u8; 32 * 32];
        for y in 10..22 {
            for x in 10..22 {
                mask[y * 32 + x] = 255;
            }
        }
        content_fill(&mut a, &mask, 32, 32);
        content_fill(&mut b, &mask, 32, 32);
        assert_eq!(a, b);
    }

    #[test]
    fn transparent_surroundings_are_not_donors() {
        // Hole whose entire neighborhood is transparent: fills from a random
        // opaque donor (or stays if none) but never copies transparency.
        let mut img = solid(16, 16, [90, 90, 90, 0]);
        for y in 0..2 {
            for x in 0..2 {
                img[(y * 16 + x) * 4..][..4].copy_from_slice(&[10, 200, 30, 255]);
            }
        }
        let mut mask = vec![0u8; 16 * 16];
        mask[8 * 16 + 8] = 255;
        // Donors need a full 5x5 known patch; the only opaque pixels are at
        // the corner edge, so there are no valid donors → returns false.
        assert!(!content_fill(&mut img, &mask, 16, 16));
        assert_eq!(img[(8 * 16 + 8) * 4 + 3], 0);
    }

    #[test]
    fn full_mask_returns_false() {
        let mut img = solid(8, 8, [1, 2, 3, 255]);
        assert!(!content_fill(&mut img, &vec![255u8; 64], 8, 8));
    }
    /// A noisy picture with structure: bands, a gradient and a few
    /// transparent pixels, so scores tie rarely and every branch runs.
    fn textured(w: usize, h: usize) -> Vec<u8> {
        let mut rng = Rng(7);
        let mut img = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let band = if (x / 6 + y / 9) % 2 == 0 { 150 } else { 60 };
                let n = (rng.next() >> 24) as usize % 40;
                let v = (band + n + x) as u8;
                let a = if (x * 7 + y * 13) % 97 == 0 { 0 } else { 255 };
                img.extend_from_slice(&[v, v / 2 + 20, 255 - v, a]);
            }
        }
        img
    }

    fn blob(w: usize, h: usize) -> Vec<u8> {
        let mut mask = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let (dx, dy) = (x as i32 - w as i32 / 2, y as i32 - h as i32 / 2);
                if dx * dx + dy * dy * 2 < (w as i32 / 5).pow(2) || (x < 3 && y < 9) {
                    mask[y * w + x] = 200;
                }
            }
        }
        mask
    }

    #[test]
    fn the_fill_gives_the_pixels_the_full_comparison_gave() {
        for (w, h) in [(64, 48), (41, 67), (4, 30)] {
            let (mut now, mut then) = (textured(w, h), textured(w, h));
            let mask = blob(w, h);
            assert_eq!(content_fill(&mut now, &mask, w, h), reference_fill(&mut then, &mask, w, h));
            assert_eq!(now, then, "{w}x{h}");
        }
    }

    #[test]
    fn the_fill_is_the_same_at_8_and_16_bits() {
        let (w, h) = (64, 48);
        let mut narrow = textured(w, h);
        let mut wide: Vec<u16> = narrow.iter().map(|&v| v as u16 * 257).collect();
        let mask = blob(w, h);
        assert!(content_fill(&mut narrow, &mask, w, h));
        assert!(content_fill(&mut wide, &mask, w, h));
        let widened: Vec<u16> = narrow.iter().map(|&v| v as u16 * 257).collect();
        assert_eq!(wide, widened);
    }

    #[test]
    fn a_fill_from_nearby_reports_what_it_wrote() {
        let (w, h) = (400, 300);
        let mut img = textured(w, h);
        let before = img.clone();
        let mut mask = vec![0u8; w * h];
        for y in 140..160 {
            for x in 190..215 {
                if (x + y) % 11 != 0 {
                    mask[y * w + x] = 255;
                }
            }
        }
        let patch = fill_near(&mut img, &mask, w, h).expect("filled");
        // One pixel of the frame around the filled ones comes along.
        assert_eq!((patch.x, patch.y, patch.w, patch.h), (189, 139, 27, 22));
        for p in 0..w * h {
            let (x, y) = (p % w, p / w);
            if mask[p] == 0 {
                assert_eq!(img[p * 4..p * 4 + 4], before[p * 4..p * 4 + 4], "untouched {x},{y}");
            } else {
                // A donor pixel: opaque.
                assert_eq!(img[p * 4 + 3], 255);
            }
            if (189..216).contains(&x) && (139..161).contains(&y) {
                let i = (y - 139) * 27 + x - 189;
                assert_eq!(patch.cover[i], (mask[p] != 0) as u8);
                for c in 0..4 {
                    assert_eq!(patch.rgba[i * 4 + c], img[p * 4 + c] as u16);
                }
            }
        }
        // Nothing selected, or nothing to take from: no patch.
        assert!(fill_near(&mut img, &vec![0u8; w * h], w, h).is_none());
        assert!(fill_near(&mut img, &vec![255u8; w * h], w, h).is_none());
    }
}
