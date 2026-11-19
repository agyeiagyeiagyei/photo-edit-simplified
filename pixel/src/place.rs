//! Where a rendered frame sits in the photo, so the effects with a spatial
//! size (Blur's radius, Grain's particle size) are defined in image units and
//! look the same at every render size: the fit preview, a cropped or zoomed
//! window, the loupe, a full-size export.
//!
//! Both are calibrated on a [`REF_LONG`]-pixel long edge: Blur 100 is a sigma
//! of 8 px there and one grain cell is one pixel there. A render at another
//! size scales the blur sigma and area-samples the same grain lattice, so a
//! 6000 px export downscaled to the screen matches the preview, and a window
//! of the frame shows exactly the export's grain at those pixels.

/// Reference long edge the effect sizes are calibrated at.
pub const REF_LONG: f64 = 2048.0;

/// Grain strength scale. Area-sampled grain is weaker per pixel than the
/// point-sampled grain previews showed before #51 (2/3 to 5/6 of it at
/// preview sizes); this keeps a preview between 0.5x and 2x the reference
/// size within about 12% of the strength it was tuned at.
pub const GRAIN_NORM: f64 = 4.0 / 3.0;

/// Lattice cells a footprint may span along one axis on the GPU, which loops
/// over at most this many per axis. Renders with smaller cells fall back to
/// the CPU (see [`Place::grain_fits_gpu`]).
pub const GPU_GRAIN_TAPS: usize = 4;

/// Where the lattice sits against the frame's pixel grid, in cells: a
/// quarter cell in from the top-left corner. With cell edges on pixel edges,
/// a 1024 or 2048 px render (exactly 2 or 1 cells per pixel) would average
/// whole cells with nothing shared between neighbours, a crisper grain than
/// any downscaled export shows (#51). The shader adds the same quarter as
/// `L/4` in its integer cell bounds.
pub const GRAIN_PHASE: f64 = 0.25;

/// One axis of a render's footprints on the grain lattice. Render pixel `p`
/// covers cells `first[p]..` with the weights `w[off[p]..off[p + 1]]`: each
/// cell's overlap with the pixel, scaled so the squares sum to one. Every
/// pixel's grain then has the same strength whatever its alignment with the
/// cells, and [`Place::grain_gain`] sets that strength to the average an
/// exact area sample would have.
#[derive(Clone, Debug, PartialEq)]
pub struct GrainAxis {
    pub first: Vec<u32>,
    pub off: Vec<u32>,
    pub w: Vec<f32>,
}

impl GrainAxis {
    /// Cells and weights under pixel `p`.
    pub fn at(&self, p: usize) -> (u32, &[f32]) {
        (self.first[p], &self.w[self.off[p] as usize..self.off[p + 1] as usize])
    }

    /// One past the last cell any pixel touches.
    pub fn end(&self) -> u32 {
        (0..self.first.len()).map(|p| self.first[p] + self.off[p + 1] - self.off[p]).max().unwrap_or(0)
    }
}

/// Overlaps of the render-pixel interval `[a, b)` (in cells) with each cell.
fn overlaps(a: f64, b: f64, mut push: impl FnMut(u32, f64)) {
    let eps = (b - a) * 1e-9;
    let mut c = a.floor();
    while c < b {
        let ov = b.min(c + 1.0) - a.max(c);
        if ov > eps {
            push(c as u32, ov);
        }
        c += 1.0;
    }
}

/// The rendered frame inside the whole working frame (geometry applied, no
/// crop), in this render's pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    /// Long edge of the whole working frame, in this render's pixels.
    pub long: f64,
    /// Top-left of this frame within the whole working frame, in this
    /// render's pixels.
    pub ox: f64,
    pub oy: f64,
}

impl Place {
    /// A render of the whole working frame at `w` x `h`.
    pub fn whole(w: usize, h: usize) -> Place {
        Place { long: w.max(h).max(1) as f64, ox: 0.0, oy: 0.0 }
    }

    /// One frame pixel per reference pixel: the pre-#47 behaviour, for unit
    /// tests of the kernels themselves.
    pub const PIXEL: Place = Place { long: REF_LONG, ox: 0.0, oy: 0.0 };

    /// The window `(x, y, w, h)` of a `ww` x `wh` working frame, rendered at
    /// `tw` pixels wide.
    pub fn window(ww: usize, wh: usize, rect: (usize, usize, usize, usize), tw: usize) -> Place {
        let (x, y, w, _) = rect;
        let s = tw as f64 / w.max(1) as f64;
        Place { long: ww.max(wh).max(1) as f64 * s, ox: x as f64 * s, oy: y as f64 * s }
    }

    /// Render pixels per reference pixel.
    pub fn unit(&self) -> f64 {
        self.long / REF_LONG
    }

    /// Gaussian sigma, in this render's pixels, for a Blur amount (0..=100).
    pub fn blur_sigma(&self, amount: f32) -> f32 {
        (amount as f64 / 100.0 * 8.0 * self.unit()) as f32
    }

    /// Grain amplitude factor: [`GRAIN_NORM`] times the deviation an exact
    /// area sample of the unit-variance lattice has at this size, averaged
    /// over the frame's pixels. That is what a full-size export downscaled
    /// to this size shows, so the preview's grain is as strong as the
    /// export's. It depends only on the frame's size, never on the window,
    /// so a zoomed window matches the export.
    pub fn grain_gain(&self) -> f32 {
        let n = self.long.ceil().clamp(1.0, 4096.0) as usize;
        let mut sum = 0.0;
        for p in 0..n {
            let (a, b) = self.span(0.0, p);
            let mut q = 0.0;
            overlaps(a, b, |_, ov| q += ov * ov);
            sum += q / ((b - a) * (b - a));
        }
        (GRAIN_NORM * sum / n as f64) as f32
    }

    /// Pixel `p`'s interval along an axis whose frame origin is `o`, in cells.
    /// Dividing last keeps whole frames on the shader's integer bounds, and
    /// gives a window at native scale the whole frame's very intervals.
    fn span(&self, o: f64, p: usize) -> (f64, f64) {
        let p = o + p as f64;
        let q = self.long * GRAIN_PHASE;
        ((p * REF_LONG + q) / self.long, ((p + 1.0) * REF_LONG + q) / self.long)
    }

    fn grain_axis(&self, o: f64, n: usize) -> GrainAxis {
        let mut ax = GrainAxis { first: Vec::with_capacity(n), off: Vec::with_capacity(n + 1), w: Vec::new() };
        ax.off.push(0);
        let mut row: Vec<(u32, f64)> = Vec::new();
        for p in 0..n {
            let (a, b) = self.span(o, p);
            row.clear();
            overlaps(a, b, |c, ov| row.push((c, ov)));
            let norm = row.iter().map(|&(_, ov)| ov * ov).sum::<f64>().sqrt();
            ax.first.push(row[0].0);
            ax.w.extend(row.iter().map(|&(_, ov)| (ov / norm) as f32));
            ax.off.push(ax.w.len() as u32);
        }
        ax
    }

    /// Grain footprints of a `w`-pixel-wide render's columns.
    pub fn grain_cols(&self, w: usize) -> GrainAxis {
        self.grain_axis(self.ox, w)
    }

    /// Grain footprints of an `h`-pixel-tall render's rows.
    pub fn grain_rows(&self, h: usize) -> GrainAxis {
        self.grain_axis(self.oy, h)
    }

    /// Whether every footprint spans at most [`GPU_GRAIN_TAPS`] cells per
    /// axis: a pixel `2048 / long` cells wide touches at most that plus one.
    pub fn grain_fits_gpu(&self) -> bool {
        self.long * (GPU_GRAIN_TAPS - 1) as f64 >= REF_LONG
    }
}

/// Lattice key for grain cell `(i, j)`. The lattice is at most `REF_LONG`
/// cells on its long edge, so 4096 columns per row never collide.
pub fn cell_index(i: u32, j: u32) -> u32 {
    j.wrapping_mul(4096).wrapping_add(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_frame_footprints_match_the_integer_formula_the_gpu_uses() {
        // The shader takes cells (8192x + L)/4L up to ceil((8192(x+1) + L)/4L):
        // x*2048/L plus the quarter-cell phase, in integers.
        for long in [683usize, 1024, 1200, 1925, 2048, 2888, 4000, 4096, 6000, 6192] {
            let ax = Place::whole(long, long * 2 / 3).grain_cols(long);
            for x in 0..long {
                let (first, w) = ax.at(x);
                let c0 = (x * 8192 + long) / (4 * long);
                let c1 = ((x + 1) * 8192 + long).div_ceil(4 * long);
                assert_eq!((first as usize, w.len()), (c0, c1 - c0), "long {long} x {x}");
                let q: f32 = w.iter().map(|v| v * v).sum();
                assert!((q - 1.0).abs() < 1e-5, "long {long} x {x}: squares sum {q}");
            }
        }
    }

    #[test]
    fn a_window_has_the_whole_frames_footprints() {
        // A 6000x4000 working frame at native size, and a 600x400 window of it
        // at (2400, 1600): every window pixel covers the whole frame's cells
        // with the whole frame's weights, bit for bit.
        let whole = Place::whole(6000, 4000);
        let win = Place::window(6000, 4000, (2400, 1600, 600, 400), 600);
        let (wc, ac) = (whole.grain_cols(6000), win.grain_cols(600));
        for x in 0..600 {
            assert_eq!(ac.at(x), wc.at(2400 + x));
        }
        let (wr, ar) = (whole.grain_rows(4000), win.grain_rows(400));
        for y in 0..400 {
            assert_eq!(ar.at(y), wr.at(1600 + y));
        }
        assert_eq!(win.grain_gain(), whole.grain_gain());
    }

    #[test]
    fn small_cells_are_spread_over_several_taps() {
        // 683 px: three cells per pixel, so up to four taps.
        let ax = Place::whole(683, 455).grain_cols(683);
        assert!((0..683).all(|x| (3..=4).contains(&ax.at(x).1.len())));
        // The quarter-cell phase reaches into one more cell.
        assert_eq!(ax.end(), 2049);
    }

    #[test]
    fn blur_sigma_is_a_fixed_fraction_of_the_long_edge() {
        let small = Place::whole(1024, 683).blur_sigma(50.0);
        let big = Place::whole(6000, 4000).blur_sigma(50.0);
        assert!((small / 1024.0 - big / 6000.0).abs() < 1e-6);
        assert!((Place::PIXEL.blur_sigma(100.0) - 8.0).abs() < 1e-6);
    }

    #[test]
    fn grain_gain_is_the_area_sample_deviation() {
        let g = |long: usize| Place::whole(long, 100).grain_gain() as f64 / GRAIN_NORM;
        // A whole number of pixels per cell, or cells per pixel: every pixel
        // sits at the quarter-cell phase. 2048 px: 3/4 and 1/4 of two cells.
        // 4096 px: alternately inside one cell and across two quarters.
        // 1024 px: a quarter, a whole and three quarters, over 2 cells.
        assert!((g(2048) - 0.625).abs() < 1e-6);
        assert!((g(4096) - 0.75).abs() < 1e-6);
        assert!((g(1024) - 0.40625).abs() < 1e-6);
        // Otherwise the mean over every alignment: 1 - 1/(3u) for cells wider
        // than a pixel (u = long / 2048), u - u^2/3 for narrower ones.
        for long in [2888usize, 6000, 6192] {
            let u = long as f64 / 2048.0;
            assert!((g(long) - (1.0 - 1.0 / (3.0 * u))).abs() < 2e-3, "long {long}: {}", g(long));
        }
        for long in [683usize, 1200, 1925] {
            let u = long as f64 / 2048.0;
            assert!((g(long) - (u - u * u / 3.0)).abs() < 2e-3, "long {long}: {}", g(long));
        }
    }

    #[test]
    fn the_gpu_takes_grain_down_to_three_cells_per_pixel() {
        assert!(Place::whole(683, 400).grain_fits_gpu());
        assert!(!Place::whole(682, 400).grain_fits_gpu());
        assert!(Place::whole(6000, 4000).grain_fits_gpu());
    }
}
