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
    /// Active selection mask (rect, lasso, or derived from ML segmentation).
    pub selection: Option<Selection>,
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
            selection: None,
            trim: None,
            keep_audio: true,
        }
    }
}

impl EditParams {
    pub fn is_color_touched(&self) -> bool {
        self.brightness != 0.0
            || self.contrast != 0.0
            || self.saturation != 0.0
            || self.warmth != 0.0
            || self.blur > 0.0
            || self.grain > 0.0
            || !self.levels.is_identity()
            || !self.curves.is_identity()
    }
}
