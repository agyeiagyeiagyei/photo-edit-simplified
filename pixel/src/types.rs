//! Pure data types shared by the app and the pixel worker.

use std::rc::Rc;

use crate::curves::CurvesSettings;
use crate::levels::LevelsSettings;

#[derive(Clone, PartialEq, Debug)]
pub enum SelectionKind {
    Rect { x: f32, y: f32, w: f32, h: f32 },
    Lasso(Vec<(f32, f32)>),
    /// Per-pixel mask (e.g. magic wand, ML segmentation) captured in the
    /// geometry-corrected pixel space it was created at; scaled on use.
    /// Rc for the same clone-cost reason as RasterLayer.pixels.
    Mask { data: Rc<Vec<u8>>, width: usize, height: usize },
}

#[derive(Clone, PartialEq, Debug)]
pub struct Selection {
    pub kind: SelectionKind,
    /// Feather radius as a fraction of the image diagonal (0..0.25).
    pub feather: f32,
}

/// Point operations an adjustment layer applies inside its mask. A subset of
/// the global colour controls: every stage is per-pixel, so it can run through
/// a mask in one pass.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct PointAdjust {
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub warmth: f32,
    pub levels: LevelsSettings,
    pub curves: CurvesSettings,
}

impl PointAdjust {
    pub fn is_identity(&self) -> bool {
        self.brightness == 0.0
            && self.contrast == 0.0
            && self.saturation == 0.0
            && self.warmth == 0.0
            && self.levels.is_identity()
            && self.curves.is_identity()
    }
}

/// One adjustment layer: point operations confined to a mask. Layers apply
/// bottom to top, in vector order, each blended by mask × opacity.
#[derive(Clone, PartialEq, Debug)]
pub struct AdjustLayer {
    /// Matches the app layer id; keys the worker's cached mask rasters.
    pub id: u64,
    pub visible: bool,
    /// 0..=1, scales the mask's effect.
    pub opacity: f32,
    /// None applies to the whole frame.
    pub mask: Option<Selection>,
    pub invert: bool,
    /// Bumped whenever the mask changes, so a stale cached raster is ignored.
    pub mask_version: u64,
    pub adjust: PointAdjust,
}

impl AdjustLayer {
    /// Whether this layer can change any pixel.
    pub fn active(&self) -> bool {
        self.visible && self.opacity > 0.0 && !self.adjust.is_identity()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Aspect {
    Original,
    Nine16,
    One1,
    Four5,
    Sixteen9,
}

impl Aspect {
    pub const ALL: [Aspect; 5] = [
        Aspect::Original,
        Aspect::Nine16,
        Aspect::One1,
        Aspect::Four5,
        Aspect::Sixteen9,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Aspect::Original => "Orig",
            Aspect::Nine16 => "9:16",
            Aspect::One1 => "1:1",
            Aspect::Four5 => "4:5",
            Aspect::Sixteen9 => "16:9",
        }
    }

    /// Filename-safe label: `:` is not legal in Windows or macOS filenames.
    pub fn slug(self) -> &'static str {
        match self {
            Aspect::Original => "orig",
            Aspect::Nine16 => "9x16",
            Aspect::One1 => "1x1",
            Aspect::Four5 => "4x5",
            Aspect::Sixteen9 => "16x9",
        }
    }

    /// (ratio_w, ratio_h) if locked.
    pub fn ratio(self) -> Option<(f32, f32)> {
        match self {
            Aspect::Original => None,
            Aspect::Nine16 => Some((9.0, 16.0)),
            Aspect::One1 => Some((1.0, 1.0)),
            Aspect::Four5 => Some((4.0, 5.0)),
            Aspect::Sixteen9 => Some((16.0, 9.0)),
        }
    }

    /// Export pixel dimensions, capped so the long edge <= source long edge
    /// (never upscale past source).
    pub fn export_dims(self, src_w: usize, src_h: usize) -> (usize, usize) {
        let (tw, th) = match self {
            Aspect::Original => (src_w as u32, src_h as u32),
            Aspect::Nine16 => (1080, 1920),
            Aspect::One1 => (1080, 1080),
            Aspect::Four5 => (1080, 1350),
            Aspect::Sixteen9 => (1920, 1080),
        };
        let scale = (src_w as f32 / tw as f32).min(src_h as f32 / th as f32).min(1.0);
        ((tw as f32 * scale) as usize, (th as f32 * scale) as usize)
    }
}

/// Crop rectangle in image coordinates (after rotation), normalized 0..1.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Default for CropRect {
    fn default() -> Self {
        CropRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }
    }
}

/// Target white-balance temperature that reproduces the camera's as-shot
/// rendering — the neutral point of `RawAdjust::temp`.
pub const NEUTRAL_K: f32 = 5500.0;

/// RAW develop adjustments, applied in linear light before the normal
/// gamma-space colour controls. Identity = camera rendering, untouched.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RawAdjust {
    /// White-balance temperature in Kelvin, 2500..9500; NEUTRAL_K = as-shot.
    pub temp: f32,
    /// Green/magenta balance -100..100; 0 = neutral, positive = magenta.
    pub tint: f32,
    /// Exposure compensation in stops, applied as a linear gain.
    pub exposure: f32,
    /// Highlight recovery 0..100 — linear shoulder above the knee.
    pub highlights: f32,
    /// Shadow lift 0..100 — soft toe in linear light.
    pub shadows: f32,
    /// Shadow chroma-noise suppression 0..100.
    pub noise: f32,
    /// Edge-aware whole-frame denoise 0..100 — separable bilateral on luma
    /// and chroma; detail and edges are preserved.
    pub denoise: f32,
    /// Capture (demosaic) sharpening 0..100 — linear unsharp mask.
    pub sharpen: f32,
}

impl Default for RawAdjust {
    fn default() -> Self {
        RawAdjust {
            temp: NEUTRAL_K,
            tint: 0.0,
            exposure: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            noise: 0.0,
            denoise: 0.0,
            sharpen: 0.0,
        }
    }
}

impl RawAdjust {
    pub fn is_identity(&self) -> bool {
        *self == RawAdjust::default()
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct EditParams {
    pub aspect: Aspect,
    pub crop: CropRect,
    /// 0..3 clockwise quarter-turns applied before fine angle.
    pub rot90: u8,
    /// Fine straighten angle, degrees, clockwise, ±10.
    pub fine_angle: f32,
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub warmth: f32,
    /// Gaussian blur amount 0..=100 (0 = off), maps to sigma 0..8 px.
    pub blur: f32,
    /// Film grain amount 0..=100 (0 = off).
    pub grain: f32,
    /// Gaussian (true) vs uniform (false) grain distribution.
    pub grain_gaussian: bool,
    /// Same noise on all channels (luma grain) vs per-channel color noise.
    pub grain_mono: bool,
    /// Levels (black/gamma/white per channel + composite). Identity = off.
    pub levels: LevelsSettings,
    /// Tone curves per channel + composite. Identity = off.
    pub curves: CurvesSettings,
    /// RAW develop stages (linear light). Identity = camera rendering.
    pub raw: RawAdjust,
    /// Active selection mask (rect, lasso, or derived from ML segmentation).
    pub selection: Option<Selection>,
    /// Bumped on every in-place change to `selection`'s mask bytes. The
    /// selection hash samples 8 bytes, so a brush stroke that misses those
    /// positions would not invalidate the render caches without this.
    pub selection_rev: u64,
    /// Adjustment layers applied on top of the global colour stages, bottom
    /// first. Empty for a photo without adjustment layers.
    pub adjust_layers: Vec<AdjustLayer>,
    /// Video trim (start_s, end_s); None = untrimmed.
    pub trim: Option<(f32, f32)>,
    /// Keep original audio track on export; if false, strip audio.
    pub keep_audio: bool,
}

impl Default for EditParams {
    fn default() -> Self {
        EditParams {
            aspect: Aspect::Original,
            crop: CropRect::default(),
            rot90: 0,
            fine_angle: 0.0,
            brightness: 0.0,
            contrast: 0.0,
            saturation: 0.0,
            warmth: 0.0,
            blur: 0.0,
            grain: 0.0,
            grain_gaussian: true,
            grain_mono: true,
            levels: LevelsSettings::default(),
            curves: CurvesSettings::default(),
            raw: RawAdjust::default(),
            selection: None,
            selection_rev: 0,
            adjust_layers: Vec::new(),
            trim: None,
            keep_audio: true,
        }
    }
}

impl EditParams {
    /// Geometry and the RAW develop only, with every colour stage reset: what
    /// a GPU base carries when the shader applies the colour edits itself.
    pub fn develop_only(&self) -> EditParams {
        EditParams {
            aspect: self.aspect,
            crop: self.crop,
            rot90: self.rot90,
            fine_angle: self.fine_angle,
            raw: self.raw,
            ..EditParams::default()
        }
    }

    /// True when a global (non-layer) colour or develop stage is active.
    pub fn base_color_touched(&self) -> bool {
        self.brightness != 0.0
            || self.contrast != 0.0
            || self.saturation != 0.0
            || self.warmth != 0.0
            || self.blur > 0.0
            || self.grain > 0.0
            || !self.levels.is_identity()
            || !self.curves.is_identity()
            || !self.raw.is_identity()
    }

    pub fn is_color_touched(&self) -> bool {
        self.base_color_touched() || self.adjust_layers.iter().any(|l| l.active())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn develop_only_keeps_geometry_and_raw_and_drops_colour() {
        let mut e = EditParams::default();
        e.rot90 = 1;
        e.fine_angle = 2.5;
        e.brightness = 0.3;
        e.saturation = -0.2;
        e.grain = 10.0;
        e.raw.exposure = 0.7;
        e.raw.denoise = 25.0;
        let d = e.develop_only();
        assert_eq!((d.rot90, d.fine_angle), (1, 2.5));
        assert_eq!(d.raw, e.raw);
        assert_eq!((d.brightness, d.saturation, d.grain), (0.0, 0.0, 0.0));
        assert!(d.levels.is_identity() && d.curves.is_identity());
    }
}

