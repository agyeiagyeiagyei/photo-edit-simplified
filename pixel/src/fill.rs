//! Content-aware fill of a selection, ported from Compositor (MIT, (c) Robbie
//! Tilton): Compositor/Rendering/ContentFill.c. PatchMatch-style: BFS inward
//! from the selection boundary, each pixel tries offsets propagated from
//! already-filled neighbors plus random donors, scores by 5x5 patch match over
//! known pixels, refines with shrinking random search, then copies the donor
//! pixel. The C works on premultiplied pixels but only copies whole pixels, so
//! our non-premultiplied buffers behave identically. Deterministic: same LCG
//! seed as the C.

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        self.0
    }
}

/// Mean squared difference over the (2*radius+1)^2 patch around p vs q,
/// counting only pixels inside the image that are known. DBL_MAX equivalent
/// when nothing overlaps.
fn patch_match(pixels: &[u8], known: &[u8], w: usize, h: usize, p: usize, q: usize, radius: i32) -> f64 {
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

/// Fill `mask != 0` pixels from the surrounding image. Unselected opaque
/// pixels are the source; unselected transparent pixels are neither filled nor
/// sampled. Returns false when there is nothing to fill or no usable donor.
pub fn content_fill(pixels: &mut [u8], mask: &[u8], w: usize, h: usize) -> bool {
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
    let mut rng = Rng(0x6d2b79f5);
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
                let s = patch_match(pixels, &known, w, h, p, q as usize, radius);
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
                let s = patch_match(pixels, &known, w, h, p, q, radius);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut img = Vec::with_capacity(w * h * 4);
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
}
