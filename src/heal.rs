//! Spot healing brush, ported from Compositor (MIT, (c) Robbie Tilton):
//! Compositor/Rendering/HealPixels.c. The algorithm is identical: coverage
//! bounds, square-dilated ring, source-patch search over angle/scale with
//! MSD scoring and ±3 refinement, multigrid SOR membrane solve, and detail
//! grain on smooth fills. Two deliberate differences: our buffers are
//! non-premultiplied, so the C's premultiplied RGB≤alpha clamp becomes a
//! plain 0..=255 clamp per channel; and the stride parameter is dropped
//! (tightly packed rows).

const OUTSIDE: u8 = 0;
const RING: u8 = 1;
const HOLE: u8 = 2;

/// Heal mode: how the membrane source is chosen.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum HealMode {
    /// Search for a matching patch elsewhere in the image (5 scales).
    ContentAware,
    /// No source patch: smooth fill from the ring, plus detail grain.
    SmoothFill,
    /// Like ContentAware but biased hard toward near patches (2 scales).
    ProximityMatch,
}

fn heal_hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846ca68b);
    x ^= x >> 16;
    x
}

fn heal_unit(key: u32) -> f64 {
    (heal_hash(key) >> 8) as f64 / 16777216.0
}

fn coverage_bounds(coverage: &[u8], width: usize, height: usize) -> Option<(usize, usize, usize, usize)> {
    let (mut x0, mut y0, mut x1, mut y1) = (width, height, 0, 0);
    for y in 0..height {
        for x in 0..width {
            if coverage[y * width + x] == 0 {
                continue;
            }
            x0 = x0.min(x);
            x1 = x1.max(x + 1);
            y0 = y0.min(y);
            y1 = y1.max(y + 1);
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0, y0, x1, y1))
}

/// Mean squared difference between the ring around the spot and the ring
/// around the patch offset by (dx, dy). Infinite when the patch would overlap
/// the spot or leave the image.
fn heal_score(rgba: &[u8], role: &[u8], wx0: i64, wy0: i64, ww: i64, wh: i64, dx: i64, dy: i64, w: i64, h: i64) -> f64 {
    if dx.abs() < ww && dy.abs() < wh {
        return f64::INFINITY;
    }
    if wx0 + dx < 0 || wy0 + dy < 0 || wx0 + ww + dx > w || wy0 + wh + dy > h {
        return f64::INFINITY;
    }
    let stride = w as usize * 4;
    let mut sum = 0.0;
    let mut n = 0u64;
    for y in 0..wh {
        for x in 0..ww {
            if role[(y * ww + x) as usize] != RING {
                continue;
            }
            let t = (wy0 + y) as usize * stride + (wx0 + x) as usize * 4;
            let s = (wy0 + y + dy) as usize * stride + (wx0 + x + dx) as usize * 4;
            for c in 0..4 {
                let d = rgba[t + c] as f64 - rgba[s + c] as f64;
                sum += d * d;
            }
            n += 1;
        }
    }
    if n > 0 { sum / n as f64 } else { f64::INFINITY }
}

/// Solves for smooth values over HOLE pixels, fixed to the RING values around
/// them. A coarser copy is solved first and used as the starting point, so
/// large spots settle in few passes.
fn heal_solve(value: &mut [f32], role: &[u8], w: usize, h: usize, depth: u32) {
    let mut iterations = 300;
    if w > 32 && h > 32 && depth < 16 {
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        let mut coarse = vec![0f32; cw * ch * 4];
        let mut coarse_role = vec![0u8; cw * ch];
        for y in 0..ch {
            for x in 0..cw {
                let (mut known, mut hole) = (0u32, 0u32);
                let mut known_sum = [0f32; 4];
                let mut hole_sum = [0f32; 4];
                for j in 0..2 {
                    for i in 0..2 {
                        let (fx, fy) = (x * 2 + i, y * 2 + j);
                        if fx >= w || fy >= h {
                            continue;
                        }
                        let p = fy * w + fx;
                        if role[p] == RING {
                            known += 1;
                            for c in 0..4 {
                                known_sum[c] += value[p * 4 + c];
                            }
                        } else if role[p] == HOLE {
                            hole += 1;
                            for c in 0..4 {
                                hole_sum[c] += value[p * 4 + c];
                            }
                        }
                    }
                }
                let q = y * cw + x;
                if known > 0 {
                    coarse_role[q] = RING;
                    for c in 0..4 {
                        coarse[q * 4 + c] = known_sum[c] / known as f32;
                    }
                } else if hole > 0 {
                    coarse_role[q] = HOLE;
                    for c in 0..4 {
                        coarse[q * 4 + c] = hole_sum[c] / hole as f32;
                    }
                }
            }
        }
        heal_solve(&mut coarse, &coarse_role, cw, ch, depth + 1);
        for y in 0..h {
            for x in 0..w {
                let (p, q) = (y * w + x, (y / 2) * cw + x / 2);
                if role[p] == HOLE && coarse_role[q] == HOLE {
                    value[p * 4..p * 4 + 4].copy_from_slice(&coarse[q * 4..q * 4 + 4]);
                }
            }
        }
        iterations = 40;
    }
    const OMEGA: f32 = 1.8;
    for _ in 0..iterations {
        for y in 0..h {
            for x in 0..w {
                let p = y * w + x;
                if role[p] != HOLE {
                    continue;
                }
                let mut sum = [0f32; 4];
                let mut n = 0u32;
                for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
                    if nx >= w || ny >= h {
                        continue;
                    }
                    let q = ny * w + nx;
                    if role[q] == OUTSIDE {
                        continue;
                    }
                    for c in 0..4 {
                        sum[c] += value[q * 4 + c];
                    }
                    n += 1;
                }
                if n == 0 {
                    continue;
                }
                for c in 0..4 {
                    value[p * 4 + c] += OMEGA * (sum[c] / n as f32 - value[p * 4 + c]);
                }
            }
        }
    }
}

/// Heal the pixels under `coverage` (w*h, 255 = full strength). Returns false
/// when there was nothing to heal. `opacity` scales the blend. `seed` drives
/// the detail grain on smooth fills.
pub fn spot_heal(rgba: &mut [u8], coverage: &[u8], width: usize, height: usize, opacity: f32, mode: HealMode, seed: u32) -> bool {
    let (w, h) = (width as i64, height as i64);
    let Some((bx0, by0, bx1, by1)) = coverage_bounds(coverage, width, height) else {
        return false;
    };
    let stride = width * 4;
    let (bw, bh) = ((bx1 - bx0) as i64, (by1 - by0) as i64);
    let size = bw.max(bh);
    let ring = (size / 8).clamp(2, 16);
    // Work box: the spot plus its ring, clipped to the image.
    let wx0 = (bx0 as i64 - ring).max(0);
    let wy0 = (by0 as i64 - ring).max(0);
    let wx1 = (bx1 as i64 + ring).min(w);
    let wy1 = (by1 as i64 + ring).min(h);
    let (ww, wh) = (wx1 - wx0, wy1 - wy0);
    let wn = (ww * wh) as usize;

    let mut role = vec![0u8; wn];
    let mut near = vec![0u8; wn];
    let mut prefix = vec![0i64; (ww.max(wh) + 1) as usize];
    let mut value = vec![0f32; wn * 4];
    for y in 0..wh {
        for x in 0..ww {
            role[(y * ww + x) as usize] =
                if coverage[(wy0 + y) as usize * width + (wx0 + x) as usize] > 0 { HOLE } else { OUTSIDE };
        }
    }
    // The ring: pixels within `ring` of the spot (square dilation, row pass
    // then column pass).
    for y in 0..wh {
        prefix[0] = 0;
        for x in 0..ww {
            prefix[(x + 1) as usize] = prefix[x as usize] + (role[(y * ww + x) as usize] == HOLE) as i64;
        }
        for x in 0..ww {
            let lo = (x - ring).max(0);
            let hi = (x + ring + 1).min(ww);
            near[(y * ww + x) as usize] = (prefix[hi as usize] - prefix[lo as usize] > 0) as u8;
        }
    }
    for x in 0..ww {
        prefix[0] = 0;
        for y in 0..wh {
            prefix[(y + 1) as usize] = prefix[y as usize] + near[(y * ww + x) as usize] as i64;
        }
        for y in 0..wh {
            let lo = (y - ring).max(0);
            let hi = (y + ring + 1).min(wh);
            let p = (y * ww + x) as usize;
            if role[p] == OUTSIDE && prefix[hi as usize] - prefix[lo as usize] > 0 {
                role[p] = RING;
            }
        }
    }
    let ring_count = role.iter().filter(|&&r| r == RING).count();
    if ring_count == 0 {
        return false;
    }

    // Source patch for Content-Aware and Proximity Match.
    let (mut ox, mut oy, mut have_source) = (0i64, 0i64, false);
    if mode != HealMode::SmoothFill {
        const FACTORS: [f64; 5] = [1.05, 1.35, 1.75, 2.25, 2.8];
        let count = if mode == HealMode::ProximityMatch { 2 } else { 5 };
        let mut best = f64::INFINITY;
        for (f, &factor) in FACTORS.iter().enumerate().take(count) {
            for a in 0..24 {
                let angle = a as f64 * std::f64::consts::PI / 12.0;
                let dx = (angle.cos() * factor * ww as f64).round() as i64;
                let dy = (angle.sin() * factor * wh as f64).round() as i64;
                let score = heal_score(rgba, &role, wx0, wy0, ww, wh, dx, dy, w, h);
                if !score.is_finite() {
                    continue;
                }
                // Nearer patches win ties.
                let score = score * if mode == HealMode::ProximityMatch { 1.0 + 0.6 * f as f64 } else { 1.0 + 0.1 * f as f64 };
                if score < best {
                    best = score;
                    ox = dx;
                    oy = dy;
                }
            }
        }
        if best.is_finite() {
            // Fine-tune the alignment so repeating texture lines up.
            let (cx, cy) = (ox, oy);
            let mut refined = heal_score(rgba, &role, wx0, wy0, ww, wh, cx, cy, w, h);
            for j in -3..=3 {
                for i in -3..=3 {
                    let score = heal_score(rgba, &role, wx0, wy0, ww, wh, cx + i, cy + j, w, h);
                    if score < refined {
                        refined = score;
                        ox = cx + i;
                        oy = cy + j;
                    }
                }
            }
            have_source = true;
        }
    }

    // Membrane: the edge difference between the original and the patch (or the
    // original itself for a smooth fill), spread across the spot.
    let mut mean = [0f64; 4];
    let mut detail = [0f64; 3];
    for y in 0..wh {
        for x in 0..ww {
            let p = (y * ww + x) as usize;
            if role[p] != RING {
                value[p * 4..p * 4 + 4].fill(0.0);
                continue;
            }
            let (ix, iy) = (wx0 + x, wy0 + y);
            let t = iy as usize * stride + ix as usize * 4;
            let s = have_source.then(|| (iy + oy) as usize * stride + (ix + ox) as usize * 4);
            for c in 0..4 {
                value[p * 4 + c] = rgba[t + c] as f32 - s.map_or(0.0, |s| rgba[s + c] as f32);
                mean[c] += value[p * 4 + c] as f64;
            }
            if !have_source {
                // Fine detail around the spot: each pixel against the average
                // of its neighbours.
                for c in 0..3 {
                    let mut around = 0.0;
                    let mut n = 0u32;
                    for (nx, ny) in [(ix - 1, iy), (ix + 1, iy), (ix, iy - 1), (ix, iy + 1)] {
                        if nx < 0 || ny < 0 || nx >= w || ny >= h {
                            continue;
                        }
                        around += rgba[ny as usize * stride + nx as usize * 4 + c] as f64;
                        n += 1;
                    }
                    if n > 0 {
                        let d = rgba[t + c] as f64 - around / n as f64;
                        detail[c] += d * d;
                    }
                }
            }
        }
    }
    for c in 0..4 {
        mean[c] /= ring_count as f64;
    }
    for p in 0..wn {
        if role[p] == HOLE {
            for c in 0..4 {
                value[p * 4 + c] = mean[c] as f32;
            }
        }
    }
    heal_solve(&mut value, &role, ww as usize, wh as usize, 0);
    for c in 0..3 {
        detail[c] = (detail[c] / ring_count as f64).sqrt() * 0.9;
    }

    for y in 0..wh {
        for x in 0..ww {
            let p = (y * ww + x) as usize;
            if role[p] != HOLE {
                continue;
            }
            let (ix, iy) = (wx0 + x, wy0 + y);
            let t = iy as usize * stride + ix as usize * 4;
            let s = have_source.then(|| (iy + oy) as usize * stride + (ix + ox) as usize * 4);
            let amount = coverage[iy as usize * width + ix as usize] as f64 / 255.0 * opacity as f64;
            let mut grain = 0.0;
            if !have_source {
                let key = heal_hash(seed ^ heal_hash((iy * w + ix) as u32));
                let u1 = heal_unit(key);
                let u2 = heal_unit(key ^ 0x68e31da4);
                grain = (-2.0 * (1.0 - u1).ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
            }
            for c in 0..4 {
                let healed = s.map_or(0.0, |s| rgba[s + c] as f64) + value[p * 4 + c] as f64
                    + if c < 3 { grain * detail[c] } else { 0.0 };
                let out = rgba[t + c] as f64 + (healed - rgba[t + c] as f64) * amount;
                rgba[t + c] = out.clamp(0.0, 255.0).round() as u8;
            }
        }
    }
    true
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

    fn dot_coverage(w: usize, h: usize, cx: usize, cy: usize, r: usize) -> Vec<u8> {
        let mut cov = vec![0u8; w * h];
        for y in cy.saturating_sub(r)..=(cy + r).min(h - 1) {
            for x in cx.saturating_sub(r)..=(cx + r).min(w - 1) {
                if (x as i64 - cx as i64).pow(2) + (y as i64 - cy as i64).pow(2) <= (r * r) as i64 {
                    cov[y * w + x] = 255;
                }
            }
        }
        cov
    }

    #[test]
    fn empty_coverage_is_noop() {
        let mut img = solid(16, 16, [10, 20, 30, 255]);
        let orig = img.clone();
        assert!(!spot_heal(&mut img, &vec![0u8; 256], 16, 16, 1.0, HealMode::ContentAware, 0));
        assert_eq!(img, orig);
    }

    #[test]
    fn smooth_fill_removes_defect() {
        // Flat gray with a black defect; smooth fill should restore ~gray.
        let mut img = solid(48, 48, [128, 128, 128, 255]);
        for y in 20..28 {
            for x in 20..28 {
                let b = (y * 48 + x) * 4;
                img[b..b + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        let cov = dot_coverage(48, 48, 23, 23, 6);
        assert!(spot_heal(&mut img, &cov, 48, 48, 1.0, HealMode::SmoothFill, 0));
        let b = (23 * 48 + 23) * 4;
        let v = img[b];
        assert!((v as i32 - 128).abs() < 20, "center healed to {v}");
    }

    #[test]
    fn content_aware_continues_stripes() {
        // Vertical stripes; damage a stripe junction; content-aware should
        // find an intact stripe column and continue the pattern.
        let mut img = Vec::new();
        for _y in 0..64 {
            for x in 0..64 {
                let v = if x % 8 < 4 { 230 } else { 30 };
                img.extend_from_slice(&[v, v, v, 255]);
            }
        }
        // Black out a 10x10 block straddling a stripe boundary.
        for y in 24..34 {
            for x in 24..34 {
                let b = (y * 64 + x) * 4;
                img[b..b + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        let cov = dot_coverage(64, 64, 29, 29, 7);
        assert!(spot_heal(&mut img, &cov, 64, 64, 1.0, HealMode::ContentAware, 0));
        // Healed pixels should be back near one of the two stripe levels.
        let mut ok = 0;
        let mut total = 0;
        for y in 24..34 {
            for x in 24..34 {
                let v = img[(y * 64 + x) * 4] as i32;
                total += 1;
                if (v - 230).abs() < 40 || (v - 30).abs() < 40 {
                    ok += 1;
                }
            }
        }
        assert!(ok * 100 / total >= 80, "{ok}/{total} pixels near stripe levels");
    }

    #[test]
    fn zero_opacity_leaves_pixels() {
        let mut img = solid(32, 32, [128, 128, 128, 255]);
        for y in 12..20 {
            for x in 12..20 {
                let b = (y * 32 + x) * 4;
                img[b..b + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        let orig = img.clone();
        let cov = dot_coverage(32, 32, 15, 15, 6);
        spot_heal(&mut img, &cov, 32, 32, 0.0, HealMode::SmoothFill, 0);
        assert_eq!(img, orig);
    }

    #[test]
    fn spot_at_edge_still_heals() {
        let mut img = solid(32, 32, [200, 200, 200, 255]);
        for y in 0..6 {
            for x in 0..6 {
                let b = (y * 32 + x) * 4;
                img[b..b + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        let cov = dot_coverage(32, 32, 2, 2, 4);
        assert!(spot_heal(&mut img, &cov, 32, 32, 1.0, HealMode::SmoothFill, 0));
        let v = img[0];
        assert!((v as i32 - 200).abs() < 30, "corner healed to {v}");
    }

    #[test]
    fn partial_coverage_blends() {
        let mut full = solid(32, 32, [128, 128, 128, 255]);
        for y in 12..20 {
            for x in 12..20 {
                let b = (y * 32 + x) * 4;
                full[b..b + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        let mut half = full.clone();
        let cov = dot_coverage(32, 32, 15, 15, 6);
        spot_heal(&mut full, &cov, 32, 32, 1.0, HealMode::SmoothFill, 0);
        let cov_half: Vec<u8> = cov.iter().map(|&v| v / 2).collect();
        spot_heal(&mut half, &cov_half, 32, 32, 1.0, HealMode::SmoothFill, 0);
        let b = (15 * 32 + 15) * 4;
        // Half-strength coverage should land between the defect (0) and the
        // full heal.
        assert!(half[b] > 10 && half[b] < full[b], "half={} full={}", half[b], full[b]);
    }
}
