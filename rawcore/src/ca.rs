//! Lateral chromatic aberration: a lens focuses red and blue at a slightly
//! different magnification than green, so towards the corners edges grow a
//! red or cyan (blue or yellow) fringe. A camera removes it from its own
//! JPEG; a RAW still carries it.
//!
//! **Measured from the photo, not a lens table.** The Bayer mosaic is read as
//! its 2x2 cells: one red, one blue and two green samples each. In blocks of
//! cells with detail, red (then blue) is slid along the line to the image
//! centre until its edges sit on green's; the best slide is the block's
//! fringe width. Those widths are fitted with `s(r) = a*r + b*r^3`, `r` the
//! distance from the centre (the magnification error and its first
//! field-dependent term), plus the constant offset between the channel's
//! sample site and green's. Blocks that disagree with the fit (motion, a
//! coloured edge, noise) are weighted out. A frame with too little agreeing
//! detail, or a fit under a tenth of a pixel, is left untouched.
//!
//! **Applied to the mosaic, before the demosaic.** The red and the blue
//! samples are resampled (cubic, on their own lattice) onto green's geometry.
//! A demosaic rebuilds red's and blue's fine detail from green, so afterwards
//! the fringe is half hidden and a shift would move that borrowed detail the
//! wrong way; before it, the demosaic simply sees an aligned sensor. Green is
//! not touched. The half-size proxy and the full develop start from the same
//! mosaic, so they get the same correction.


/// Block edge and the margin read around it for the slides.
const BLOCK: usize = 32;
const MARGIN: usize = 8;
const PATCH: usize = BLOCK + 2 * MARGIN;
/// About this many blocks across the long edge.
const BLOCKS_ACROSS: usize = 44;
/// Blocks nearer the centre than this share of the half-diagonal carry no
/// measurable fringe.
const MIN_RHO: f32 = 0.25;
/// Largest fringe looked for, as a share of the half-diagonal (and at least
/// a pixel and a half): 4.5 px at the corner of a 24 MP frame.
const MAX_SHIFT_SHARE: f32 = 0.0012;
const STEP: f32 = 0.5;
/// A block counts when its best slide correlates this well with green.
const MIN_NCC: f32 = 0.6;
/// Fewest agreeing blocks for a fit to be trusted.
const MIN_BLOCKS: usize = 40;
/// A fit whose blocks scatter more than this (px, robust deviation) is not
/// a lens signature.
const MAX_SCATTER: f32 = 0.45;
/// A correction smaller than this at its largest is not worth a resample.
const MIN_CORRECTION: f32 = 0.1;

/// The active area of a Bayer mosaic, addressed by 2x2 cell.
pub struct Mosaic<'a> {
    pub data: &'a mut [f32],
    /// Samples per sensor row.
    pub stride: usize,
    /// Top-left sample of the first cell.
    pub x0: usize,
    pub y0: usize,
    /// Size in cells.
    pub w: usize,
    pub h: usize,
    /// `(dx, dy)` inside a cell of the red, the two green and the blue sample.
    pub sites: [(usize, usize); 4],
}

impl Mosaic<'_> {
    #[inline]
    fn index(&self, site: usize, x: usize, y: usize) -> usize {
        let (dx, dy) = self.sites[site];
        (self.y0 + 2 * y + dy) * self.stride + self.x0 + 2 * x + dx
    }
}

/// `s(r) = (a + b * (r / rd)^2) * r`, in pixels along the radius.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Radial {
    pub a: f32,
    pub b: f32,
}

impl Radial {
    fn factor(&self, rho2: f32) -> f32 {
        self.a + self.b * rho2
    }
}

struct Sample {
    r: f32,
    rho2: f32,
    ux: f32,
    uy: f32,
    shift: f32,
    weight: f32,
}

/// Box mean of radius `r` over a patch (edges clamp).
fn box_mean(src: &[f32], dst: &mut [f32], tmp: &mut [f32], r: usize) {
    let n = (2 * r + 1) as f32;
    for y in 0..PATCH {
        for x in 0..PATCH {
            let mut s = 0.0;
            for k in 0..=2 * r {
                s += src[y * PATCH + (x + k).saturating_sub(r).min(PATCH - 1)];
            }
            tmp[y * PATCH + x] = s / n;
        }
    }
    for y in 0..PATCH {
        for x in 0..PATCH {
            let mut s = 0.0;
            for k in 0..=2 * r {
                s += tmp[(y + k).saturating_sub(r).min(PATCH - 1) * PATCH + x];
            }
            dst[y * PATCH + x] = s / n;
        }
    }
}

/// Band-pass of a patch: a 3x3 mean less a 7x7 mean. The fine mean takes
/// the pixel noise down, so the slide's interpolation (which smooths too)
/// cannot pull the match towards half-pixel positions; the coarse one takes
/// out the shading.
fn band_pass(src: &[f32], dst: &mut [f32], a: &mut [f32], tmp: &mut [f32]) {
    box_mean(src, a, tmp, 1);
    box_mean(src, dst, tmp, 3);
    for (d, &f) in dst.iter_mut().zip(a.iter()) {
        *d = f - *d;
    }
}

/// Correlation of `g` over the block with `c` read `s` pixels along `(ux, uy)`.
fn ncc_at(g: &[f32], c: &[f32], ux: f32, uy: f32, s: f32) -> f32 {
    let (ox, oy) = (s * ux, s * uy);
    let (fx, fy) = (ox.floor(), oy.floor());
    let (tx, ty) = (ox - fx, oy - fy);
    let (ix, iy) = (fx as isize, fy as isize);
    let w = [(1.0 - tx) * (1.0 - ty), tx * (1.0 - ty), (1.0 - tx) * ty, tx * ty];
    let (mut sgc, mut sgg, mut scc) = (0f32, 0f32, 0f32);
    for y in MARGIN..MARGIN + BLOCK {
        let gi = y * PATCH + MARGIN;
        let ci = ((y as isize + iy) * PATCH as isize + MARGIN as isize + ix) as usize;
        for x in 0..BLOCK {
            let gv = g[gi + x];
            let j = ci + x;
            let cv = w[0] * c[j] + w[1] * c[j + 1] + w[2] * c[j + PATCH] + w[3] * c[j + PATCH + 1];
            sgc += gv * cv;
            sgg += gv * gv;
            scc += cv * cv;
        }
    }
    let d = (sgg * scc).sqrt();
    if d > 0.0 {
        sgc / d
    } else {
        0.0
    }
}

/// The slide of channel `c` against green in one block, with its confidence.
fn block_shift(g: &[f32], c: &[f32], ux: f32, uy: f32, max_shift: f32) -> Option<(f32, f32)> {
    let n = (max_shift / STEP).floor() as i32;
    let mut best = (f32::MIN, 0i32);
    let mut scores = Vec::with_capacity((2 * n + 1) as usize);
    for k in -n..=n {
        let v = ncc_at(g, c, ux, uy, k as f32 * STEP);
        scores.push(v);
        if v > best.0 {
            best = (v, k);
        }
    }
    // A peak at the end of the range is not a peak.
    if best.0 < MIN_NCC || best.1.abs() == n {
        return None;
    }
    let i = (best.1 + n) as usize;
    let (lo, mid, hi) = (scores[i - 1], scores[i], scores[i + 1]);
    let curve = lo - 2.0 * mid + hi;
    let frac = if curve < 0.0 { 0.5 * (lo - hi) / curve } else { 0.0 };
    Some(((best.1 as f32 + frac.clamp(-0.5, 0.5)) * STEP, best.0))
}

/// The fitted slide: the radial part, and a constant offset between the
/// channel's sample sites and green's (a binned Bayer frame has one).
#[derive(Clone, Copy)]
struct Fit {
    radial: Radial,
    ox: f32,
    oy: f32,
}

impl Fit {
    fn at(&self, s: &Sample) -> f32 {
        self.radial.factor(s.rho2) * s.r + self.ox * s.ux + self.oy * s.uy
    }
}

/// Weighted least squares of `shift = (a + b * rho2) * r + o . u`.
fn fit(samples: &[Sample], weights: &[f32]) -> Option<Fit> {
    let mut m = [[0f64; 5]; 4];
    for (s, &w) in samples.iter().zip(weights) {
        let row = [s.r as f64, (s.r * s.rho2) as f64, s.ux as f64, s.uy as f64, s.shift as f64];
        for i in 0..4 {
            for j in 0..5 {
                m[i][j] += w as f64 * row[i] * row[j];
            }
        }
    }
    // Gaussian elimination with partial pivoting; columns scaled by their
    // diagonal so the pivot test is unit-free.
    let scale: [f64; 4] = std::array::from_fn(|i| m[i][i].sqrt());
    if scale.iter().any(|&v| v <= 0.0) {
        return None;
    }
    for i in 0..4 {
        for j in 0..5 {
            m[i][j] /= scale[i] * if j < 4 { scale[j] } else { 1.0 };
        }
    }
    for col in 0..4 {
        let piv = (col..4).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[piv][col].abs() < 1e-6 {
            return None;
        }
        m.swap(col, piv);
        for row in 0..4 {
            if row != col {
                let f = m[row][col] / m[col][col];
                for j in col..5 {
                    m[row][j] -= f * m[col][j];
                }
            }
        }
    }
    let x: [f32; 4] = std::array::from_fn(|i| (m[i][4] / m[i][i] / scale[i]) as f32);
    Some(Fit { radial: Radial { a: x[0], b: x[1] }, ox: x[2], oy: x[3] })
}

fn median(v: &mut [f32]) -> f32 {
    v.sort_unstable_by(f32::total_cmp);
    v[v.len() / 2]
}

/// Robust fit: blocks far from the curve lose their weight over a few rounds.
fn robust_fit(samples: &[Sample], max_shift: f32) -> Option<Radial> {
    if samples.len() < MIN_BLOCKS {
        return None;
    }
    let mut weights: Vec<f32> = samples.iter().map(|s| s.weight).collect();
    let mut model = fit(samples, &weights)?;
    let mut scatter = f32::MAX;
    for _ in 0..4 {
        let res: Vec<f32> = samples.iter().map(|s| s.shift - model.at(s)).collect();
        scatter = 1.4826 * median(&mut res.iter().map(|v| v.abs()).collect::<Vec<_>>());
        let cut = (2.5 * scatter).max(0.15);
        for ((w, s), r) in weights.iter_mut().zip(samples).zip(&res) {
            let t = (r / cut).abs();
            *w = if t < 1.0 { s.weight * (1.0 - t * t) * (1.0 - t * t) } else { 0.0 };
        }
        model = fit(samples, &weights)?;
    }
    let kept = weights.iter().filter(|&&w| w > 0.0).count();
    if kept < MIN_BLOCKS || scatter > MAX_SCATTER {
        return None;
    }
    // Largest correction over the field, at the corner's distance `rd`.
    let rd = samples.iter().map(|s| s.r / s.rho2.sqrt().max(1e-6)).fold(0f32, f32::max);
    let peak = (1..=10)
        .map(|i| {
            let rho = i as f32 / 10.0;
            (model.radial.factor(rho * rho) * rho * rd).abs()
        })
        .fold(0f32, f32::max);
    (MIN_CORRECTION..=max_shift * 1.2).contains(&peak).then_some(model.radial)
}

/// Measure red's and blue's radial slide against green.
pub fn estimate(img: &Mosaic) -> [Option<Radial>; 2] {
    let (w, h) = (img.w, img.h);
    if w < 8 * PATCH || h < 8 * PATCH {
        return [None, None];
    }
    let (cx, cy) = ((w - 1) as f32 * 0.5, (h - 1) as f32 * 0.5);
    let rd = (cx * cx + cy * cy).sqrt();
    let max_shift = (MAX_SHIFT_SHARE * rd).max(1.5).min((MARGIN - 3) as f32);
    let step = (w.max(h) / BLOCKS_ACROSS).max(BLOCK);
    let mut samples: [Vec<Sample>; 2] = [Vec::new(), Vec::new()];
    let mut patch = [vec![0f32; PATCH * PATCH], vec![0f32; PATCH * PATCH], vec![0f32; PATCH * PATCH]];
    let mut hp = [vec![0f32; PATCH * PATCH], vec![0f32; PATCH * PATCH], vec![0f32; PATCH * PATCH]];
    let (mut fine, mut tmp) = (vec![0f32; PATCH * PATCH], vec![0f32; PATCH * PATCH]);
    let mut y0 = step / 2;
    while y0 + PATCH < h {
        let mut x0 = step / 2;
        while x0 + PATCH < w {
            let (dx, dy) = (x0 as f32 + PATCH as f32 * 0.5 - cx, y0 as f32 + PATCH as f32 * 0.5 - cy);
            let r = (dx * dx + dy * dy).sqrt();
            let rho = r / rd;
            if rho >= MIN_RHO {
                let mut top = 0f32;
                for py in 0..PATCH {
                    for px in 0..PATCH {
                        let at = |site: usize| img.data[img.index(site, x0 + px, y0 + py)];
                        let v = [at(0), 0.5 * (at(1) + at(2)), at(3)];
                        for ch in 0..3 {
                            patch[ch][py * PATCH + px] = v[ch];
                        }
                        top = top.max(v[0]).max(v[1]).max(v[2]);
                    }
                }
                // A clipped channel has no edge where the others still do.
                if top < 0.98 {
                    for ch in 0..3 {
                        band_pass(&patch[ch], &mut hp[ch], &mut fine, &mut tmp);
                    }
                    let (ux, uy) = (dx / r, dy / r);
                    for (k, ch) in [0usize, 2].into_iter().enumerate() {
                        if let Some((shift, ncc)) = block_shift(&hp[1], &hp[ch], ux, uy, max_shift) {
                            samples[k].push(Sample { r, rho2: rho * rho, ux, uy, shift, weight: ncc * ncc });
                        }
                    }
                }
            }
            x0 += step;
        }
        y0 += step;
    }
    [robust_fit(&samples[0], max_shift), robust_fit(&samples[1], max_shift)]
}

/// Catmull-Rom weights for a fraction `t`.
#[inline]
fn cubic(t: f32) -> [f32; 4] {
    let (t2, t3) = (t * t, t * t * t);
    [
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    ]
}

/// Resample the red and the blue samples onto green's geometry, in place:
/// along each row of the channel's lattice, then down each column. The slide
/// changes by thousandths of a pixel over its own length, so the two passes
/// equal the 2-D warp.
pub fn apply(img: &mut Mosaic, models: [Option<Radial>; 2]) {
    let (w, h) = (img.w, img.h);
    if w < 4 || h < 4 {
        return;
    }
    // The frame's centre and corner distance, in sensor samples.
    let (cx, cy) = (w as f32 - 0.5, h as f32 - 0.5);
    let inv_rd2 = 1.0 / (cx * cx + cy * cy);
    let mut line = vec![0f32; w];
    for (site, m) in [0usize, 3].into_iter().zip(models) {
        let Some(m) = m else { continue };
        let (sx, sy) = (img.sites[site].0 as f32, img.sites[site].1 as f32);
        // Rows: a sample at sensor x reads its own row at x + dx * f, which
        // is half as many lattice steps.
        for y in 0..h {
            let dy = 2.0 * y as f32 + sy - cy;
            for (x, l) in line.iter_mut().enumerate() {
                *l = img.data[img.index(site, x, y)];
            }
            for x in 0..w {
                let dx = 2.0 * x as f32 + sx - cx;
                let src = x as f32 + 0.5 * dx * m.factor((dx * dx + dy * dy) * inv_rd2);
                let fx = src.floor();
                let k = cubic(src - fx);
                let i = fx as isize;
                let at = |j: isize| line[(i + j).clamp(0, w as isize - 1) as usize];
                let v = k[0] * at(-1) + k[1] * at(0) + k[2] * at(1) + k[3] * at(2);
                let d = img.index(site, x, y);
                img.data[d] = v.max(0.0);
            }
        }
        // Columns. Rows above the current one are already rewritten, so the
        // originals wait in a ring.
        let reach = (0.5 * (m.a.abs() + m.b.abs()) * cy).ceil() as usize + 3;
        let ring_rows = reach + 1;
        let mut ring = vec![0f32; ring_rows * w];
        for y in 0..h {
            let slot = (y % ring_rows) * w;
            for x in 0..w {
                ring[slot + x] = img.data[img.index(site, x, y)];
            }
            let dy = 2.0 * y as f32 + sy - cy;
            for x in 0..w {
                let dx = 2.0 * x as f32 + sx - cx;
                let src = y as f32 + 0.5 * dy * m.factor((dx * dx + dy * dy) * inv_rd2);
                let fy = src.floor();
                let k = cubic(src - fy);
                let i = fy as isize;
                let mut v = 0.0;
                for (j, kv) in k.iter().enumerate() {
                    let yy = (i + j as isize - 1).clamp(0, h as isize - 1) as usize;
                    // Rows up to this one come from the ring (it holds the
                    // last `ring_rows`); rows below are still untouched.
                    v += kv
                        * if yy <= y {
                            ring[(yy.max(y.saturating_sub(ring_rows - 1)) % ring_rows) * w + x]
                        } else {
                            img.data[img.index(site, x, yy)]
                        };
                }
                let d = img.index(site, x, y);
                img.data[d] = v.max(0.0);
            }
        }
    }
}

/// Measure and remove lateral chromatic aberration.
pub fn correct(img: &mut Mosaic) {
    let models = estimate(img);
    apply(img, models);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A field of soft-edged random blobs, sampled at any real position.
    struct Scene(Vec<(f32, f32, f32, f32)>);
    impl Scene {
        fn new(w: usize, h: usize, n: usize) -> Self {
            let mut s = 0x2545F4914F6CDD1Du64;
            let mut next = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 40) as f32 / (1u64 << 24) as f32
            };
            Scene((0..n).map(|_| (next() * w as f32, next() * h as f32, 8.0 + 28.0 * next(), 0.1 + 0.5 * next())).collect())
        }
        fn at(&self, x: f32, y: f32) -> f32 {
            let mut v = 0.15;
            for &(bx, by, br, amp) in &self.0 {
                let d = ((x - bx) * (x - bx) + (y - by) * (y - by)).sqrt();
                // A blob with a 3 px soft edge.
                v += amp * ((br - d) / 3.0).clamp(0.0, 1.0);
            }
            v.min(0.9)
        }
    }

    const GAIN: [f32; 4] = [0.8, 1.0, 1.0, 0.6];
    /// GRBG, with a 2-sample border around the active area.
    const SITES: [(usize, usize); 4] = [(1, 0), (0, 0), (1, 1), (0, 1)];
    const PAD: usize = 2;

    struct Raw {
        data: Vec<f32>,
        w: usize,
        h: usize,
        scene: Scene,
    }

    impl Raw {
        /// A mosaic of the scene; red and blue magnified by `1 + kr`, `1 + kb`.
        fn new(w: usize, h: usize, kr: f32, kb: f32) -> Self {
            let scene = Scene::new(w, h, w * h / 3600);
            let (cx, cy) = ((w - 1) as f32 * 0.5, (h - 1) as f32 * 0.5);
            let stride = w + 2 * PAD;
            let mut data = vec![0f32; stride * (h + 2 * PAD)];
            for y in 0..h {
                for x in 0..w {
                    let site = SITES.iter().position(|&s| s == (x % 2, y % 2)).unwrap();
                    let k = [kr, 0.0, 0.0, kb][site];
                    let (dx, dy) = (x as f32 - cx, y as f32 - cy);
                    data[(y + PAD) * stride + x + PAD] = GAIN[site] * scene.at(cx + dx * (1.0 + k), cy + dy * (1.0 + k));
                }
            }
            Raw { data, w, h, scene }
        }
        fn view(&mut self) -> Mosaic<'_> {
            Mosaic { data: &mut self.data, stride: self.w + 2 * PAD, x0: PAD, y0: PAD, w: self.w / 2, h: self.h / 2, sites: SITES }
        }
        /// Mean distance of a channel's samples from the scene as green sees
        /// it, over the outer field: the fringe.
        fn fringe(&self, site: usize) -> f32 {
            let stride = self.w + 2 * PAD;
            let (mut s, mut n) = (0f32, 0f32);
            for y in (16 + SITES[site].1..self.h - 16).step_by(2) {
                for x in (16 + SITES[site].0..self.w - 16).step_by(2) {
                    let (dx, dy) = (x as f32 - self.w as f32 * 0.5, y as f32 - self.h as f32 * 0.5);
                    if (dx * dx + dy * dy).sqrt() > 0.3 * self.w as f32 {
                        s += (self.data[(y + PAD) * stride + x + PAD] / GAIN[site] - self.scene.at(x as f32, y as f32)).abs();
                        n += 1.0;
                    }
                }
            }
            s / n
        }
    }

    #[test]
    fn a_magnification_error_is_measured_and_removed() {
        // Corner distance 1080 samples: red 2.2 px out, blue 1.6 px in.
        let (kr, kb) = (0.002f32, -0.0015f32);
        let mut raw = Raw::new(1800, 1200, kr, kb);
        let [r, b] = estimate(&raw.view());
        let (r, b) = (r.expect("red fit"), b.expect("blue fit"));
        // The slide that undoes a magnification k is -k (to first order).
        assert!((r.a + r.b * 0.5 + kr).abs() < 0.0004, "red {r:?}");
        assert!((b.a + b.b * 0.5 + kb).abs() < 0.0004, "blue {b:?}");
        let before = [raw.fringe(0), raw.fringe(3)];
        apply(&mut raw.view(), [Some(r), Some(b)]);
        let after = [raw.fringe(0), raw.fringe(3)];
        assert!(after[0] < 0.4 * before[0], "red fringe {} -> {}", before[0], after[0]);
        assert!(after[1] < 0.4 * before[1], "blue fringe {} -> {}", before[1], after[1]);
    }

    #[test]
    fn an_aligned_frame_is_left_alone() {
        let mut raw = Raw::new(1800, 1200, 0.0, 0.0);
        assert_eq!(estimate(&raw.view()), [None, None]);
        let before = raw.data.clone();
        correct(&mut raw.view());
        assert_eq!(raw.data, before);
    }

    #[test]
    fn a_flat_or_tiny_frame_is_left_alone() {
        let mut flat = Raw::new(1400, 1000, 0.0, 0.0);
        flat.data.fill(0.3);
        assert_eq!(estimate(&flat.view()), [None, None]);
        let mut tiny = Raw::new(240, 180, 0.004, 0.0);
        assert_eq!(estimate(&tiny.view()), [None, None]);
    }

    #[test]
    fn the_resample_keeps_green_the_border_and_a_flat_field() {
        let mut raw = Raw::new(600, 400, 0.0, 0.0);
        let stride = raw.w + 2 * PAD;
        for (i, v) in raw.data.iter_mut().enumerate() {
            let (x, y) = (i % stride, i / stride);
            let inside = (PAD..PAD + 600).contains(&x) && (PAD..PAD + 400).contains(&y);
            *v = if !inside {
                0.05
            } else {
                match SITES.iter().position(|&s| s == ((x - PAD) % 2, (y - PAD) % 2)).unwrap() {
                    0 => 0.31,
                    3 => 0.23,
                    _ => 0.4 + 0.001 * (x % 7) as f32,
                }
            };
        }
        let before = raw.data.clone();
        apply(&mut raw.view(), [Some(Radial { a: 0.004, b: -0.002 }), Some(Radial { a: -0.003, b: 0.001 })]);
        for (i, (a, b)) in raw.data.iter().zip(&before).enumerate() {
            let (x, y) = (i % stride, i / stride);
            let inside = (PAD..PAD + 600).contains(&x) && (PAD..PAD + 400).contains(&y);
            let colour = inside && matches!(SITES.iter().position(|&s| s == ((x - PAD) % 2, (y - PAD) % 2)), Some(0 | 3));
            if colour {
                assert!((a - b).abs() < 1e-5, "flat field moved at {x},{y}: {b} -> {a}");
            } else {
                assert_eq!(a, b, "green or border changed at {x},{y}");
            }
        }
    }
}
