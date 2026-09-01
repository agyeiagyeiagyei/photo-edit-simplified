//! Shared application state and edit-parameter model.

use leptos::*;
use leptos::batch;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

impl TextAlign {
    pub const ALL: [TextAlign; 3] = [TextAlign::Left, TextAlign::Center, TextAlign::Right];

    pub fn label(self) -> &'static str {
        match self {
            TextAlign::Left => "Left",
            TextAlign::Center => "Center",
            TextAlign::Right => "Right",
        }
    }

    pub fn canvas_value(self) -> &'static str {
        match self {
            TextAlign::Left => "left",
            TextAlign::Center => "center",
            TextAlign::Right => "right",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool {
    Select,
    Pen,
    Brush,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::Select, Tool::Pen, Tool::Brush];

    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Pen => "Pen",
            Tool::Brush => "Brush",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SelectTool {
    Rect,
    Lasso,
}

impl SelectTool {
    pub const ALL: [SelectTool; 2] = [SelectTool::Rect, SelectTool::Lasso];

    pub fn label(self) -> &'static str {
        match self {
            SelectTool::Rect => "Rect",
            SelectTool::Lasso => "Lasso",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct TextLayer {
    pub text: String,
    pub x: f32,
    pub y: f32,
    /// Font size as a fraction of the image height.
    pub font_size: f32,
    pub font_family: String,
    pub font_weight: u16,
    pub color: String,
    pub stroke_color: String,
    pub stroke_width: f32,
    pub shadow_color: String,
    pub shadow_blur: f32,
    pub shadow_offset_x: f32,
    pub shadow_offset_y: f32,
    pub alignment: TextAlign,
}

impl Default for TextLayer {
    fn default() -> Self {
        TextLayer {
            text: "Text".into(),
            x: 0.5,
            y: 0.5,
            font_size: 0.08,
            font_family: "Inter".into(),
            font_weight: 700,
            color: "#ffffff".into(),
            stroke_color: "#000000".into(),
            stroke_width: 0.0,
            shadow_color: "rgba(0,0,0,0.5)".into(),
            shadow_blur: 4.0,
            shadow_offset_x: 2.0,
            shadow_offset_y: 2.0,
            alignment: TextAlign::Center,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct PathPoint {
    pub x: f32,
    pub y: f32,
    /// Incoming control handle (relative to point).
    pub in_x: f32,
    pub in_y: f32,
    /// Outgoing control handle (relative to point).
    pub out_x: f32,
    pub out_y: f32,
    /// True for smooth (collinear mirrored handles), false for corner.
    pub smooth: bool,
}

impl PathPoint {
    pub fn new(x: f32, y: f32) -> Self {
        PathPoint { x, y, in_x: 0.0, in_y: 0.0, out_x: 0.0, out_y: 0.0, smooth: true }
    }

    pub fn corner(x: f32, y: f32) -> Self {
        PathPoint { x, y, in_x: 0.0, in_y: 0.0, out_x: 0.0, out_y: 0.0, smooth: false }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct PathLayer {
    pub points: Vec<PathPoint>,
    pub closed: bool,
    pub fill_color: String,
    pub stroke_color: String,
    pub stroke_width: f32,
    /// Per-path undo stack of point states.
    pub history: Vec<Vec<PathPoint>>,
}

impl Default for PathLayer {
    fn default() -> Self {
        PathLayer {
            points: Vec::new(),
            closed: false,
            fill_color: "#ff3b30".into(),
            stroke_color: "#000000".into(),
            stroke_width: 0.01,
            history: Vec::new(),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct BrushStroke {
    pub points: Vec<(f32, f32)>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct BrushLayer {
    pub strokes: Vec<BrushStroke>,
    pub color: String,
    pub width: f32,
    /// Per-layer undo stack of stroke states.
    pub history: Vec<Vec<BrushStroke>>,
}

impl Default for BrushLayer {
    fn default() -> Self {
        BrushLayer {
            strokes: Vec::new(),
            color: "#0a84ff".into(),
            width: 0.015,
            history: Vec::new(),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum LayerKind {
    Text(TextLayer),
    Path(PathLayer),
    Brush(BrushLayer),
    Raster(RasterLayer),
}

#[derive(Clone, PartialEq, Debug)]
pub struct RasterLayer {
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, PartialEq, Debug)]
pub enum SelectionKind {
    Rect { x: f32, y: f32, w: f32, h: f32 },
    Lasso(Vec<(f32, f32)>),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Selection {
    pub kind: SelectionKind,
    /// Feather radius as a fraction of the image diagonal (0..0.25).
    pub feather: f32,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Layer {
    pub id: usize,
    pub visible: bool,
    pub opacity: f32,
    pub kind: LayerKind,
}

impl Layer {
    pub fn new_text(id: usize) -> Self {
        Layer {
            id,
            visible: true,
            opacity: 1.0,
            kind: LayerKind::Text(TextLayer::default()),
        }
    }

    pub fn new_path(id: usize) -> Self {
        Layer {
            id,
            visible: true,
            opacity: 1.0,
            kind: LayerKind::Path(PathLayer::default()),
        }
    }

    pub fn new_brush(id: usize) -> Self {
        Layer {
            id,
            visible: true,
            opacity: 1.0,
            kind: LayerKind::Brush(BrushLayer::default()),
        }
    }

    pub fn new_raster(id: usize, pixels: Vec<u8>, width: usize, height: usize) -> Self {
        Layer {
            id,
            visible: true,
            opacity: 1.0,
            kind: LayerKind::Raster(RasterLayer { pixels, width, height }),
        }
    }

    pub fn label(&self) -> String {
        match &self.kind {
            LayerKind::Text(t) => format!("T: {}", t.text),
            LayerKind::Path(_) => "Path".into(),
            LayerKind::Brush(_) => "Brush".into(),
            LayerKind::Raster(_) => "Raster".into(),
        }
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
    pub layers: Vec<Layer>,
    pub next_layer_id: usize,
}

#[derive(Clone, Copy)]
pub struct AppState {
    pub items: RwSignal<Vec<MediaItem>>,
    pub selected: RwSignal<Option<usize>>,
    pub selected_layer: RwSignal<Option<usize>>,
    pub selected_tool: RwSignal<Tool>,
    pub selected_select_tool: RwSignal<SelectTool>,
    pub busy: RwSignal<Option<String>>,
    /// 0.0..1.0 while a video transcode runs.
    pub progress: RwSignal<f32>,
    pub next_id: RwSignal<usize>,
    /// Available font family names (bundled + user-uploaded).
    pub fonts: RwSignal<Vec<String>>,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            items: create_rw_signal(Vec::new()),
            selected: create_rw_signal(None),
            selected_layer: create_rw_signal(None),
            selected_tool: create_rw_signal(Tool::Select),
            selected_select_tool: create_rw_signal(SelectTool::Rect),
            busy: create_rw_signal(None),
            progress: create_rw_signal(0.0),
            next_id: create_rw_signal(0),
            fonts: create_rw_signal(vec!["Inter".into(), "Oswald".into()]),
        }
    }

    pub fn current(self) -> Option<MediaItem> {
        let sel = self.selected.get()?;
        self.items.with(|v| v.iter().find(|m| m.id == sel).cloned())
    }

    pub fn update_current(self, f: impl FnOnce(&mut EditParams)) {
        let sel = match self.selected.get_untracked() {
            Some(s) => s,
            None => return,
        };
        batch(|| {
            self.items.update(|v| {
                if let Some(m) = v.iter_mut().find(|m| m.id == sel) {
                    f(&mut m.edit);
                }
            });
        });
    }

    pub fn update_current_item(self, f: impl FnOnce(&mut MediaItem)) {
        let sel = match self.selected.get_untracked() {
            Some(s) => s,
            None => return,
        };
        batch(|| {
            self.items.update(|v| {
                if let Some(m) = v.iter_mut().find(|m| m.id == sel) {
                    f(m);
                }
            });
        });
    }

    pub fn current_layers(self) -> Vec<Layer> {
        self.current().map(|m| m.layers).unwrap_or_default()
    }
}
