//! Shared application state and edit-parameter model.

use leptos::*;
use leptos::batch;
use std::rc::Rc;
use wasm_bindgen::JsCast;

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
    Wand,
}

impl SelectTool {
    pub const ALL: [SelectTool; 3] = [SelectTool::Rect, SelectTool::Lasso, SelectTool::Wand];

    pub fn label(self) -> &'static str {
        match self {
            SelectTool::Rect => "Rect",
            SelectTool::Lasso => "Lasso",
            SelectTool::Wand => "Wand",
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
    /// Clockwise rotation in degrees about the (x, y) anchor.
    pub angle: f32,
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
            angle: 0.0,
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

/// Stamp shape for a brush layer. Smooth is the classic continuous stroke;
/// the rest stamp a dab every `spacing * width` along the stroke.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BrushShape {
    Smooth,
    Square,
    Dot,
    Triangle,
    /// User-supplied SVG path drawn in a 100x100 box, centered on (50, 50),
    /// pointing +x (rotated to the stroke direction).
    Custom,
}

impl BrushShape {
    pub const ALL: [BrushShape; 5] = [
        BrushShape::Smooth,
        BrushShape::Square,
        BrushShape::Dot,
        BrushShape::Triangle,
        BrushShape::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BrushShape::Smooth => "Smooth",
            BrushShape::Square => "Square",
            BrushShape::Dot => "Dot",
            BrushShape::Triangle => "Triangle",
            BrushShape::Custom => "Custom",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct BrushLayer {
    pub strokes: Vec<BrushStroke>,
    pub color: String,
    pub width: f32,
    pub shape: BrushShape,
    /// Distance between dabs as a multiple of brush width (0.1..4).
    /// Ignored for Smooth.
    pub spacing: f32,
    /// SVG path data for BrushShape::Custom (100x100 box).
    pub custom_path: String,
    /// Per-layer undo stack of stroke states.
    pub history: Vec<Vec<BrushStroke>>,
}

impl Default for BrushLayer {
    fn default() -> Self {
        BrushLayer {
            strokes: Vec::new(),
            color: "#0a84ff".into(),
            width: 0.015,
            shape: BrushShape::Smooth,
            spacing: 1.0,
            custom_path: "M50 5 L61 39 L98 39 L68 60 L79 95 L50 73 L21 95 L32 60 L2 39 L39 39 Z"
                .into(),
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
    /// Rc so cloning a MediaItem (every AppState::current read) is cheap;
    /// pixels are immutable after layer creation.
    pub pixels: Rc<Vec<u8>>,
    pub width: usize,
    pub height: usize,
    /// Center anchor in normalized canvas coords.
    pub x: f32,
    pub y: f32,
    /// Drawn width as a fraction of canvas width; height follows the
    /// buffer's aspect ratio. 1.0 = full-bleed.
    pub scale: f32,
}

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
            kind: LayerKind::Raster(RasterLayer { pixels: Rc::new(pixels), width, height, x: 0.5, y: 0.5, scale: 1.0 }),
        }
    }

    pub fn label(&self) -> String {
        match &self.kind {
            LayerKind::Text(t) => format!("T: {}", t.text),
            LayerKind::Path(_) => "Path".into(),
            LayerKind::Brush(_) => "Brush".into(),
            LayerKind::Raster(_) => "Image".into(),
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
    /// Gaussian blur amount 0..=100 (0 = off), maps to sigma 0..8 px.
    pub blur: f32,
    /// Film grain amount 0..=100 (0 = off).
    pub grain: f32,
    /// Gaussian (true) vs uniform (false) grain distribution.
    pub grain_gaussian: bool,
    /// Same noise on all channels (luma grain) vs per-channel color noise.
    pub grain_mono: bool,
    /// Levels (black/gamma/white per channel + composite). Identity = off.
    pub levels: crate::levels::LevelsSettings,
    /// Tone curves per channel + composite. Identity = off.
    pub curves: crate::curves::CurvesSettings,
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
            levels: crate::levels::LevelsSettings::default(),
            curves: crate::curves::CurvesSettings::default(),
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

// --- Parametric edit recipes (Drive manifest persistence) ---------------------

fn aspect_wire(a: Aspect) -> &'static str {
    match a {
        Aspect::Original => "original",
        Aspect::Nine16 => "9:16",
        Aspect::One1 => "1:1",
        Aspect::Four5 => "4:5",
        Aspect::Sixteen9 => "16:9",
    }
}

/// Serialize edit params to a recipe JSON string for the Drive manifest's
/// edits map. Identity fields are omitted to keep the manifest small; returns
/// None when every param is at its default (no entry needed). Selection masks
/// and layers are out of scope for recipes (pixel/stroke data).
pub fn edit_recipe_json(e: &EditParams) -> Option<String> {
    let d = EditParams::default();
    let mut parts: Vec<String> = Vec::new();
    if e.aspect != d.aspect {
        parts.push(format!("\"aspect\":\"{}\"", aspect_wire(e.aspect)));
    }
    if e.crop != d.crop {
        parts.push(format!(
            "\"crop\":{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            e.crop.x, e.crop.y, e.crop.w, e.crop.h
        ));
    }
    if e.rot90 != d.rot90 {
        parts.push(format!("\"rot90\":{}", e.rot90));
    }
    if e.fine_angle != d.fine_angle {
        parts.push(format!("\"fine_angle\":{}", e.fine_angle));
    }
    if e.brightness != d.brightness {
        parts.push(format!("\"brightness\":{}", e.brightness));
    }
    if e.contrast != d.contrast {
        parts.push(format!("\"contrast\":{}", e.contrast));
    }
    if e.saturation != d.saturation {
        parts.push(format!("\"saturation\":{}", e.saturation));
    }
    if e.warmth != d.warmth {
        parts.push(format!("\"warmth\":{}", e.warmth));
    }
    if e.blur != d.blur {
        parts.push(format!("\"blur\":{}", e.blur));
    }
    if e.grain != d.grain {
        parts.push(format!("\"grain\":{}", e.grain));
    }
    if e.grain_gaussian != d.grain_gaussian {
        parts.push(format!("\"grain_gaussian\":{}", e.grain_gaussian));
    }
    if e.grain_mono != d.grain_mono {
        parts.push(format!("\"grain_mono\":{}", e.grain_mono));
    }
    if !e.levels.is_identity() {
        let chans: Vec<String> = e
            .levels
            .ranges
            .iter()
            .map(|r| {
                format!(
                    "[{},{},{},{},{}]",
                    r.black, r.gamma, r.white, r.output_black, r.output_white
                )
            })
            .collect();
        parts.push(format!("\"levels\":[{}]", chans.join(",")));
    }
    if !e.curves.is_identity() {
        let chans: Vec<String> = e
            .curves
            .channels
            .iter()
            .map(|pts| {
                let ps: Vec<String> =
                    pts.iter().map(|p| format!("[{},{}]", p.x, p.y)).collect();
                format!("[{}]", ps.join(","))
            })
            .collect();
        parts.push(format!("\"curves\":[{}]", chans.join(",")));
    }
    if e.trim != d.trim {
        if let Some((a, b)) = e.trim {
            parts.push(format!("\"trim\":[{},{}]", a, b));
        }
    }
    if e.keep_audio != d.keep_audio {
        parts.push(format!("\"keep_audio\":{}", e.keep_audio));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("{{\"v\":1,{}}}", parts.join(",")))
    }
}

fn jget(v: &wasm_bindgen::JsValue, key: &str) -> Option<wasm_bindgen::JsValue> {
    let r = js_sys::Reflect::get(v, &wasm_bindgen::JsValue::from_str(key)).ok()?;
    if r.is_undefined() || r.is_null() {
        None
    } else {
        Some(r)
    }
}

fn jnum(v: &wasm_bindgen::JsValue, key: &str) -> Option<f64> {
    jget(v, key)?.as_f64()
}

fn jbool(v: &wasm_bindgen::JsValue, key: &str) -> Option<bool> {
    jget(v, key)?.as_bool()
}

/// Parse a recipe JSON string back into EditParams. Unknown/missing fields
/// fall back to defaults; malformed input returns None (caller treats it as
/// "no recipe"). Selection is always None — masks aren't serialized.
pub fn edit_params_from_recipe(json: &str) -> Option<EditParams> {
    let v = js_sys::JSON::parse(json).ok()?;
    let mut e = EditParams::default();
    if let Some(s) = jget(&v, "aspect").and_then(|a| a.as_string()) {
        e.aspect = match s.as_str() {
            "9:16" => Aspect::Nine16,
            "1:1" => Aspect::One1,
            "4:5" => Aspect::Four5,
            "16:9" => Aspect::Sixteen9,
            _ => Aspect::Original,
        };
    }
    if let Some(c) = jget(&v, "crop") {
        e.crop = CropRect {
            x: jnum(&c, "x").unwrap_or(0.0) as f32,
            y: jnum(&c, "y").unwrap_or(0.0) as f32,
            w: jnum(&c, "w").unwrap_or(1.0) as f32,
            h: jnum(&c, "h").unwrap_or(1.0) as f32,
        };
    }
    if let Some(n) = jnum(&v, "rot90") {
        e.rot90 = (n as u8).min(3);
    }
    if let Some(n) = jnum(&v, "fine_angle") {
        e.fine_angle = n as f32;
    }
    if let Some(n) = jnum(&v, "brightness") {
        e.brightness = n as f32;
    }
    if let Some(n) = jnum(&v, "contrast") {
        e.contrast = n as f32;
    }
    if let Some(n) = jnum(&v, "saturation") {
        e.saturation = n as f32;
    }
    if let Some(n) = jnum(&v, "warmth") {
        e.warmth = n as f32;
    }
    if let Some(n) = jnum(&v, "blur") {
        e.blur = n as f32;
    }
    if let Some(n) = jnum(&v, "grain") {
        e.grain = n as f32;
    }
    if let Some(b) = jbool(&v, "grain_gaussian") {
        e.grain_gaussian = b;
    }
    if let Some(b) = jbool(&v, "grain_mono") {
        e.grain_mono = b;
    }
    if let Some(l) = jget(&v, "levels") {
        if js_sys::Array::is_array(&l) {
            let arr: js_sys::Array = l.unchecked_into();
            if arr.length() == 4 {
                let mut ranges = [crate::levels::LevelRange::default(); 4];
                let mut ok = true;
                for (i, chan) in arr.iter().enumerate() {
                    let row: js_sys::Array = chan.unchecked_into();
                    if row.length() != 5 {
                        ok = false;
                        break;
                    }
                    let n = |j: u32| row.get(j).as_f64().unwrap_or_default();
                    ranges[i] = crate::levels::LevelRange {
                        black: n(0),
                        gamma: n(1),
                        white: n(2),
                        output_black: n(3),
                        output_white: n(4),
                    };
                }
                if ok {
                    e.levels = crate::levels::LevelsSettings { ranges };
                }
            }
        }
    }
    if let Some(c) = jget(&v, "curves") {
        if js_sys::Array::is_array(&c) {
        let arr: js_sys::Array = c.unchecked_into();
        if arr.length() == 4 {
            let mut channels: [Vec<crate::curves::CurvePoint>; 4] =
                [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
            let mut ok = true;
            for (i, chan) in arr.iter().enumerate() {
                let pts: js_sys::Array = chan.unchecked_into();
                for p in pts.iter() {
                    let pair: js_sys::Array = p.unchecked_into();
                    if pair.length() != 2 {
                        ok = false;
                        break;
                    }
                    channels[i].push(crate::curves::CurvePoint {
                        x: pair.get(0).as_f64().unwrap_or_default(),
                        y: pair.get(1).as_f64().unwrap_or_default(),
                    });
                }
                if !ok {
                    break;
                }
            }
            let parsed = crate::curves::CurvesSettings { channels };
            if ok && parsed.is_valid() {
                e.curves = parsed;
            }
        }
        }
    }
    if let Some(t) = jget(&v, "trim") {
        if js_sys::Array::is_array(&t) {
            let arr: js_sys::Array = t.unchecked_into();
            if arr.length() == 2 {
                if let (Some(a), Some(b)) = (arr.get(0).as_f64(), arr.get(1).as_f64()) {
                    e.trim = Some((a as f32, b as f32));
                }
            }
        }
    }
    if let Some(b) = jbool(&v, "keep_audio") {
        e.keep_audio = b;
    }
    e.selection = None;
    Some(e)
}

#[derive(Clone, PartialEq)]
pub enum MediaKind {
    Photo,
    Video,
}

#[derive(Clone, Copy, PartialEq)]
pub enum PhotoFormat {
    Jpeg,
    JpegMax,
    Png,
}

#[derive(Clone)]
pub struct MediaItem {
    pub id: usize,
    pub kind: MediaKind,
    pub name: String,
    /// Object URL for the source blob (photo preview / video element).
    pub object_url: String,
    /// Small (<=256px) thumbnail blob URL for the filmstrip, so tiles don't
    /// decode the full-resolution source.
    pub thumb_url: String,
    /// Full-res RGBA for photos (loaded lazily).
    pub width: usize,
    pub height: usize,
    pub edit: EditParams,
    pub layers: Vec<Layer>,
    pub next_layer_id: usize,
    /// EXIF TIFF payload carried from the source file into exports.
    pub exif: Option<std::rc::Rc<Vec<u8>>>,
    /// Drive file id when the item was imported from (or saved to) Drive.
    pub drive_file_id: Option<String>,
    /// Id of the Drive folder containing the file, when imported from Drive.
    pub drive_parent_id: Option<String>,
    /// True when the Drive copy already has the edits baked into its pixels
    /// (pes_edited=1 import, or set after any successful save) — parametric
    /// recipes are neither applied nor written for baked items, so a reopen
    /// never double-applies an edit.
    pub drive_baked: bool,
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
    /// Magic wand options-bar settings.
    pub wand_tolerance: RwSignal<i32>,
    pub wand_contiguous: RwSignal<bool>,
    /// Spot-heal brush: radius as a fraction of the image diagonal.
    pub heal_radius: RwSignal<f32>,
    pub heal_mode: RwSignal<crate::heal::HealMode>,
    /// Clone stamp source point, normalized geometry coords.
    pub clone_source: RwSignal<Option<(f32, f32)>>,
    /// Fixed stroke offset once an aligned stroke has started.
    pub clone_offset: RwSignal<Option<(f32, f32)>>,
    /// Aligned: source follows the brush across strokes; off: every stroke
    /// restarts from the source point.
    pub clone_aligned: RwSignal<bool>,
    /// Set-source pick mode for the next canvas click.
    pub clone_pick: RwSignal<bool>,
    /// Clone brush radius as a fraction of the image diagonal.
    pub clone_radius: RwSignal<f32>,
    /// Photo export format.
    pub photo_format: RwSignal<PhotoFormat>,
    /// Google Drive access token (memory only — re-auth with one click).
    pub drive_token: RwSignal<Option<std::rc::Rc<String>>>,
    /// Granted Drive folder: (id, display name).
    pub drive_folder: RwSignal<Option<(String, String)>>,
    /// Files listed from the granted Drive folder.
    pub drive_files: RwSignal<Vec<crate::drive::DriveFile>>,
    /// Subfolders of the folder currently being browsed.
    pub drive_subfolders: RwSignal<Vec<crate::drive::SubFolder>>,
    /// Breadcrumb below the granted root: (id, name) per descended level.
    pub drive_path: RwSignal<Vec<(String, String)>>,
    /// Multi-select set for batch import (Drive file ids).
    pub drive_selected: RwSignal<std::collections::HashSet<String>>,
    /// Shortlisted (starred) Drive file ids, synced via the manifest file.
    pub drive_shortlist: RwSignal<std::collections::HashSet<String>>,
    /// Parametric edit recipes from the manifest: Drive file id → raw recipe
    /// JSON string. Synced alongside the shortlist in the same debounced write.
    pub drive_edits: RwSignal<std::collections::HashMap<String, String>>,
    /// Drive id of the shortlist manifest, when it exists.
    pub drive_manifest_id: RwSignal<Option<String>>,
    /// Bumped on every manifest-affecting change; the debounced saver only
    /// writes the latest.
    pub drive_save_gen: RwSignal<u32>,
    /// True while a debounced manifest write is outstanding — the remote
    /// manifest is stale in that window, so drive_refresh must not apply it.
    pub drive_save_pending: RwSignal<bool>,
    /// Bumped when the hi-res preview cache finishes rebuilding; the canvas
    /// render effect listens and swaps the sharp frame in.
    pub hi_built: RwSignal<u32>,
    /// Bumped on window resize so the canvas backing can track display size.
    pub viewport_gen: RwSignal<u32>,
    pub drive_filter: RwSignal<DriveFilter>,
    pub drive_sort: RwSignal<DriveSort>,
    /// Grid tile size in px.
    pub drive_thumb_px: RwSignal<u32>,
    /// Index into the visible (filtered+sorted) file list while in loupe view.
    pub drive_loupe: RwSignal<Option<usize>>,
    /// Recently viewed loupe file ids (most-recent last, capped) — kept as
    /// hidden <img> elements so their large thumbnails stay decoded in memory.
    pub drive_loupe_seen: RwSignal<Vec<String>>,
    /// Hover-zoom preview: (file id, cursor x, cursor y) while hovering a tile.
    pub drive_hover: RwSignal<Option<(String, f64, f64)>>,
    /// Bumped on hover enter/leave; only the latest generation opens a preview.
    pub drive_hover_gen: RwSignal<u32>,
    /// Last Drive error to surface in the panel.
    pub drive_error: RwSignal<Option<String>>,
    /// Whether the Drive panel is expanded.
    pub drive_open: RwSignal<bool>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum DriveFilter {
    All,
    Starred,
    Unstarred,
}

#[derive(Clone, Copy, PartialEq)]
pub enum DriveSort {
    DateDesc,
    NameAsc,
}

impl DriveSort {
    pub fn order_by(self) -> &'static str {
        match self {
            DriveSort::DateDesc => "modifiedTime desc",
            DriveSort::NameAsc => "name",
        }
    }
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
            wand_tolerance: create_rw_signal(32),
            wand_contiguous: create_rw_signal(true),
            heal_radius: create_rw_signal(0.02),
            heal_mode: create_rw_signal(crate::heal::HealMode::ContentAware),
            clone_source: create_rw_signal(None),
            clone_offset: create_rw_signal(None),
            clone_aligned: create_rw_signal(true),
            clone_pick: create_rw_signal(false),
            clone_radius: create_rw_signal(0.03),
            photo_format: create_rw_signal(PhotoFormat::Jpeg),
            drive_token: create_rw_signal(None),
            drive_folder: create_rw_signal(crate::drive::saved_folder()),
            drive_files: create_rw_signal(Vec::new()),
            drive_subfolders: create_rw_signal(Vec::new()),
            drive_path: create_rw_signal(Vec::new()),
            drive_selected: create_rw_signal(std::collections::HashSet::new()),
            drive_shortlist: create_rw_signal(std::collections::HashSet::new()),
            drive_edits: create_rw_signal(std::collections::HashMap::new()),
            drive_manifest_id: create_rw_signal(None),
            drive_save_gen: create_rw_signal(0),
            drive_save_pending: create_rw_signal(false),
            hi_built: create_rw_signal(0),
            viewport_gen: create_rw_signal(0),
            drive_filter: create_rw_signal(DriveFilter::All),
            drive_sort: create_rw_signal(DriveSort::DateDesc),
            drive_thumb_px: create_rw_signal(88),
            drive_loupe: create_rw_signal(None),
            drive_loupe_seen: create_rw_signal(Vec::new()),
            drive_hover: create_rw_signal(None),
            drive_hover_gen: create_rw_signal(0),
            drive_error: create_rw_signal(None),
            drive_open: create_rw_signal(false),
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
