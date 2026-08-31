//! Shared application state and edit-parameter model.

use leptos::*;

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

#[derive(Clone, Copy, PartialEq, Debug)]
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
    /// Video trim (start_s, end_s); None = untrimmed.
    pub trim: Option<(f32, f32)>,
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
            trim: None,
        }
    }
}

impl EditParams {
    pub fn is_color_touched(&self) -> bool {
        self.brightness != 0.0
            || self.contrast != 0.0
            || self.saturation != 0.0
            || self.warmth != 0.0
    }
}

#[derive(Clone, PartialEq)]
pub enum MediaKind {
    Photo,
    Video,
}

#[derive(Clone)]
pub struct MediaItem {
    pub id: usize,
    pub kind: MediaKind,
    pub name: String,
    /// Object URL for the source blob (photo preview / video element).
    pub object_url: String,
    /// Full-res RGBA for photos (loaded lazily).
    pub width: usize,
    pub height: usize,
    pub edit: EditParams,
}

#[derive(Clone, Copy)]
pub struct AppState {
    pub items: RwSignal<Vec<MediaItem>>,
    pub selected: RwSignal<Option<usize>>,
    pub busy: RwSignal<Option<String>>,
    /// 0.0..1.0 while a video transcode runs.
    pub progress: RwSignal<f32>,
    pub next_id: RwSignal<usize>,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            items: create_rw_signal(Vec::new()),
            selected: create_rw_signal(None),
            busy: create_rw_signal(None),
            progress: create_rw_signal(0.0),
            next_id: create_rw_signal(0),
        }
    }

    pub fn current(self) -> Option<MediaItem> {
        let sel = self.selected.get()?;
        self.items.with(|v| v.iter().find(|m| m.id == sel).cloned())
    }

    pub fn update_current(self, f: impl Fn(&mut EditParams)) {
        let sel = match self.selected.get() {
            Some(s) => s,
            None => return,
        };
        self.items.update(|v| {
            if let Some(m) = v.iter_mut().find(|m| m.id == sel) {
                f(&mut m.edit);
            }
        });
    }
}
