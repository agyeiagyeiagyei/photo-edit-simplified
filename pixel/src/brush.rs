//! Selection-brush rasterization.
//!
//! A stroke is a soft-edged disc walked along the pointer path. The coverage
//! of one stroke is accumulated first (max over dabs) so overlapping dabs at
//! a soft edge do not build up: one pass over a pixel yields the same value as
//! any other. Add keeps the brighter of mask and coverage; Remove scales the
//! mask down by the coverage, so at softness 0 an Add followed by Remove over
//! the same path returns an empty mask exactly.

use std::rc::Rc;

use crate::stroke;

/// Bounding box of a stroke's coverage, in mask pixels, plus the coverage.
#[derive(Clone, PartialEq, Debug)]
pub struct StrokeMask {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

/// Axis-aligned region in mask pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    fn union(self, other: Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Rect { x, y, w: right - x, h: bottom - y }
    }
}

fn union_opt(acc: Option<Rect>, next: Rect) -> Rect {
    match acc {
        Some(acc) => acc.union(next),
        None => next,
    }
}

/// Bounds `stamp` would touch for a dab, clipped to the mask.
fn dab_bounds(cx: f32, cy: f32, radius: f32, w: usize, h: usize) -> Option<Rect> {
    let x0 = ((cx - radius).floor().max(0.0) as usize).min(w);
    let y0 = ((cy - radius).floor().max(0.0) as usize).min(h);
    let x1 = (((cx + radius).ceil().max(0.0) as usize) + 1).min(w);
    let y1 = (((cy + radius).ceil().max(0.0) as usize) + 1).min(h);
    (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
}

/// Falloff table size. Indexed by squared distance, so the alpha error stays
/// well under one 8-bit step even at tight softness; a table lookup replaces a
/// sqrt and a smoothstep per stamped pixel, which dominates a large dab.
const LUT_BINS: usize = 8192;

/// Alpha profile of one dab shape, shared by every dab of a stroke.
struct AlphaProfile {
    radius: f32,
    r2: f32,
    /// d² → table index.
    idx_scale: f32,
    /// Empty for a hard edge.
    table: Vec<u8>,
}

impl AlphaProfile {
    fn new(radius: f32, softness: f32) -> Self {
        let r2 = radius * radius;
        let idx_scale = (LUT_BINS as f32) / r2.max(f32::MIN_POSITIVE);
        if softness <= 0.001 {
            AlphaProfile { radius, r2, idx_scale, table: Vec::new() }
        } else {
            let mut table = Vec::with_capacity(LUT_BINS);
            for i in 0..LUT_BINS {
                let d = (i as f32 / LUT_BINS as f32).sqrt();
                let t = ((1.0 - d) / softness).clamp(0.0, 1.0);
                table.push((t * t * (3.0 - 2.0 * t) * 255.0).round() as u8);
            }
            AlphaProfile { radius, r2, idx_scale, table }
        }
    }

    #[inline]
    fn alpha(&self, d2: f32) -> u8 {
        if d2 >= self.r2 {
            return 0;
        }
        if self.table.is_empty() {
            return 255;
        }
        // d² < r² makes the index < LUT_BINS except for a float rounding edge.
        self.table.get((d2 * self.idx_scale) as usize).copied().unwrap_or(0)
    }
}

fn combine(base: u8, coverage: u8, add: bool) -> u8 {
    if add {
        base.max(coverage)
    } else {
        ((base as u16 * (255 - coverage as u16) + 127) / 255) as u8
    }
}

/// Incremental coverage for one in-flight brush stroke.
///
/// Dabs are stamped once as the pointer path grows instead of re-rasterizing
/// the whole path every frame. The current path end is stamped analytically
/// rather than into `coverage`, so when it moves it leaves no residue behind.
/// `Rect`s are reported in mask pixels: `take_dirty` for the pixels whose live
/// value changed since the last call, `bounds` for the whole stroke.
pub struct LiveStroke {
    mask_w: usize,
    mask_h: usize,
    /// `None` is an all-zero base (empty selection): the zero mask is virtual,
    /// so starting a stroke on an empty target allocates no full-frame buffer
    /// for it.
    base: Option<Rc<Vec<u8>>>,
    radius: f32,
    softness: f32,
    profile: AlphaProfile,
    add: bool,
    coverage: Vec<u8>,
    /// Base merged with coverage as dabs land (commit strokes only).
    merged: Option<Vec<u8>>,
    /// Zero base + Add: the coverage raster is already the final mask, so
    /// `finish` hands it back instead of copying a full-frame `merged` buffer.
    coverage_is_result: bool,
    /// Exact bbox of pixels where `merged` differs from `base`.
    changed: Option<Rect>,
    points: Vec<(f32, f32)>,
    stamped: usize,
    endpoint: Option<(f32, f32)>,
    dirty: Option<Rect>,
    bounds: Option<Rect>,
}

impl LiveStroke {
    pub fn new(
        mask_w: usize,
        mask_h: usize,
        base: Rc<Vec<u8>>,
        radius: f32,
        softness: f32,
        add: bool,
        first: (f32, f32),
    ) -> Self {
        let mut live = Self {
            mask_w,
            mask_h,
            base: Some(base),
            radius,
            softness,
            profile: AlphaProfile::new(radius, softness),
            add,
            coverage: vec![0u8; mask_w * mask_h],
            merged: None,
            coverage_is_result: false,
            changed: None,
            points: Vec::new(),
            stamped: 0,
            endpoint: None,
            dirty: None,
            bounds: None,
        };
        live.push_point(first.0, first.1);
        live
    }

    /// A stroke on a target whose mask is all zeros. No base buffer is
    /// allocated; with Add, `finish` returns the coverage raster directly.
    pub fn new_empty(
        mask_w: usize,
        mask_h: usize,
        radius: f32,
        softness: f32,
        add: bool,
        first: (f32, f32),
    ) -> Self {
        let mut live = Self {
            mask_w,
            mask_h,
            base: None,
            radius,
            softness,
            profile: AlphaProfile::new(radius, softness),
            add,
            coverage: vec![0u8; mask_w * mask_h],
            merged: None,
            // With a zero base and Add, coverage is the final mask: dabs need
            // no per-dab merge, and `finish` scans the stroke bbox once.
            coverage_is_result: add,
            changed: None,
            points: Vec::new(),
            stamped: 0,
            endpoint: None,
            dirty: None,
            bounds: None,
        };
        live.push_point(first.0, first.1);
        live
    }

    /// Merge each dab into a base copy as it is stamped, so `finish` returns
    /// the final mask without a full-frame pass at pointerup. The per-dab cost
    /// lands in the drag's own (small) pointermove tasks.
    pub fn track_committed(mut self) -> Self {
        if self.merged.is_none() && !self.coverage_is_result {
            if self.base.is_none() {
                if self.add {
                    self.coverage_is_result = true;
                } else {
                    // Remove over an empty base is a no-op mask; keep the
                    // generic path, it allocates the zero result lazily.
                    self.merged = Some(vec![0u8; self.mask_w * self.mask_h]);
                }
            } else {
                self.merged = Some((**self.base.as_ref().unwrap()).clone());
                if let Some(r) = self.bounds {
                    self.merge_dab(r);
                }
            }
        }
        self
    }

    /// Merge the current coverage over one dab's bounds into `merged` and fold
    /// the exact changed extents into `changed`.
    fn merge_dab(&mut self, r: Rect) {
        let Some(merged) = self.merged.as_mut() else { return };
        let Some(base) = self.base.as_deref() else { return };
        let cov: &[u8] = &self.coverage;
        let add = self.add;
        let stride = self.mask_w;
        let (mut cx0, mut cy0, mut cx1, mut cy1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for y in r.y..r.y + r.h {
            let row = y * stride;
            let mut first = usize::MAX;
            let mut last = 0;
            for x in r.x..r.x + r.w {
                let i = row + x;
                let v = combine(base[i], cov[i], add);
                merged[i] = v;
                if v != base[i] {
                    if first == usize::MAX {
                        first = x;
                    }
                    last = x;
                }
            }
            if first != usize::MAX {
                cy0 = cy0.min(y);
                cy1 = y + 1;
                cx0 = cx0.min(first);
                cx1 = cx1.max(last + 1);
            }
        }
        if cx1 > cx0 {
            self.changed =
                Some(union_opt(self.changed, Rect { x: cx0, y: cy0, w: cx1 - cx0, h: cy1 - cy0 }));
        }
    }

    fn stamp_dab(&mut self, cx: f32, cy: f32) {
        stamp(&mut self.coverage, self.mask_w, self.mask_h, 0, 0, cx, cy, &self.profile);
        // Without a committed buffer the merge happens once in `finish`;
        // merging per dab would reprocess every overlapped pixel of every dab.
        if self.merged.is_some() {
            if let Some(r) = dab_bounds(cx, cy, self.radius, self.mask_w, self.mask_h) {
                self.merge_dab(r);
            }
        }
    }

    /// Extends the path in mask pixels. Returns false for a duplicate point.
    pub fn push_point(&mut self, x: f32, y: f32) -> bool {
        if self.points.last().is_some_and(|&(lx, ly)| lx == x && ly == y) {
            return false;
        }
        // The old tip was only ever painted analytically; mark it for repaint.
        if let Some((ox, oy)) = self.endpoint {
            if let Some(r) = dab_bounds(ox, oy, self.radius, self.mask_w, self.mask_h) {
                self.dirty = Some(union_opt(self.dirty, r));
            }
        }
        self.points.push((x, y));
        let spacing = (self.radius * 0.25).max(0.75);
        let dabs = stroke::dab_positions(&self.points, spacing);
        for d in dabs.iter().skip(self.stamped) {
            self.stamp_dab(d.x, d.y);
            if let Some(r) = dab_bounds(d.x, d.y, self.radius, self.mask_w, self.mask_h) {
                self.dirty = Some(union_opt(self.dirty, r));
                self.bounds = Some(union_opt(self.bounds, r));
            }
        }
        self.stamped = dabs.len();
        self.endpoint = Some((x, y));
        if let Some(r) = dab_bounds(x, y, self.radius, self.mask_w, self.mask_h) {
            self.dirty = Some(union_opt(self.dirty, r));
            self.bounds = Some(union_opt(self.bounds, r));
        }
        true
    }

    pub fn points(&self) -> &[(f32, f32)] {
        &self.points
    }

    pub fn radius(&self) -> f32 {
        self.radius
    }

    pub fn softness(&self) -> f32 {
        self.softness
    }

    pub fn add(&self) -> bool {
        self.add
    }

    /// Pixels changed since the previous call, as one bounding rect.
    pub fn take_dirty(&mut self) -> Option<Rect> {
        self.dirty.take()
    }

    /// Bounds of everything this stroke can affect, for the commit diff.
    pub fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    fn live_at(&self, x: usize, y: usize) -> u8 {
        let i = y * self.mask_w + x;
        let mut a = self.coverage[i];
        if let Some((cx, cy)) = self.endpoint {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            a = a.max(self.profile.alpha(dx * dx + dy * dy));
        }
        let base = self.base.as_ref().map_or(0, |b| b[i]);
        combine(base, a, self.add)
    }

    /// Live mask values for a dirty rect, row-major.
    pub fn fill_live_rect(&self, r: Rect, out: &mut [u8]) {
        for row in 0..r.h {
            for col in 0..r.w {
                out[row * r.w + col] = self.live_at(r.x + col, r.y + row);
            }
        }
    }

    /// Stamp the moving endpoint into the coverage buffer and return the final
    /// mask plus the bbox of pixels the stroke actually changed. Consumes the
    /// analytic endpoint, so a later `compose` no longer re-evaluates it.
    pub fn finish(&mut self) -> (Vec<u8>, Option<Rect>) {
        if let Some((x, y)) = self.endpoint.take() {
            self.stamp_dab(x, y);
            if let Some(r) = dab_bounds(x, y, self.radius, self.mask_w, self.mask_h) {
                self.bounds = Some(union_opt(self.bounds, r));
            }
        }
        if let Some(out) = self.merged.take() {
            return (out, self.changed);
        }
        if self.coverage_is_result {
            // Bounds is the union of every stamped dab, so it is a valid
            // (slightly loose) changed rect for the coverage mask. Scanning
            // for exact edges here costs tens of milliseconds on a full-frame
            // stroke and only trims the patch corners.
            return (std::mem::take(&mut self.coverage), self.bounds);
        }
        let mut out = match &self.base {
            Some(b) => (**b).clone(),
            None => vec![0u8; self.mask_w * self.mask_h],
        };
        let Some(b) = self.bounds else { return (out, None) };
        let Some(base) = self.base.as_deref() else { return (out, None) };
        // Change extents tracked per row, not per pixel: a full-frame stroke
        // has millions of changed pixels and a Rect update on each one is the
        // slowest part of the commit.
        let (mut cx0, mut cy0, mut cx1, mut cy1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for y in b.y..b.y + b.h {
            let row = y * self.mask_w + b.x;
            let base_row = &base[row..row + b.w];
            let cov_row = &self.coverage[row..row + b.w];
            let out_row = &mut out[row..row + b.w];
            let mut first = usize::MAX;
            let mut last = 0;
            for (x, ((o, &base_v), &cov)) in
                out_row.iter_mut().zip(base_row).zip(cov_row).enumerate()
            {
                let v = if self.add {
                    base_v.max(cov)
                } else {
                    ((base_v as u16 * (255 - cov as u16) + 127) / 255) as u8
                };
                *o = v;
                if v != base_v {
                    if first == usize::MAX {
                        first = x;
                    }
                    last = x;
                }
            }
            if first != usize::MAX {
                cy0 = cy0.min(y);
                cy1 = y + 1;
                cx0 = cx0.min(b.x + first);
                cx1 = cx1.max(b.x + last + 1);
            }
        }
        let changed = (cx1 > cx0)
            .then_some(Rect { x: cx0, y: cy0, w: cx1 - cx0, h: cy1 - cy0 });
        (out, changed)
    }

    /// Base mask with the stroke applied. Equivalent to `stroke_mask` plus
    /// `apply_stroke`, without re-stamping the path.
    pub fn compose(&self) -> Vec<u8> {
        let mut out = match &self.base {
            Some(b) => (**b).clone(),
            None => vec![0u8; self.mask_w * self.mask_h],
        };
        let Some(b) = self.bounds else { return out };
        for y in b.y..b.y + b.h {
            for x in b.x..b.x + b.w {
                out[y * self.mask_w + x] = self.live_at(x, y);
            }
        }
        out
    }
}

/// Coverage of a stroke over a `mask_w` × `mask_h` mask. Points are in mask
/// pixels; `radius` is in mask pixels; `softness` 0 = hard edge, 1 = falloff
/// from the centre. Returns None for an empty path or a non-positive radius.
pub fn stroke_mask(
    mask_w: usize,
    mask_h: usize,
    points: &[(f32, f32)],
    radius: f32,
    softness: f32,
) -> Option<StrokeMask> {
    if mask_w == 0 || mask_h == 0 || points.is_empty() || radius <= 0.0 {
        return None;
    }
    let (mut x0, mut y0) = (f32::INFINITY, f32::INFINITY);
    let (mut x1, mut y1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for &(x, y) in points {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let left = ((x0 - radius).floor().max(0.0) as usize).min(mask_w);
    let top = ((y0 - radius).floor().max(0.0) as usize).min(mask_h);
    let right = ((x1 + radius).ceil().max(0.0) as usize + 1).min(mask_w);
    let bottom = ((y1 + radius).ceil().max(0.0) as usize + 1).min(mask_h);
    if right <= left || bottom <= top {
        return None;
    }
    let w = right - left;
    let h = bottom - top;
    let mut data = vec![0u8; w * h];
    let spacing = (radius * 0.25).max(0.75);
    let dabs = stroke::dab_positions(points, spacing);
    let profile = AlphaProfile::new(radius, softness);
    // dab_positions does not force a dab at the final point; a short drag can
    // end between dabs, so stamp the endpoint explicitly.
    for dab in dabs
        .iter()
        .map(|d| (d.x, d.y))
        .chain(points.last().copied())
    {
        stamp(&mut data, w, h, left, top, dab.0, dab.1, &profile);
    }
    Some(StrokeMask { x: left, y: top, w, h, data })
}

fn stamp(
    data: &mut [u8],
    w: usize,
    h: usize,
    origin_x: usize,
    origin_y: usize,
    cx: f32,
    cy: f32,
    profile: &AlphaProfile,
) {
    let radius = profile.radius;
    let x0 = ((cx - radius).floor().max(0.0) as usize).saturating_sub(origin_x);
    let y0 = ((cy - radius).floor().max(0.0) as usize).saturating_sub(origin_y);
    let x1 = (((cx + radius).ceil().max(0.0) as usize) + 1)
        .min(origin_x + w)
        .saturating_sub(origin_x);
    let y1 = (((cy + radius).ceil().max(0.0) as usize) + 1)
        .min(origin_y + h)
        .saturating_sub(origin_y);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // Squared distance advances linearly along a row, so no multiply or sqrt
    // is needed per pixel; alpha comes from the profile table.
    let hard = profile.table.is_empty();
    let r2 = profile.r2;
    let idx_scale = profile.idx_scale;
    let table = &profile.table;
    for py in y0..y1 {
        let dy = (origin_y + py) as f32 + 0.5 - cy;
        let dy2 = dy * dy;
        let dx = (origin_x + x0) as f32 + 0.5 - cx;
        let mut d2 = dy2 + dx * dx;
        let mut step = 2.0 * dx + 1.0;
        let row = py * w;
        for px in x0..x1 {
            if d2 < r2 {
                let a = if hard {
                    255
                } else {
                    table.get((d2 * idx_scale) as usize).copied().unwrap_or(0)
                };
                if a > data[row + px] {
                    data[row + px] = a;
                }
            }
            d2 += step;
            step += 2.0;
        }
    }
}

/// Combine a stroke's coverage with a mask. Add takes the brighter value;
/// Remove scales by the inverse coverage. Both clamp to 0..=255.
pub fn apply_stroke(mask: &mut [u8], mask_w: usize, mask_h: usize, stroke: &StrokeMask, add: bool) {
    if stroke.w == 0 || stroke.h == 0 || stroke.x + stroke.w > mask_w || stroke.y + stroke.h > mask_h {
        return;
    }
    for y in 0..stroke.h {
        let row = (stroke.y + y) * mask_w + stroke.x;
        let srow = y * stroke.w;
        for x in 0..stroke.w {
            let a = stroke.data[srow + x] as u16;
            let v = &mut mask[row + x];
            if add {
                *v = (*v).max(a as u8);
            } else {
                *v = ((*v as u16 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(w: usize, h: usize) -> Vec<u8> {
        vec![0u8; w * h]
    }

    #[test]
    fn hard_stroke_fills_inside_and_leaves_outside_alone() {
        let w = 64;
        let mut m = mask(w, w);
        let s = stroke_mask(w, w, &[(32.0, 32.0)], 10.0, 0.0).unwrap();
        apply_stroke(&mut m, w, w, &s, true);
        assert_eq!(m[32 * w + 32], 255);
        assert_eq!(m[32 * w + (32 + 9)], 255);
        assert_eq!(m[32 * w + (32 + 11)], 0);
        assert_eq!(m[0], 0);
    }

    #[test]
    fn soft_edge_falls_off_monotonically() {
        let w = 64;
        let mut m = mask(w, w);
        // Click on a pixel centre so the centre sample is exactly d = 0.
        let s = stroke_mask(w, w, &[(32.5, 32.5)], 12.0, 1.0).unwrap();
        apply_stroke(&mut m, w, w, &s, true);
        let center = m[32 * w + 32];
        let mid = m[32 * w + 36];
        let edge = m[32 * w + 42];
        assert_eq!(center, 255);
        assert!(mid < center && mid > edge, "center={center} mid={mid} edge={edge}");
        assert_eq!(m[32 * w + 46], 0);
    }

    #[test]
    fn add_then_remove_at_hardness_restores_empty_mask() {
        let w = 48;
        let h = 40;
        let mut m = mask(w, h);
        let pts = [(12.0, 12.0), (30.0, 26.0)];
        let s = stroke_mask(w, h, &pts, 6.0, 0.0).unwrap();
        apply_stroke(&mut m, w, h, &s, true);
        assert!(m.iter().any(|&v| v == 255));
        apply_stroke(&mut m, w, h, &s, false);
        assert!(m.iter().all(|&v| v == 0));
    }

    #[test]
    fn add_clamps_and_remove_scales_existing_values() {
        let w = 8;
        let mut m = vec![200u8; w * w];
        let s = stroke_mask(w, w, &[(4.0, 4.0)], 3.0, 0.0).unwrap();
        apply_stroke(&mut m, w, w, &s, true);
        assert_eq!(m[4 * w + 4], 255);
        apply_stroke(&mut m, w, w, &s, false);
        assert_eq!(m[4 * w + 4], 0);
        assert_eq!(m[0], 200);
    }

    #[test]
    fn coverage_overlaps_do_not_accumulate() {
        // Two dabs at the same spot must produce one coat, not two.
        let w = 32;
        let once = stroke_mask(w, w, &[(16.0, 16.0)], 8.0, 1.0).unwrap();
        let twice = stroke_mask(w, w, &[(16.0, 16.0), (16.0, 16.0)], 8.0, 1.0).unwrap();
        assert_eq!(once.data, twice.data);
    }

    #[test]
    fn stroke_clips_at_mask_edges() {
        let w = 24;
        let h = 20;
        let s = stroke_mask(w, h, &[(-5.0, -5.0), (3.0, 4.0)], 6.0, 0.2).unwrap();
        assert!(s.x == 0 && s.y == 0);
        assert!(s.x + s.w <= w && s.y + s.h <= h);
        let mut m = mask(w, h);
        apply_stroke(&mut m, w, h, &s, true);
        assert!(m.iter().any(|&v| v > 0));
    }

    #[test]
    fn dragging_between_points_covers_the_segment() {
        let w = 64;
        let mut m = mask(w, w);
        let s = stroke_mask(w, w, &[(8.0, 32.0), (56.0, 32.0)], 4.0, 0.0).unwrap();
        apply_stroke(&mut m, w, w, &s, true);
        for x in [10, 20, 32, 44, 54] {
            assert_eq!(m[32 * w + x], 255, "gap at x={x}");
        }
    }

    #[test]
    fn live_stroke_matches_batch_rasterization() {
        let (w, h) = (96, 72);
        let base: Rc<Vec<u8>> =
            Rc::new((0..w * h).map(|i| ((i * 37) % 256) as u8).collect());
        let pts = [(8.0, 10.0), (30.0, 12.0), (50.0, 40.0), (80.0, 50.0)];
        for (softness, add) in [(0.35, true), (0.0, false), (1.0, true)] {
            let mut live = LiveStroke::new(w, h, base.clone(), 7.0, softness, add, pts[0]);
            for &p in &pts[1..] {
                assert!(live.push_point(p.0, p.1));
            }
            let mut batch = (*base).clone();
            let s = stroke_mask(w, h, &pts, 7.0, softness).unwrap();
            apply_stroke(&mut batch, w, h, &s, add);
            assert_eq!(live.compose(), batch, "softness={softness} add={add}");
        }
    }

    #[test]
    fn finish_matches_batch_and_reports_changed_bbox() {
        let (w, h) = (96, 72);
        let base: Rc<Vec<u8>> = Rc::new(vec![0u8; w * h]);
        let pts = [(8.0, 10.0), (30.0, 12.0), (50.0, 40.0), (80.0, 50.0)];
        let mut live = LiveStroke::new(w, h, base.clone(), 7.0, 0.4, true, pts[0]);
        for &p in &pts[1..] {
            assert!(live.push_point(p.0, p.1));
        }
        let (mask, changed) = live.finish();
        let mut batch = (*base).clone();
        let s = stroke_mask(w, h, &pts, 7.0, 0.4).unwrap();
        apply_stroke(&mut batch, w, h, &s, true);
        assert_eq!(mask, batch);
        let b = changed.expect("stroke changed pixels");
        let mut x0 = w;
        let mut y0 = h;
        let (mut x1, mut y1) = (0, 0);
        for y in 0..h {
            for x in 0..w {
                if mask[y * w + x] != base[y * w + x] {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        assert_eq!((b.x, b.y, b.x + b.w, b.y + b.h), (x0, y0, x1, y1));
    }

    #[test]
    fn tracked_finish_matches_untracked_and_reports_exact_bbox() {
        let (w, h) = (96, 72);
        let base: Rc<Vec<u8>> =
            Rc::new((0..w * h).map(|i| ((i * 53 + 11) % 256) as u8).collect());
        let pts = [(8.0, 10.0), (30.0, 12.0), (50.0, 40.0), (80.0, 50.0)];
        for add in [true, false] {
            let mut plain = LiveStroke::new(w, h, base.clone(), 7.0, 0.4, add, pts[0]);
            let mut tracked =
                LiveStroke::new(w, h, base.clone(), 7.0, 0.4, add, pts[0]).track_committed();
            for &p in &pts[1..] {
                plain.push_point(p.0, p.1);
                tracked.push_point(p.0, p.1);
            }
            let (want, want_b) = plain.finish();
            let (got, got_b) = tracked.finish();
            assert_eq!(got, want, "add={add}");
            assert_eq!(got_b, want_b, "add={add}");
        }
    }

    #[test]
    fn live_stroke_reports_dirty_only_for_the_tip() {
        let (w, h) = (64, 64);
        let base = Rc::new(vec![0u8; w * h]);
        let mut live = LiveStroke::new(w, h, base, 4.0, 0.0, true, (10.0, 10.0));
        let first = live.take_dirty().unwrap();
        assert!(first.x <= 6 && first.y <= 6 && first.w <= 9 && first.h <= 9);
        assert!(live.take_dirty().is_none());
        // A tiny move repaints at most the old and new tip areas.
        live.push_point(12.0, 10.0);
        let next = live.take_dirty().unwrap();
        assert!(next.w <= 14 && next.h <= 9, "{next:?}");
    }
}
