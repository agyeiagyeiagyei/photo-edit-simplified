//! Smart denoise: noise-adaptive, multi-scale (à-trous wavelet) shrinkage
//! over a luma / colour-difference split (#53).
//!
//! High-ISO noise from a demosaiced sensor is not white: next to the
//! pixel-level speckle it carries coarse mottle, luma blotches and above all
//! colour blotches 4 to 32 px across. Those coarse bands are what show at the
//! sizes a photo is viewed at. The frame is split into wavelet bands of 1, 2,
//! 4, 8 and 16 px (a B3-spline à-trous transform, each band the difference of
//! two successive smoothings), and each band is shrunk on its own.
//!
//! **Noise is measured, not assumed.** Each band's noise deviation comes from
//! the photo itself: the median absolute coefficient over its flattest blocks.
//! So the thresholds follow the ISO, the exposure and the tone curve that made
//! this frame, and a slider value means the same relative clean-up on an
//! ISO 400 and an ISO 6400 file. The ISO sets only the starting value
//! ([`crate::rawops::denoise_start_for_iso`]).
//!
//! **Detail is kept by local energy.** A coefficient is scaled by
//! `1 - (k * sigma)^2 / E`, with `E` the band's mean square over the 8 taps
//! around it at that band's spacing (not itself, so a lone noise spike can't
//! vouch for itself): an edge or a textured patch stands well
//! above the noise and passes, a flat patch is noise and goes. Luma uses the
//! slider's `k`. Colour uses a stronger `k` that is reached early on the
//! slider, since colour noise is never wanted and colour detail is coarse;
//! a clear luma edge at the same place lends its energy to the colour bands,
//! so a thin white beak against a blue wall keeps its colour.
//!
//! **Texture is told from noise by the next band.** Low-contrast texture
//! (feathers, foliage) stands little above the noise in its own band, and a
//! strong setting would take it with the noise. Sensor noise halves from one
//! band to the next coarser one while texture does not, so where a band and
//! the band above it both show structure, the band's threshold is lowered.
//!
//! **Only the removed noise is subtracted.** The bands' removed parts are
//! accumulated and taken off the 16-bit input, so a pixel the filter leaves
//! alone comes back bit for bit and the tone is untouched.
//!
//! The frame is processed in tiles with an apron that covers every tap a
//! tile's pixels depend on, so the result is the whole-frame result; band
//! scales are in sensor pixels, so a preview at 1/p of the sensor resolution
//! uses the bands the export's coarse bands land on.

/// B3-spline smoothing taps.
const B3: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
/// Coarsest band, in sensor pixels: 16 px bands, on 32 px of support.
const SENSOR_SCALE: f32 = 32.0;
const MAX_LEVELS: usize = 5;
/// Luma threshold multiplier at full strength; the slider is linear in it.
const KY_MAX: f32 = 2.0;
/// Colour threshold multiplier, reached at `KC_FULL_AT` of the slider.
const KC: f32 = 2.5;
const KC_FULL_AT: f32 = 0.2;
/// A luma band counts as an edge for the colour bands above this multiple of
/// its noise deviation.
const EDGE_T: f32 = 2.0;
/// Upper bound on a band's noise relative to the band below it.
const NOISE_DECAY: f32 = 0.7;
/// Noise is measured over blocks this size, the flattest `FLAT_SHARE` of them,
/// at most `MAX_SAMPLE_BLOCKS` spread over that share.
const BLOCK: usize = 32;
const FLAT_SHARE: f32 = 0.2;
const MAX_SAMPLE_BLOCKS: usize = 64;
/// Output tile edge (the apron is added around it).
const TILE: usize = 1024;

/// Capture sharpening: the gain on the kept luma detail of the 1, 2 and 4 px
/// sensor bands at full strength. Fine detail takes the most; the 4 px band
/// carries the edge contrast a lens and the demosaic soften.
const SHARP_GAIN: [f32; 3] = [SHARP_G0, SHARP_G1, SHARP_G2];
const SHARP_G0: f32 = 2.0;
const SHARP_G1: f32 = 3.8;
const SHARP_G2: f32 = 3.2;
/// A band is sharpened where its local energy stands this many noise
/// variances up (`SHARP_E0` none, `SHARP_E1` full), and the next coarser
/// band's does too: flat-area noise has an energy of one variance, give or
/// take a half, and where noise does cluster in one band it does not in the
/// next, while an edge or a texture shows in both.
const SHARP_E0: f32 = 3.0;
const SHARP_E1: f32 = 9.0;
/// With no measurable noise (a clean or tiny frame) the gate still needs a
/// floor, in 8-bit codes, so dither and quantisation are not sharpened.
const SHARP_SIGMA_FLOOR: f32 = 0.4;
/// Share of the gain a band keeps where the coarser band shows nothing: a
/// fine regular texture lives in one band.
const SHARP_LONE: f32 = 0.3;
/// Halo limit, in 8-bit codes: the added contrast at a pixel approaches this
/// and never passes it, so fine texture takes the full gain and a hard edge
/// does not grow a bright and dark rim.
const SHARP_LIMIT: f32 = 30.0;

/// Texture: the gain on the kept luma detail of the 4, 8 and 16 px sensor
/// bands at +100, the mid-size detail between what Sharpening lifts and what
/// contrast moves (bark, fabric, feathers, skin). At -100 the bands lose the
/// `TEXTURE_SMOOTH` share of it instead; the halo limit then leaves a hard
/// edge most of its contrast.
const TEXTURE_GAIN: [f32; 5] = [0.0, 0.0, TEX_G2, TEX_G3, TEX_G4];
const TEX_G2: f32 = 1.0;
const TEX_G3: f32 = 2.5;
const TEX_G4: f32 = 2.5;
const TEXTURE_SMOOTH: [f32; 5] = [0.0, 0.0, 0.35, 0.75, 0.75];
/// Texture protection: where a band and the next coarser one both show
/// structure, the band's noise threshold falls by this share. Low-contrast texture (feathers,
/// foliage) sits close to the noise in its own band and a strong denoise
/// would take it for noise; the coarser band, where the noise is half as
/// strong, tells the two apart.
const PROTECT: f32 = 0.9;

#[derive(Clone, Copy, Debug)]
struct Params {
    ky: f32,
    kc: f32,
    /// Gain on the kept luma detail per frame band: Sharpening and Texture
    /// together. Negative smooths.
    gain: [f32; MAX_LEVELS],
    protect: f32,
    levels: usize,
}

fn params(strength: f32, sharpen: f32, texture: f32, px_scale: f32, w: usize, h: usize) -> Params {
    let t = strength.clamp(0.0, 1.0);
    let p = px_scale.max(1.0);
    // Every dilated tap must stay inside the frame (half-sample reflection).
    let by_size = ((w.min(h) as f32 / 4.0).log2().floor().max(0.0)) as usize;
    let mut levels = if t > 0.0 { (SENSOR_SCALE / p).log2().round().max(1.0) as usize } else { 0 };
    // Frame band j sits at sensor scale p * 2^j: it takes the gain of the
    // sensor band it lands on (between two, a blend), so a preview sharpens
    // the detail the export does.
    let mut gain = [0f32; MAX_LEVELS];
    let a = sharpen.clamp(0.0, 1.0);
    let x = texture.clamp(-1.0, 1.0);
    if a > 0.0 || x != 0.0 {
        for (j, g) in gain.iter_mut().enumerate() {
            let s = p.log2() + j as f32;
            let (i, f) = (s.floor() as usize, s.fract());
            let at = |t: &[f32], i: usize| t.get(i).copied().unwrap_or(0.0);
            let mix = |t: &[f32]| at(t, i) * (1.0 - f) + at(t, i + 1) * f;
            *g = a * mix(&SHARP_GAIN) + x * mix(if x < 0.0 { &TEXTURE_SMOOTH } else { &TEXTURE_GAIN });
            if *g != 0.0 {
                // One band past the last boosted one, for its gate.
                levels = levels.max(j + 2);
            }
        }
    }
    let levels = levels.min(by_size).min(MAX_LEVELS);
    gain[levels..].fill(0.0);
    Params { ky: KY_MAX * t, kc: KC * (t / KC_FULL_AT).min(1.0), gain, protect: PROTECT, levels }
}

/// Pixels a band-`levels` result depends on, on each side: the smoothing
/// taps of every level plus the energy taps of the coarsest band and the
/// taps that smooth that energy for the sharpening gate.
fn apron(levels: usize) -> usize {
    if levels == 0 {
        return 0;
    }
    (2 << levels) - 2 + (1 << levels)
}

/// Half-sample symmetric reflection into `0..n`.
#[inline]
fn refl(i: isize, n: isize) -> usize {
    let mut i = i;
    loop {
        if i < 0 {
            i = -i - 1;
        } else if i >= n {
            i = 2 * n - i - 1;
        } else {
            return i as usize;
        }
    }
}

/// Separable filter with taps `k` at spacing `d` (centred), reflecting at the
/// plane's edges. `tmp` holds the horizontal pass.
fn sep_filter(src: &[f32], dst: &mut [f32], tmp: &mut [f32], w: usize, h: usize, k: &[f32], d: usize) {
    let r = k.len() / 2;
    let reach = r * d;
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        let out = &mut tmp[y * w..(y + 1) * w];
        if w > 2 * reach {
            out[reach..w - reach].fill(0.0);
            for (i, &kv) in k.iter().enumerate() {
                let off = i * d;
                for (o, &v) in out[reach..w - reach].iter_mut().zip(&row[off..off + w - 2 * reach]) {
                    *o += kv * v;
                }
            }
        }
        // The edge columns (or every column of a narrow plane) reflect.
        let (a, b) = if w > 2 * reach { (reach, w - reach) } else { (w, w) };
        for x in (0..a).chain(b..w) {
            let mut s = 0.0;
            for (i, &kv) in k.iter().enumerate() {
                s += kv * row[refl(x as isize + (i as isize - r as isize) * d as isize, w as isize)];
            }
            out[x] = s;
        }
    }
    for y in 0..h {
        let out = &mut dst[y * w..(y + 1) * w];
        out.fill(0.0);
        for (i, &kv) in k.iter().enumerate() {
            let sy = refl(y as isize + (i as isize - r as isize) * d as isize, h as isize);
            for (o, &v) in out.iter_mut().zip(&tmp[sy * w..(sy + 1) * w]) {
                *o += kv * v;
            }
        }
    }
}

/// Planes of a region: Rec.601 luma and the B-Y, R-Y differences, in 8-bit
/// code units.
fn load_region(buf: &[u16], w: usize, x0: usize, y0: usize, rw: usize, rh: usize) -> [Vec<f32>; 3] {
    let n = rw * rh;
    let (mut y, mut cb, mut cr) = (vec![0f32; n], vec![0f32; n], vec![0f32; n]);
    for ry in 0..rh {
        let src = &buf[((y0 + ry) * w + x0) * 4..((y0 + ry) * w + x0 + rw) * 4];
        for (rx, px) in src.chunks_exact(4).enumerate() {
            let (r, g, b) = (px[0] as f32 / 257.0, px[1] as f32 / 257.0, px[2] as f32 / 257.0);
            let l = 0.299 * r + 0.587 * g + 0.114 * b;
            let i = ry * rw + rx;
            y[i] = l;
            cb[i] = b - l;
            cr[i] = r - l;
        }
    }
    [y, cb, cr]
}

/// Walk the bands of a region: `visit(level, band_planes)` sees the three
/// channels' band `level`, and may shrink them. Returns nothing; the caller
/// keeps what it needs from the visits.
/// Channels with an empty plane are skipped (luma-only sharpening).
fn bands(mut c: [Vec<f32>; 3], w: usize, h: usize, levels: usize, visit: &mut dyn FnMut(usize, &mut [Vec<f32>; 3])) {
    let n = w * h;
    let mut s = [0, 1, 2].map(|ch| vec![0f32; c[ch].len()]);
    let mut tmp = vec![0f32; n];
    for j in 0..levels {
        for ch in 0..3 {
            if c[ch].is_empty() {
                continue;
            }
            sep_filter(&c[ch], &mut s[ch], &mut tmp, w, h, &B3, 1 << j);
            // c becomes the band (c - s); s carries on as the next level.
            for (cv, &sv) in c[ch].iter_mut().zip(&s[ch]) {
                *cv -= sv;
            }
        }
        visit(j, &mut c);
        std::mem::swap(&mut c, &mut s);
    }
}

/// Mean of `b`^2 over the 3x3 taps at spacing `d`.
/// Mean of `b`^2 over the 8 taps around each coefficient at spacing `d`,
/// leaving the coefficient itself out: an isolated noise spike then has only
/// its quiet neighbours to vouch for it and goes, while an edge or texture is
/// carried by its neighbours along it.
fn energy(b: &[f32], out: &mut [f32], sq: &mut [f32], tmp: &mut [f32], w: usize, h: usize, d: usize) {
    for (q, &v) in sq.iter_mut().zip(b) {
        *q = v * v;
    }
    sep_filter(sq, out, tmp, w, h, &[1.0; 3], d);
    for (o, &q) in out.iter_mut().zip(sq.iter()) {
        *o = (*o - q) * (1.0 / 8.0);
    }
}

/// The `k`-th smallest value of `v`, reordering it: a three-way quickselect,
/// so the worker carries no standard-library sort for two small selections.
fn kth_smallest(v: &mut [f32], k: usize) -> f32 {
    let (mut lo, mut hi) = (0, v.len());
    loop {
        if hi - lo <= 1 {
            return v[lo];
        }
        let (a, b, c) = (v[lo], v[lo + (hi - lo) / 2], v[hi - 1]);
        let p = a.min(b).max(a.max(b).min(c));
        // v[lo..lt] < p, v[lt..gt] == p, v[gt..hi] > p.
        let (mut lt, mut i, mut gt) = (lo, lo, hi);
        while i < gt {
            if v[i] < p {
                v.swap(lt, i);
                lt += 1;
                i += 1;
            } else if v[i] > p {
                gt -= 1;
                v.swap(i, gt);
            } else {
                i += 1;
            }
        }
        if k < lt {
            hi = lt;
        } else if k >= gt {
            lo = gt;
        } else {
            return p;
        }
    }
}

/// Noise deviation per band and channel, from the flattest blocks.
fn estimate_noise(buf: &[u16], w: usize, h: usize, levels: usize) -> Vec<[f32; 3]> {
    let (bw, bh) = (w / BLOCK, h / BLOCK);
    // Flatness: variance of 4x4 means within the block (noise averaged down,
    // structure kept). Near-black and near-white blocks hide the noise.
    let mut blocks: Vec<(f32, usize, usize)> = Vec::new();
    const CELLS: usize = BLOCK / 4;
    for by in 0..bh {
        for bx in 0..bw {
            // Sums of the 4x4 cells, one block row at a time.
            let mut cell = [0f32; CELLS * CELLS];
            for yy in 0..BLOCK {
                let start = ((by * BLOCK + yy) * w + bx * BLOCK) * 4;
                let sums = &mut cell[yy / 4 * CELLS..(yy / 4 + 1) * CELLS];
                for (xx, p) in buf[start..start + BLOCK * 4].chunks_exact(4).enumerate() {
                    sums[xx / 4] += 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
                }
            }
            let (mut s, mut s2) = (0f32, 0f32);
            for &c in &cell {
                let m = c / (16.0 * 257.0);
                s += m;
                s2 += m * m;
            }
            let n = (CELLS * CELLS) as f32;
            let mean = s / n;
            if (12.0..=243.0).contains(&mean) {
                blocks.push((s2 / n - mean * mean, bx, by));
            }
        }
    }
    let mut sig = vec![[0f32; 3]; levels];
    if blocks.is_empty() {
        return sig;
    }
    // The flattest share, kept in scan order so the sample spreads over the
    // frame: everything below the share's threshold, then ties up to it.
    let flat = ((blocks.len() as f32 * FLAT_SHARE).ceil() as usize).clamp(1, blocks.len());
    let thr = kth_smallest(&mut blocks.iter().map(|b| b.0).collect::<Vec<_>>(), flat - 1);
    let mut ties = flat - blocks.iter().filter(|b| b.0 < thr).count();
    blocks.retain(|b| {
        b.0 < thr
            || (b.0 == thr && ties > 0 && {
                ties -= 1;
                true
            })
    });
    let take = flat.min(MAX_SAMPLE_BLOCKS);
    let a = apron(levels);
    let mut coef: Vec<[Vec<f32>; 3]> = (0..levels).map(|_| [Vec::new(), Vec::new(), Vec::new()]).collect();
    for i in 0..take {
        let (_, bx, by) = blocks[i * flat / take];
        let (cx0, cy0) = (bx * BLOCK, by * BLOCK);
        let x0 = cx0.saturating_sub(a);
        let y0 = cy0.saturating_sub(a);
        let x1 = (cx0 + BLOCK + a).min(w);
        let y1 = (cy0 + BLOCK + a).min(h);
        let (rw, rh) = (x1 - x0, y1 - y0);
        let planes = load_region(buf, w, x0, y0, rw, rh);
        bands(planes, rw, rh, levels, &mut |j, b| {
            for ch in 0..3 {
                for yy in cy0 - y0..cy0 - y0 + BLOCK {
                    let row = &b[ch][yy * rw + cx0 - x0..yy * rw + cx0 - x0 + BLOCK];
                    coef[j][ch].extend(row.iter().map(|v| v.abs()));
                }
            }
        });
    }
    for (j, per) in coef.iter_mut().enumerate() {
        for ch in 0..3 {
            let v = &mut per[ch];
            let mid = v.len() / 2;
            let measured = 1.4826 * kth_smallest(v, mid);
            // A coarse band of a flat block still sees edges up to its support
            // away, so its MAD overstates the noise. Sensor noise falls about
            // 0.5-0.6x per band (measured on an ISO 3200 ARW); cap each band at
            // `NOISE_DECAY` of the band below.
            sig[j][ch] = if j == 0 { measured } else { measured.min(NOISE_DECAY * sig[j - 1][ch]) };
        }
    }
    sig
}

/// Settle luma band `j` once the next coarser band's gate is known (`None`
/// for the coarsest band): shrink it into `rem` and put its boost in `add`.
/// `band` holds its coefficients, their local energy and that energy over a
/// wider patch in noise variances; `coarse` is the latter for the next band.
fn settle(rem: &mut [f32], add: &mut [f32], p: Params, sig: &[[f32; 3]], j: usize, band: [&[f32]; 3], coarse: Option<&[f32]>) {
    let [pb, pe, pg] = band;
    let ty = (p.ky * sig[j][0]) * (p.ky * sig[j][0]);
    let g = p.gain[j];
    if ty <= 0.0 && g == 0.0 {
        return;
    }
    // 0..1 shares of "this stands clear of the noise".
    let gate = |v: f32| ((v - SHARP_E0) * (1.0 / (SHARP_E1 - SHARP_E0))).clamp(0.0, 1.0);
    for i in 0..rem.len() {
        let c = coarse.map(|c| c[i]);
        // Structure in this band and the coarser one lowers the threshold.
        let keep = if ty > 0.0 { p.protect * gate(pg[i]) * c.map_or(0.0, gate) } else { 0.0 };
        let cut = (ty * (1.0 - keep) / pe[i].max(1e-12)).min(1.0);
        let bv = pb[i];
        rem[i] += bv * cut;
        if g > 0.0 {
            // A boost needs this band clear of its noise, and the coarser
            // one too: structure shows in both, noise in the finer alone.
            // The coarsest band has no coarser one to ask.
            add[i] += g * gate(pg[i]) * bv * (1.0 - cut) * c.map_or(1.0, |c| SHARP_LONE + (1.0 - SHARP_LONE) * gate(c));
        } else if g < 0.0 {
            add[i] += g * bv * (1.0 - cut);
        }
    }
}

/// Shrink a region's bands and boost (or smooth) the kept luma detail.
/// Returns the part to take off each channel: the removed noise, less the
/// added contrast.
fn removed(mut planes: [Vec<f32>; 3], w: usize, h: usize, p: Params, sig: &[[f32; 3]]) -> [Vec<f32>; 3] {
    let n = w * h;
    let colour = p.kc > 0.0;
    if !colour {
        planes[1] = Vec::new();
        planes[2] = Vec::new();
    }
    let cn = if colour { n } else { 0 };
    let mut rem = [vec![0f32; n], vec![0f32; cn], vec![0f32; cn]];
    let boosting = p.gain.iter().any(|&g| g != 0.0);
    let mut add = vec![0f32; if boosting { n } else { 0 }];
    let (mut ey, mut ec, mut sq, mut tmp) = (vec![0f32; n], vec![0f32; cn], vec![0f32; n], vec![0f32; n]);
    // A luma band is settled once the next coarser band's gate is known:
    // until then its coefficients, energy and own gate wait here.
    let (mut eg, mut pb, mut pe, mut pg) = (vec![0f32; n], vec![0f32; n], vec![0f32; n], vec![0f32; n]);
    bands(planes, w, h, p.levels, &mut |j, b| {
        let d = 1 << j;
        let [sy, scb, scr] = sig[j];
        energy(&b[0], &mut ey, &mut sq, &mut tmp, w, h, d);
        // The gates read the energy over a wider patch than the shrinkage
        // does, in noise variances.
        let var = sy.max(SHARP_SIGMA_FLOOR) * sy.max(SHARP_SIGMA_FLOOR);
        sep_filter(&ey, &mut eg, &mut tmp, w, h, &[0.25, 0.5, 0.25], d);
        for v in eg.iter_mut() {
            *v /= var;
        }
        if j > 0 {
            settle(&mut rem[0], &mut add, p, sig, j - 1, [&pb, &pe, &pg], Some(&eg));
        }
        if colour {
            // Where luma has a clear edge, the colour bands keep at least the
            // share an edge-only luma gain would keep there.
            let te = (EDGE_T * sy) * (EDGE_T * sy);
            for (ch, sc) in [(1usize, scb), (2, scr)] {
                energy(&b[ch], &mut ec, &mut sq, &mut tmp, w, h, d);
                let tc = (p.kc * sc) * (p.kc * sc);
                for (((r, &bv), &e), &eyv) in rem[ch].iter_mut().zip(&b[ch]).zip(&ec).zip(&ey) {
                    let cut = (tc / e.max(1e-12)).min(1.0);
                    let edge_cut = (te / eyv.max(1e-12)).min(1.0);
                    let edge_cut = edge_cut * edge_cut;
                    *r += bv * cut.min(edge_cut);
                }
            }
        }
        // The band plane is scratch for the next level from here on.
        std::mem::swap(&mut b[0], &mut pb);
        std::mem::swap(&mut ey, &mut pe);
        std::mem::swap(&mut eg, &mut pg);
    });
    settle(&mut rem[0], &mut add, p, sig, p.levels - 1, [&pb, &pe, &pg], None);
    for (r, &a) in rem[0].iter_mut().zip(&add) {
        *r -= a * SHARP_LIMIT / (SHARP_LIMIT + a.abs());
    }
    rem
}

fn denoise_tiled(buf: &mut [u16], w: usize, h: usize, p: Params, tile: usize) {
    let sig = estimate_noise(buf, w, h, p.levels);
    let boosting = p.gain.iter().any(|&g| g != 0.0);
    if !boosting && sig.iter().all(|s| s.iter().all(|&v| v <= 0.0)) {
        return;
    }
    let a = apron(p.levels);
    // Tiles read their apron from the frame as it was: each row of tiles
    // takes its source rows from `src`, a copy holding rows `src_y0..`.
    // Rows above the current tile row were written already, so they come
    // from the previous copy, which still holds them unfiltered.
    let row = w * 4;
    let (mut src, mut src_y0): (Vec<u16>, usize) = (Vec::new(), 0);
    let mut ty0 = 0;
    while ty0 < h {
        let ty1 = (ty0 + tile).min(h);
        let (y0, y1) = (ty0.saturating_sub(a), (ty1 + a).min(h));
        let mut band = Vec::with_capacity((y1 - y0) * row);
        for yy in y0..y1 {
            let line = if yy < ty0 { &src[(yy - src_y0) * row..(yy - src_y0 + 1) * row] } else { &buf[yy * row..(yy + 1) * row] };
            band.extend_from_slice(line);
        }
        (src, src_y0) = (band, y0);
        let mut tx0 = 0;
        while tx0 < w {
            let tx1 = (tx0 + tile).min(w);
            let (x0, x1) = (tx0.saturating_sub(a), (tx1 + a).min(w));
            let (rw, rh) = (x1 - x0, y1 - y0);
            let planes = load_region(&src, w, x0, 0, rw, rh);
            let [dy, dcb, dcr] = removed(planes, rw, rh, p, &sig);
            for yy in ty0..ty1 {
                for xx in tx0..tx1 {
                    let i = (yy - y0) * rw + (xx - x0);
                    // Removed Y/Cb/Cr back to RGB (the split is linear).
                    let ly = dy[i];
                    let (lb, lr) = if dcb.is_empty() { (0.0, 0.0) } else { (dcb[i], dcr[i]) };
                    let r = lr + ly;
                    let b = lb + ly;
                    let g = (ly - 0.299 * r - 0.114 * b) / 0.587;
                    let px = &mut buf[(yy * w + xx) * 4..(yy * w + xx) * 4 + 3];
                    for (v, dv) in px.iter_mut().zip([r, g, b]) {
                        let step = (dv * 257.0).round();
                        if step != 0.0 {
                            *v = (*v as f32 - step).clamp(0.0, 65535.0) as u16;
                        }
                    }
                }
            }
            tx0 = tx1;
        }
        ty0 = ty1;
    }
}

/// Denoise a 16-bit sRGB RGBA frame in place. `strength` 0..=1 (0 is a
/// no-op); `px_scale` is sensor pixels per frame pixel (1 for a full-size
/// export, larger for a downscaled preview).
pub fn smart_denoise_wide(buf: &mut [u16], w: usize, h: usize, strength: f32, px_scale: f32) {
    detail_wide(buf, w, h, strength, 0.0, 0.0, px_scale);
}

/// Smart denoise, capture sharpening and Texture in one pass over the same
/// bands: each luma band's noise is shrunk as above, what is kept of the 1, 2
/// and 4 px sensor bands is given more contrast (`sharpen`), and what is kept
/// of the 4 to 16 px bands is lifted or smoothed (`texture`).
///
/// **A boost follows the measured noise.** A band is boosted only where its
/// local energy stands clear of the band's noise, so the grain of a flat
/// wall is not sharpened along with a feather, at any ISO. The added contrast
/// is soft-limited per pixel, which keeps hard edges free of halos.
///
/// `strength` and `sharpen` are 0..=1, `texture` is -1..=1; all 0 is a no-op.
pub fn detail_wide(buf: &mut [u16], w: usize, h: usize, strength: f32, sharpen: f32, texture: f32, px_scale: f32) {
    if (strength <= 0.0 && sharpen <= 0.0 && texture == 0.0) || w == 0 || h == 0 || buf.len() < w * h * 4 {
        return;
    }
    let p = params(strength, sharpen, texture, px_scale, w, h);
    if p.levels == 0 {
        return;
    }
    denoise_tiled(buf, w, h, p, TILE);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random normal noise.
    struct Rng(u64);
    impl Rng {
        fn unit(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 + 0.5) / (1u64 << 24) as f32
        }
        fn normal(&mut self) -> f32 {
            let (a, b) = (self.unit(), self.unit());
            (-2.0 * a.ln()).sqrt() * (std::f32::consts::TAU * b).cos()
        }
    }

    /// A frame from an 8-bit RGB function plus luma noise `sl` and colour
    /// noise `sc` (in 8-bit codes), the colour noise box-blurred over `blot`
    /// px like demosaiced colour mottle.
    fn frame(w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3], sl: f32, sc: f32, blot: usize, seed: u64) -> Vec<u16> {
        let mut rng = Rng(seed);
        let n = w * h;
        let ln: Vec<f32> = (0..n).map(|_| rng.normal() * sl).collect();
        let mut cn: [Vec<f32>; 2] = [(0..n).map(|_| rng.normal()).collect(), (0..n).map(|_| rng.normal()).collect()];
        for c in cn.iter_mut() {
            let src = c.clone();
            let r = blot / 2;
            for y in 0..h {
                for x in 0..w {
                    let (mut s, mut k) = (0.0, 0.0);
                    for yy in y.saturating_sub(r)..(y + r + 1).min(h) {
                        for xx in x.saturating_sub(r)..(x + r + 1).min(w) {
                            s += src[yy * w + xx];
                            k += 1.0;
                        }
                    }
                    // Rescale so the blotchy noise keeps deviation sc.
                    c[y * w + x] = s / k * sc * (k as f32).sqrt();
                }
            }
        }
        let mut out = vec![65535u16; n * 4];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let [r, g, b] = f(x, y);
                let (l, cb, cr) = (ln[i], cn[0][i], cn[1][i]);
                let rr = r + l + cr;
                let bb = b + l + cb;
                let gg = g + l - (0.299 * cr + 0.114 * cb) / 0.587;
                for (c, v) in [rr, gg, bb].into_iter().enumerate() {
                    out[i * 4 + c] = (v * 257.0).round().clamp(0.0, 65535.0) as u16;
                }
            }
        }
        out
    }

    fn planes(buf: &[u16], w: usize, h: usize) -> [Vec<f32>; 3] {
        load_region(buf, w, 0, 0, w, h)
    }

    /// Deviation of the k x k box means of `v` inside a window.
    fn sd_at(v: &[f32], w: usize, (x0, y0, x1, y1): (usize, usize, usize, usize), k: usize) -> f32 {
        let mut m = Vec::new();
        let mut y = y0;
        while y + k <= y1 {
            let mut x = x0;
            while x + k <= x1 {
                let mut s = 0.0;
                for yy in y..y + k {
                    for xx in x..x + k {
                        s += v[yy * w + xx];
                    }
                }
                m.push(s / (k * k) as f32);
                x += k;
            }
            y += k;
        }
        let mean = m.iter().sum::<f32>() / m.len() as f32;
        (m.iter().map(|a| (a - mean) * (a - mean)).sum::<f32>() / m.len() as f32).sqrt()
    }

    const GREY: fn(usize, usize) -> [f32; 3] = |_, _| [120.0, 130.0, 125.0];

    #[test]
    fn kth_smallest_matches_a_sort() {
        let mut rng = Rng(7);
        for n in [1usize, 2, 3, 10, 101, 1000] {
            // A third of the values tie, as zero coefficients do.
            let v: Vec<f32> = (0..n).map(|i| if i % 3 == 0 { 1.0 } else { rng.normal() }).collect();
            let mut sorted = v.clone();
            sorted.sort_by(f32::total_cmp);
            for k in [0, n / 3, n / 2, n - 1] {
                assert_eq!(kth_smallest(&mut v.clone(), k), sorted[k], "n {n}, k {k}");
            }
        }
    }

    #[test]
    fn zero_strength_is_a_noop() {
        let mut a = frame(96, 64, GREY, 6.0, 8.0, 5, 1);
        let b = a.clone();
        smart_denoise_wide(&mut a, 96, 64, 0.0, 1.0);
        assert_eq!(a, b);
    }

    #[test]
    fn a_noise_free_frame_is_unchanged() {
        // A smooth gradient of a saturated colour and a hard edge: no band
        // stands above a noise of zero, so nothing is removed.
        let f = |x: usize, y: usize| if x < 60 { [200.0, 40.0 + y as f32, 30.0] } else { [20.0, 90.0, 210.0 - x as f32 * 0.3] };
        let mut a = frame(128, 96, f, 0.0, 0.0, 1, 2);
        let b = a.clone();
        smart_denoise_wide(&mut a, 128, 96, 1.0, 1.0);
        assert_eq!(a, b);
    }

    #[test]
    fn colour_mottle_goes_at_every_scale() {
        // ISO-3200-like: luma 6, colour 10 with 5 px blotches.
        let (w, h) = (256, 192);
        let mut a = frame(w, h, GREY, 6.0, 10.0, 5, 3);
        let before = planes(&a, w, h);
        smart_denoise_wide(&mut a, w, h, 0.25, 1.0);
        let after = planes(&a, w, h);
        let win = (24, 24, w - 24, h - 24);
        for k in [1usize, 4, 8] {
            for ch in 1..3 {
                let (b, a) = (sd_at(&before[ch], w, win, k), sd_at(&after[ch], w, win, k));
                assert!(a < 0.25 * b, "colour {ch} at {k} px: {a:.2} of {b:.2}");
            }
        }
    }

    #[test]
    fn luma_noise_falls_with_the_slider_at_fine_and_coarse_scales() {
        let (w, h) = (256, 192);
        let src = frame(w, h, GREY, 6.0, 4.0, 3, 4);
        let base = planes(&src, w, h);
        let win = (24, 24, w - 24, h - 24);
        let mut last = [f32::MAX; 2];
        for s in [0.25f32, 0.5, 1.0] {
            let mut a = src.clone();
            smart_denoise_wide(&mut a, w, h, s, 1.0);
            let p = planes(&a, w, h);
            let now = [sd_at(&p[0], w, win, 1), sd_at(&p[0], w, win, 4)];
            assert!(now[0] < last[0] && now[1] < last[1], "strength {s}: {now:?} not below {last:?}");
            last = now;
        }
        assert!(last[0] < 0.4 * sd_at(&base[0], w, win, 1), "full strength fine luma {}", last[0]);
        assert!(last[1] < 0.4 * sd_at(&base[0], w, win, 4), "full strength 4 px luma {}", last[1]);
    }

    #[test]
    fn the_same_slider_cleans_any_noise_level_alike() {
        // Thresholds come from the measured noise, so a quiet and a noisy
        // frame lose the same share of it.
        let (w, h) = (256, 192);
        let win = (24, 24, w - 24, h - 24);
        let mut share = Vec::new();
        for sl in [2.0f32, 8.0] {
            let mut a = frame(w, h, GREY, sl, sl, 3, 5);
            let b = planes(&a, w, h)[0].clone();
            smart_denoise_wide(&mut a, w, h, 0.5, 1.0);
            share.push(sd_at(&planes(&a, w, h)[0], w, win, 1) / sd_at(&b, w, win, 1));
        }
        assert!((share[0] - share[1]).abs() < 0.08, "kept share {share:?}");
    }

    #[test]
    fn edges_and_tone_are_kept() {
        // A 60-code luma step under ISO-3200-like noise: both sides keep their
        // level and the step stays as sharp.
        let (w, h) = (192, 128);
        let f = |x: usize, _| if x < 96 { [70.0, 80.0, 75.0] } else { [130.0, 140.0, 135.0] };
        let mut a = frame(w, h, f, 6.0, 8.0, 3, 6);
        let before = planes(&a, w, h);
        smart_denoise_wide(&mut a, w, h, 0.5, 1.0);
        let after = planes(&a, w, h);
        let col = |p: &[f32], x: usize| (16..h - 16).map(|y| p[y * w + x]).sum::<f32>() / (h - 32) as f32;
        // Against the noise-free level: one noisy column's mean is itself
        // off by about 0.6 codes.
        let truth = |c: [f32; 3]| 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
        for x in [40usize, 150] {
            let want = truth(f(x, 0));
            assert!((col(&after[0], x) - want).abs() < 0.4, "level at {x}: {} vs {want}", col(&after[0], x));
        }
        let step = |p: &[f32]| col(p, 97) - col(p, 94);
        assert!(step(&after[0]) > 0.9 * step(&before[0]), "step {} vs {}", step(&after[0]), step(&before[0]));
    }

    #[test]
    fn a_thin_white_line_keeps_its_colour_on_a_coloured_ground() {
        // The beak case: a 3 px neutral line on a cyan ground.
        let (w, h) = (160, 120);
        let f = |_, y: usize| if (58..61).contains(&y) { [215.0, 215.0, 215.0] } else { [110.0, 160.0, 165.0] };
        let mut a = frame(w, h, f, 6.0, 8.0, 3, 7);
        let before = planes(&a, w, h);
        smart_denoise_wide(&mut a, w, h, 0.5, 1.0);
        let after = planes(&a, w, h);
        let mean = |p: &[f32]| (20..w - 20).flat_map(|x| (58..61).map(move |y| (x, y))).map(|(x, y)| p[y * w + x]).sum::<f32>() / ((w - 40) * 3) as f32;
        for ch in 1..3 {
            assert!((mean(&after[ch]) - mean(&before[ch])).abs() < 2.0, "colour {ch}: {} vs {}", mean(&after[ch]), mean(&before[ch]));
        }
    }

    #[test]
    fn tiles_give_the_whole_frame_result() {
        let (w, h) = (300, 220);
        let f = |x: usize, y: usize| [100.0 + (x % 37) as f32, 120.0 + (y % 23) as f32, 90.0];
        let src = frame(w, h, f, 6.0, 8.0, 3, 8);
        let p = params(0.6, 0.5, 0.4, 1.0, w, h);
        let (mut whole, mut tiled) = (src.clone(), src.clone());
        denoise_tiled(&mut whole, w, h, p, 4096);
        denoise_tiled(&mut tiled, w, h, p, 70);
        assert_eq!(whole, tiled);
    }

    #[test]
    fn a_preview_cleans_as_the_export_does() {
        // A 2x downscale halves white noise; measured noise and sensor-scaled
        // bands make the preview lose the same share as the export.
        let (w, h) = (384, 256);
        let src = frame(w, h, GREY, 8.0, 10.0, 5, 9);
        let down = |b: &[u16]| -> Vec<u16> {
            let (hw, hh) = (w / 2, h / 2);
            let mut o = vec![65535u16; hw * hh * 4];
            for y in 0..hh {
                for x in 0..hw {
                    for c in 0..3 {
                        let s: u32 = [(0, 0), (1, 0), (0, 1), (1, 1)].iter().map(|(dx, dy)| b[((2 * y + dy) * w + 2 * x + dx) * 4 + c] as u32).sum();
                        o[(y * hw + x) * 4 + c] = ((s + 2) / 4) as u16;
                    }
                }
            }
            o
        };
        let mut export = src.clone();
        smart_denoise_wide(&mut export, w, h, 0.5, 1.0);
        let export_small = down(&export);
        let mut preview = down(&src);
        smart_denoise_wide(&mut preview, w / 2, h / 2, 0.5, 2.0);
        let (hw, hh) = (w / 2, h / 2);
        let win = (12, 12, hw - 12, hh - 12);
        let (e, p) = (planes(&export_small, hw, hh), planes(&preview, hw, hh));
        for ch in 0..3 {
            let (se, sp) = (sd_at(&e[ch], hw, win, 1), sd_at(&p[ch], hw, win, 1));
            assert!(sp < se * 1.6 + 0.3 && se < sp * 1.6 + 0.3, "channel {ch}: export {se:.2} preview {sp:.2}");
        }
    }

    fn luma_row(buf: &[u16], w: usize, y: usize) -> Vec<f32> {
        (0..w).map(|x| buf[(y * w + x) * 4 + 1] as f32 / 257.0).collect()
    }

    #[test]
    fn sharpening_adds_edge_contrast_within_the_halo_limit() {
        let (w, h) = (96, 64);
        for (lo, hi) in [(96.0f32, 126.0f32), (40.0, 210.0)] {
            let f = move |x: usize, _| if x < 48 { [lo; 3] } else { [hi; 3] };
            let mut a = frame(w, h, f, 0.0, 0.0, 1, 10);
            detail_wide(&mut a, w, h, 0.0, 1.0, 0.0, 1.0);
            let row = luma_row(&a, w, 32);
            assert!(row[47] < lo - 2.0 && row[48] > hi + 2.0, "step {lo}..{hi}: {:?}", &row[44..52]);
            // Neither side over- or undershoots past the limit, and pixels
            // beyond the bands' reach are untouched.
            let (min, max) = row.iter().fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            assert!(min >= lo - SHARP_LIMIT - 0.5 && max <= hi + SHARP_LIMIT + 0.5, "step {lo}..{hi}: {min}..{max}");
            assert_eq!((row[20], row[76]), (lo, hi));
        }
    }

    #[test]
    fn sharpening_lifts_texture_and_leaves_flat_noise() {
        // Left half flat, right half 3 px stripes of 24 codes, under luma
        // noise of 5: the stripes gain contrast, the flat half's noise does not.
        let (w, h) = (256, 192);
        let f = |x: usize, _| if x >= 128 && x % 6 < 3 { [142.0; 3] } else { [118.0; 3] };
        let src = frame(w, h, f, 5.0, 0.0, 1, 11);
        let mut a = src.clone();
        detail_wide(&mut a, w, h, 0.0, 1.0, 0.0, 1.0);
        let (b, p) = (planes(&src, w, h), planes(&a, w, h));
        let flat = (16, 16, 104, h - 16);
        let (nb, na) = (sd_at(&b[0], w, flat, 1), sd_at(&p[0], w, flat, 1));
        assert!(na < 1.1 * nb, "flat noise {nb:.2} -> {na:.2}");
        let tex = (144, 16, w - 16, h - 16);
        let (tb, ta) = (sd_at(&b[0], w, tex, 1), sd_at(&p[0], w, tex, 1));
        assert!(ta > 1.3 * tb, "texture {tb:.2} -> {ta:.2}");
        // With denoise as well, the flat half ends cleaner than it started,
        // and the stripes stand out more than denoise alone leaves them.
        let (mut c, mut dn) = (src.clone(), src.clone());
        detail_wide(&mut c, w, h, 0.5, 1.0, 0.0, 1.0);
        detail_wide(&mut dn, w, h, 0.5, 0.0, 0.0, 1.0);
        let (q, dq) = (planes(&c, w, h), planes(&dn, w, h));
        assert!(sd_at(&q[0], w, flat, 1) < 0.7 * nb);
        let (td, ts) = (sd_at(&dq[0], w, tex, 1), sd_at(&q[0], w, tex, 1));
        assert!(ts > 1.15 * td, "texture after denoise {td:.2} -> {ts:.2}");
    }

    #[test]
    fn a_preview_sharpens_the_bands_the_export_does() {
        // Half size: frame band 0 is the sensor's 2 px band. An eighth: the
        // sharpened bands are below the preview's pixel, so nothing changes.
        let full = params(0.0, 1.0, 0.0, 1.0, 512, 512);
        let half = params(0.0, 1.0, 0.0, 2.0, 512, 512);
        assert_eq!((full.levels, half.levels), (4, 3));
        assert_eq!(half.gain[..2], full.gain[1..3]);
        let (w, h) = (96, 64);
        let mut a = frame(w, h, |x, _| if x < 48 { [96.0; 3] } else { [160.0; 3] }, 0.0, 0.0, 1, 12);
        let b = a.clone();
        detail_wide(&mut a, w, h, 0.0, 1.0, 0.0, 8.0);
        assert_eq!(a, b);
    }

    /// Amplitude of the `period` px sine across x in the column means of a
    /// window (the noise averages away down the columns).
    fn sine_amp(v: &[f32], w: usize, h: usize, (x0, x1): (usize, usize), period: f32) -> f32 {
        let (mut sc, mut ss) = (0.0, 0.0);
        for x in x0..x1 {
            let col = (16..h - 16).map(|y| v[y * w + x]).sum::<f32>() / (h - 32) as f32;
            let ph = std::f32::consts::TAU * x as f32 / period;
            sc += col * ph.cos();
            ss += col * ph.sin();
        }
        2.0 * (sc * sc + ss * ss).sqrt() / (x1 - x0) as f32
    }

    #[test]
    fn texture_lifts_and_smooths_mid_size_detail_and_leaves_flat_noise() {
        // Left half flat, right half ripples 12, 24 and 48 px long, 5 codes
        // each, under luma noise of 4.
        let (w, h) = (384, 192);
        let f = |x: usize, _| {
            let s = |p: f32| 5.0 * (std::f32::consts::TAU * x as f32 / p).sin();
            [118.0 + if x >= 192 { s(12.0) + s(24.0) + s(48.0) } else { 0.0 }; 3]
        };
        let src = frame(w, h, f, 4.0, 0.0, 1, 13);
        let flat = (16, 16, 160, h - 16);
        let run = |texture: f32| {
            let mut a = src.clone();
            detail_wide(&mut a, w, h, 0.0, 0.0, texture, 1.0);
            let p = planes(&a, w, h);
            (sine_amp(&p[0], w, h, (216, 360), 24.0), sd_at(&p[0], w, flat, 1))
        };
        let (base, noise) = run(0.0);
        let (up, up_noise) = run(1.0);
        let (down, down_noise) = run(-1.0);
        eprintln!("texture {base:.2} up {up:.2} down {down:.2}; noise {noise:.2} {up_noise:.2} {down_noise:.2}");
        assert!(up > 1.3 * base, "texture +100: {base:.2} -> {up:.2}");
        assert!(up_noise < 1.1 * noise, "flat noise at +100: {noise:.2} -> {up_noise:.2}");
        assert!(down < 0.75 * base, "texture -100: {base:.2} -> {down:.2}");
        assert!(down_noise <= noise, "flat noise at -100: {noise:.2} -> {down_noise:.2}");
    }

    #[test]
    fn smoothing_texture_keeps_a_hard_edge() {
        let (w, h) = (96, 64);
        let mut a = frame(w, h, |x, _| if x < 48 { [40.0; 3] } else { [210.0; 3] }, 0.0, 0.0, 1, 14);
        detail_wide(&mut a, w, h, 0.0, 0.0, -1.0, 1.0);
        let row = luma_row(&a, w, 32);
        // The step keeps most of its contrast across two pixels, and the
        // levels away from it are untouched.
        assert!(row[49] - row[46] > 0.75 * 170.0, "{:?}", &row[42..54]);
        assert_eq!((row[8], row[88]), (40.0, 210.0));
    }

    #[test]
    fn a_strong_denoise_protects_texture_and_still_cleans_the_flat_part() {
        // Left half flat, right half ripples 5 and 10 px long, 6 codes each,
        // under luma noise of 4: the fine ripple stands above the noise in
        // its band, but under the full-strength threshold.
        let (w, h) = (384, 192);
        let f = |x: usize, _| {
            let s = |p: f32| 6.0 * (std::f32::consts::TAU * x as f32 / p).sin();
            [118.0 + if x >= 192 { s(5.0) + s(10.0) } else { 0.0 }; 3]
        };
        let src = frame(w, h, f, 4.0, 0.0, 1, 15);
        let flat = (16, 16, 160, h - 16);
        let run = |protect: f32| {
            let mut a = src.clone();
            denoise_tiled(&mut a, w, h, Params { protect, ..params(1.0, 0.0, 0.0, 1.0, w, h) }, TILE);
            let p = planes(&a, w, h);
            (sine_amp(&p[0], w, h, (216, 360), 5.0), sd_at(&p[0], w, flat, 1))
        };
        let (bare, bare_noise) = run(0.0);
        let (kept, noise) = run(PROTECT);
        eprintln!("ripple {bare:.2} -> {kept:.2}; flat noise {bare_noise:.2} -> {noise:.2}");
        assert!(kept > 1.15 * bare, "fine ripple {bare:.2} -> {kept:.2} of 6");
        assert!(noise < 1.1 * bare_noise && noise < 1.5, "flat noise {bare_noise:.2} -> {noise:.2}");
    }
}
