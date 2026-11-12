//! Where a rendered frame sits in the photo, so the effects with a spatial
//! size (Blur's radius, Grain's particle size) are defined in image units and
//! look the same at every render size: the fit preview, a cropped or zoomed
//! window, the loupe, a full-size export.
//!
//! Both are calibrated on a [`REF_LONG`]-pixel long edge: Blur 100 is a sigma
//! of 8 px there and one grain cell is one pixel there. A render at another
//! size scales the blur sigma and samples the same grain lattice, so a 6000 px
//! export downscaled to the screen matches the preview, and a window of the
//! frame shows exactly the export's grain at those pixels.

/// Reference long edge the effect sizes are calibrated at.
pub const REF_LONG: f64 = 2048.0;

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

    /// Grain amplitude factor. A grain cell smaller than a render pixel is
    /// averaged with its neighbours, as it would be when a full-size export
    /// is downscaled to this size: the deviation of a mean of n independent
    /// cells falls by sqrt(n) = 1/unit.
    pub fn grain_gain(&self) -> f32 {
        self.unit().min(1.0) as f32
    }

    /// Grain lattice column for frame column `x` (and row for `y` with
    /// [`Place::cell_y`]): the reference pixel under the render pixel's
    /// centre. Whole frames reduce to the integer `((2x+1)*1024)/long`, which
    /// the GPU computes the same way.
    pub fn cell_x(&self, x: usize) -> u32 {
        ((self.ox + x as f64 + 0.5) * REF_LONG / self.long) as u32
    }

    pub fn cell_y(&self, y: usize) -> u32 {
        ((self.oy + y as f64 + 0.5) * REF_LONG / self.long) as u32
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
    fn whole_frame_cells_match_the_integer_formula_the_gpu_uses() {
        for long in [683usize, 1024, 1925, 2048, 2888, 4000, 6000] {
            let p = Place::whole(long, long * 2 / 3);
            for x in 0..long {
                let want = (((2 * x + 1) * 1024) / long) as u32;
                assert_eq!(p.cell_x(x), want, "long {long} x {x}");
            }
        }
    }

    #[test]
    fn a_window_samples_the_same_cells_as_the_whole_frame() {
        // A 6000x4000 working frame at native size, and a 600x400 window of it
        // at (2400, 1600): every window pixel lands on the whole frame's cell.
        let whole = Place::whole(6000, 4000);
        let win = Place::window(6000, 4000, (2400, 1600, 600, 400), 600);
        for x in 0..600 {
            assert_eq!(win.cell_x(x), whole.cell_x(2400 + x));
        }
        for y in 0..400 {
            assert_eq!(win.cell_y(y), whole.cell_y(1600 + y));
        }
    }

    #[test]
    fn blur_sigma_is_a_fixed_fraction_of_the_long_edge() {
        let small = Place::whole(1024, 683).blur_sigma(50.0);
        let big = Place::whole(6000, 4000).blur_sigma(50.0);
        assert!((small / 1024.0 - big / 6000.0).abs() < 1e-6);
        assert!((Place::PIXEL.blur_sigma(100.0) - 8.0).abs() < 1e-6);
    }

    #[test]
    fn grain_gain_only_falls_below_the_reference_size() {
        assert_eq!(Place::whole(4096, 100).grain_gain(), 1.0);
        assert_eq!(Place::whole(2048, 100).grain_gain(), 1.0);
        assert!((Place::whole(1024, 100).grain_gain() - 0.5).abs() < 1e-6);
    }
}
