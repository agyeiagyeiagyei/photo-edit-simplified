//! Hi-res display render, layer compositing, render fingerprints/crop helpers and retouch actions.

use crate::prelude::*;
use crate::*;

/// Worker id + pixel dims to render from: sensor pixels when loaded, else the
/// display proxy.
pub(crate) fn render_source(id: usize, photo: &PhotoData) -> (usize, (usize, usize)) {
    if photo.full {
        (id, photo.full_dims)
    } else if let Some(p) = proxy_data(id) {
        (proxy_wid(id), (p.w, p.h))
    } else {
        (id, photo.full_dims)
    }
}

pub(crate) fn draw_text_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Text(t) = &layer.kind else { return; };
    let px = (t.font_size * h as f32) as f64;
    ctx.set_font(&format!(
        "{} {}px \"{}\", sans-serif",
        t.font_weight, px, t.font_family
    ));
    ctx.set_text_align(t.alignment.canvas_value());
    ctx.set_text_baseline("middle");

    let x = (t.x * w as f32) as f64;
    let y = (t.y * h as f32) as f64;

    ctx.save();
    if t.angle != 0.0 {
        let _ = ctx.translate(x, y);
        let _ = ctx.rotate((t.angle as f64).to_radians());
        let _ = ctx.translate(-x, -y);
    }

    ctx.set_shadow_color(&t.shadow_color);
    ctx.set_shadow_blur(t.shadow_blur as f64);
    ctx.set_shadow_offset_x(t.shadow_offset_x as f64);
    ctx.set_shadow_offset_y(t.shadow_offset_y as f64);

    if t.stroke_width > 0.0 {
        ctx.set_line_width(t.stroke_width as f64 * px);
        ctx.set_stroke_style_str(&t.stroke_color);
        let _ = ctx.stroke_text(&t.text, x, y);
    }

    ctx.set_fill_style_str(&t.color);
    let _ = ctx.fill_text(&t.text, x, y);

    ctx.restore();
    ctx.set_shadow_color("transparent");
    ctx.set_shadow_blur(0.0);
    ctx.set_shadow_offset_x(0.0);
    ctx.set_shadow_offset_y(0.0);
}

pub(crate) fn draw_path_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Path(p) = &layer.kind else { return; };
    if p.points.len() < 2 {
        return;
    }
    ctx.begin_path();
    let first = &p.points[0];
    ctx.move_to((first.x * w as f32) as f64, (first.y * h as f32) as f64);
    for i in 1..p.points.len() {
        let prev = &p.points[i - 1];
        let curr = &p.points[i];
        let c1x = (prev.x + prev.out_x) * w as f32;
        let c1y = (prev.y + prev.out_y) * h as f32;
        let c2x = (curr.x + curr.in_x) * w as f32;
        let c2y = (curr.y + curr.in_y) * h as f32;
        let x = curr.x * w as f32;
        let y = curr.y * h as f32;
        ctx.bezier_curve_to(c1x as f64, c1y as f64, c2x as f64, c2y as f64, x as f64, y as f64);
    }
    if p.closed && p.points.len() > 2 {
        let last = p.points.last().unwrap();
        let first = &p.points[0];
        let c1x = (last.x + last.out_x) * w as f32;
        let c1y = (last.y + last.out_y) * h as f32;
        let c2x = (first.x + first.in_x) * w as f32;
        let c2y = (first.y + first.in_y) * h as f32;
        let x = first.x * w as f32;
        let y = first.y * h as f32;
        ctx.bezier_curve_to(c1x as f64, c1y as f64, c2x as f64, c2y as f64, x as f64, y as f64);
        ctx.close_path();
    }

    let stroke_px = (p.stroke_width * h as f32) as f64;
    if !p.fill_color.is_empty() && p.fill_color != "none" {
        ctx.set_fill_style_str(&p.fill_color);
        ctx.fill();
    }
    if stroke_px > 0.0 {
        ctx.set_line_width(stroke_px);
        ctx.set_stroke_style_str(&p.stroke_color);
        ctx.set_line_join("round");
        ctx.stroke();
    }
}

pub(crate) fn draw_brush_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Brush(b) = &layer.kind else { return; };
    let size_px = (b.width * h as f32) as f64;
    if b.shape == state::BrushShape::Smooth {
        ctx.set_stroke_style_str(&b.color);
        ctx.set_line_width(size_px);
        ctx.set_line_cap("round");
        ctx.set_line_join("round");
        for stroke in &b.strokes {
            if stroke.points.len() < 2 {
                continue;
            }
            ctx.begin_path();
            let (x0, y0) = stroke.points[0];
            ctx.move_to((x0 * w as f32) as f64, (y0 * h as f32) as f64);
            for &(x, y) in &stroke.points[1..] {
                ctx.line_to((x * w as f32) as f64, (y * h as f32) as f64);
            }
            ctx.stroke();
        }
        return;
    }

    ctx.set_fill_style_str(&b.color);
    let spacing_px = (b.spacing.clamp(0.1, 4.0) * b.width * h as f32).max(0.5);
    let custom = if b.shape == state::BrushShape::Custom {
        web_sys::Path2d::new_with_path_string(&b.custom_path).ok()
    } else {
        None
    };
    let half = size_px / 2.0;
    for stroke in &b.strokes {
        let pts: Vec<(f32, f32)> = stroke
            .points
            .iter()
            .map(|&(x, y)| (x * w as f32, y * h as f32))
            .collect();
        for dab in stroke::dab_positions(&pts, spacing_px) {
            ctx.save();
            ctx.translate(dab.x as f64, dab.y as f64).ok();
            ctx.rotate(dab.angle as f64).ok();
            match b.shape {
                state::BrushShape::Square => {
                    ctx.fill_rect(-half, -half, size_px, size_px);
                }
                state::BrushShape::Dot => {
                    ctx.begin_path();
                    ctx.arc(0.0, 0.0, half, 0.0, std::f64::consts::TAU).ok();
                    ctx.fill();
                }
                state::BrushShape::Triangle => {
                    ctx.begin_path();
                    ctx.move_to(half, 0.0);
                    ctx.line_to(half * -0.5, half * 0.866);
                    ctx.line_to(half * -0.5, half * -0.866);
                    ctx.close_path();
                    ctx.fill();
                }
                state::BrushShape::Custom => {
                    if let Some(path) = &custom {
                        ctx.scale(size_px / 100.0, size_px / 100.0).ok();
                        ctx.translate(-50.0, -50.0).ok();
                        ctx.fill_with_path_2d(path);
                    }
                }
                state::BrushShape::Smooth => unreachable!(),
            }
            ctx.restore();
        }
    }
}

pub(crate) fn draw_raster_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Raster(r) = &layer.kind else { return };
    if r.width == 0 || r.height == 0 {
        return;
    }
    let canvas = web::create_canvas(r.width as u32, r.height as u32);
    web::put_pixels(&canvas, &r.pixels, r.width as u32, r.height as u32);
    let dw = (r.scale * w as f32) as f64;
    let dh = dw * r.height as f64 / r.width as f64;
    let dx = (r.x as f64 * w) - dw / 2.0;
    let dy = (r.y as f64 * h) - dh / 2.0;
    let _ = ctx.draw_image_with_html_canvas_element_and_dw_and_dh(&canvas, dx, dy, dw, dh);
}

pub(crate) fn composite_layers(canvas: &web_sys::HtmlCanvasElement, layers: &[state::Layer], w: usize, h: usize) {
    let ctx = web::ctx2d(canvas);
    for layer in layers {
        if !layer.visible || layer.opacity <= 0.01 {
            continue;
        }
        ctx.set_global_alpha(layer.opacity as f64);
        match &layer.kind {
            LayerKind::Text(_) => draw_text_layer(&ctx, layer, w as f64, h as f64),
            LayerKind::Path(_) => draw_path_layer(&ctx, layer, w as f64, h as f64),
            LayerKind::Brush(_) => draw_brush_layer(&ctx, layer, w as f64, h as f64),
            LayerKind::Raster(_) => draw_raster_layer(&ctx, layer, w as f64, h as f64),
            LayerKind::Adjust(_) => {}
        }
        ctx.set_global_alpha(1.0);
    }
}

pub(crate) async fn render_text_overlay(t: &state::TextLayer, w: usize, h: usize) -> Result<web_sys::Blob, JsValue> {
    let canvas = web::create_canvas(w as u32, h as u32);
    let layer = state::Layer {
        id: 0,
        visible: true,
        opacity: 1.0,
        kind: state::LayerKind::Text(t.clone()),
    };
    draw_text_layer(&web::ctx2d(&canvas), &layer, w as f64, h as f64);
    web::canvas_to_blob(&canvas, "image/png").await
}

//
// The editing preview is a 1024px downscale; once the on-screen canvas is
// bigger than that (large windows, retina, tight crops) it visibly softens.
// When the preview underserves the display we render the whole working image
// from the full-res original at display resolution into this cache, and blit
// from it. Rebuilds are debounced so slider drags stay on the cheap path.

/// Ceiling for the hi-res long edge — past this, CPU edit ops cost more than
/// the sharpness is worth.
pub(crate) const HI_MAX_EDGE: usize = 3200;

/// The cropped view renders only its crop window, so that window can afford
/// native density far past the whole-frame cap.
pub(crate) const WINDOW_MAX_EDGE: usize = 4096;

pub(crate) struct HiCache {
    pub(crate) item_id: usize,
    pub(crate) edit_fp: u64,
    pub(crate) source_fp: u64,
    /// The frame, as a canvas (worker renders, proxy seeds) or as the decoded
    /// bitmap the reload prefetch produced — storing the bitmap skips a 27 MB
    /// draw on the restore path and blits straight to the visible canvas.
    pub(crate) canvas: Option<web_sys::HtmlCanvasElement>,
    pub(crate) bitmap: Option<web_sys::ImageBitmap>,
    pub(crate) w: usize,
    pub(crate) h: usize,
    /// Painted from the reload prefetch bitmap for the stored proxy, so a
    /// matching record decode may re-stamp `source_fp` instead of re-rendering.
    pub(crate) prefetched: bool,
    /// The pixels lack the item's RAW detail stage (denoise, sharpening): an
    /// unedited frame shown while the detailed render is requested. Treated
    /// like a stale source, so it is always replaced.
    pub(crate) stand_in: bool,
}

/// Blit a cached hi frame into the visible canvas — whole, or a source window
/// for the cropped view. The frame is either a canvas or (restore prefetch) an
/// ImageBitmap; both blit without a readback.
pub(crate) fn blit_hi_frame(
    hi: &HiCache,
    canvas: &web_sys::HtmlCanvasElement,
    crop: Option<(usize, usize, usize, usize)>,
) {
    let (cw, ch) = crop.map_or((hi.w as u32, hi.h as u32), |(_, _, w, h)| {
        (w as u32, h as u32)
    });
    canvas.set_width(cw);
    canvas.set_height(ch);
    let ctx = web::ctx2d(canvas);
    match (&hi.canvas, &hi.bitmap) {
        (Some(src), _) => {
            if let Some((sx, sy, sw, sh)) = crop {
                let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    src,
                    sx as f64,
                    sy as f64,
                    sw as f64,
                    sh as f64,
                    0.0,
                    0.0,
                    sw as f64,
                    sh as f64,
                );
            } else {
                let _ = ctx.draw_image_with_html_canvas_element(src, 0.0, 0.0);
            }
        }
        (None, Some(bmp)) => {
            if let Some((sx, sy, sw, sh)) = crop {
                let _ = ctx.draw_image_with_image_bitmap_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    bmp,
                    sx as f64,
                    sy as f64,
                    sw as f64,
                    sh as f64,
                    0.0,
                    0.0,
                    sw as f64,
                    sh as f64,
                );
            } else {
                let _ = ctx.draw_image_with_image_bitmap(bmp, 0.0, 0.0);
            }
        }
        (None, None) => {}
    }
}

/// Crop-window render for the cropped view: the canvas holds only the crop
/// region at display (up to native) density.
pub(crate) struct HiWindow {
    pub(crate) item_id: usize,
    pub(crate) edit_fp: u64,
    pub(crate) source_fp: u64,
    /// Crop rect bits (normalized x, y, w, h). The same normalized region is
    /// the same framing even when the source's pixel density changes, so a
    /// proxy→full swap keeps the old window on screen while it re-renders.
    pub(crate) crop: [u32; 4],
    pub(crate) canvas: web_sys::HtmlCanvasElement,
    pub(crate) w: usize,
    pub(crate) h: usize,
}

/// Which sharp frame currently backs the canvas, for mapping overlays from
/// preview working coordinates onto it.
pub(crate) enum Sharp {
    /// Whole working image from [`HiCache`].
    Whole { w: usize },
    /// Crop window from [`HiWindow`]; `cw` is the crop width in preview px.
    Window { w: usize, cw: usize },
}

pub(crate) fn crop_key(c: &state::CropRect) -> [u32; 4] {
    [c.x.to_bits(), c.y.to_bits(), c.w.to_bits(), c.h.to_bits()]
}

pub(crate) struct PreviewGeo {
    pub(crate) item_id: usize,
    pub(crate) gen: u64,
    pub(crate) rot90: u8,
    pub(crate) fine_bits: u32,
    pub(crate) pixels: Rc<Vec<u8>>,
    pub(crate) w: usize,
    pub(crate) h: usize,
}

thread_local! {
    pub(crate) static HI_CACHE: RefCell<Option<HiCache>> = const { RefCell::new(None) };
    /// Crop-window render for the cropped view (separate from the whole-frame
    /// cache: it holds a different region at a different density).
    pub(crate) static HI_WINDOW: RefCell<Option<HiWindow>> = const { RefCell::new(None) };
    /// Rotated/fine-angle preview, reused across colour-only renders. The
    /// identity transform is not cached — callers borrow the source directly.
    pub(crate) static PREVIEW_GEO: RefCell<Option<PreviewGeo>> = const { RefCell::new(None) };
    /// Generation guard for the debounced hi rebuild; only the latest fires.
    pub(crate) static HI_DEBOUNCE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Generation guard for an in-flight worker render; only the latest may
    /// land in HI_CACHE.
    pub(crate) static HI_REQ_GEN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Last nonzero canvas display need (CSS px × DPR). Reactive remounts
    /// briefly hand us a not-yet-inserted canvas with zero client size.
    pub(crate) static LAST_NEED: std::cell::Cell<(f64, f64)> = const { std::cell::Cell::new((0.0, 0.0)) };
    /// A trailing rAF pass is armed; further changes this frame only set DIRTY.
    pub(crate) static PREVIEW_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Changes arrived since the leading-edge paint; the trailing pass runs.
    pub(crate) static PREVIEW_DIRTY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Generation guard for the debounced GPU base request; only the latest fires.
    pub(crate) static GPU_DEBOUNCE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The base request in flight, so repeated effect passes don't queue more.
    pub(crate) static GPU_BASE_REQ: RefCell<Option<gpu::BaseKey>> = const { RefCell::new(None) };
}

/// Geometry applied to the 1024px preview, cached across renders.
pub(crate) fn preview_geometry_cached(
    item_id: usize,
    photo_gen: u64,
    preview: &(Vec<u8>, usize, usize),
    edit: &EditParams,
) -> Option<(Rc<Vec<u8>>, usize, usize)> {
    if edit.rot90.is_multiple_of(4) && edit.fine_angle.abs() <= 0.01 {
        return None;
    }
    let (rot90, fine_bits) = (edit.rot90, edit.fine_angle.to_bits());
    PREVIEW_GEO.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(g) = c.as_ref() {
            if g.item_id == item_id
                && g.gen == photo_gen
                && g.rot90 == rot90
                && g.fine_bits == fine_bits
            {
                return Some((g.pixels.clone(), g.w, g.h));
            }
        }
        let (p, w, h) = geometry(&preview.0, preview.1, preview.2, edit);
        let pixels = Rc::new(p.into_owned());
        *c = Some(PreviewGeo {
            item_id,
            gen: photo_gen,
            rot90,
            fine_bits,
            pixels: pixels.clone(),
            w,
            h,
        });
        Some((pixels, w, h))
    })
}

/// Edit identity for the hi caches: everything in the recipe and layer set
/// that changes rendered pixels EXCEPT the crop rect (crop only reframes the
/// blit) and the source generation (kept separate so a source swap can show
/// the stale frame while the new one renders).
fn hash_selection(h: &mut std::collections::hash_map::DefaultHasher, sel: Option<&state::Selection>) {
    use std::hash::{Hash, Hasher};
    match sel {
        None => h.write_u8(0),
        Some(sel) => {
            h.write_u32(sel.feather.to_bits());
            match &sel.kind {
                SelectionKind::Rect { x, y, w, h: rh } => {
                    h.write_u8(1);
                    for v in [*x, *y, *w, *rh] {
                        h.write_u32(v.to_bits());
                    }
                }
                SelectionKind::Lasso(pts) => {
                    h.write_u8(2);
                    pts.len().hash(h);
                    for p in pts.first().into_iter().chain(pts.last()) {
                        h.write_u32(p.0.to_bits());
                        h.write_u32(p.1.to_bits());
                    }
                }
                SelectionKind::Mask { data, width, height } => {
                    h.write_u8(3);
                    width.hash(h);
                    height.hash(h);
                    if !data.is_empty() {
                        data.len().hash(h);
                        for i in 0..8 {
                            h.write_u8(data[i * data.len() / 8]);
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn edit_fingerprint(item: &MediaItem) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let e = &item.edit;
    h.write_u8(e.rot90);
    for v in [e.fine_angle, e.brightness, e.contrast, e.saturation, e.warmth, e.blur, e.grain] {
        h.write_u32(v.to_bits());
    }
    for v in [
        e.raw.temp,
        e.raw.tint,
        e.raw.exposure,
        e.raw.highlights,
        e.raw.shadows,
        e.raw.noise,
        e.raw.denoise,
        e.raw.sharpen,
    ] {
        h.write_u32(v.to_bits());
    }
    h.write_u8(e.grain_gaussian as u8);
    h.write_u8(e.grain_mono as u8);
    match e.aspect.ratio() {
        Some((a, b)) => {
            h.write_u32(a.to_bits());
            h.write_u32(b.to_bits());
        }
        None => h.write_u8(0),
    }
    format!("{:?}", e.levels).hash(&mut h);
    format!("{:?}", e.curves).hash(&mut h);
    hash_selection(&mut h, e.selection.as_ref());
    item.layers.len().hash(&mut h);
    for l in &item.layers {
        l.id.hash(&mut h);
        l.visible.hash(&mut h);
        h.write_u32(l.opacity.to_bits());
        match &l.kind {
            LayerKind::Text(t) => {
                h.write_u8(1);
                t.text.hash(&mut h);
                t.font_family.hash(&mut h);
                t.color.hash(&mut h);
                t.stroke_color.hash(&mut h);
                t.shadow_color.hash(&mut h);
                t.font_weight.hash(&mut h);
                std::mem::discriminant(&t.alignment).hash(&mut h);
                for v in [t.x, t.y, t.font_size, t.angle, t.stroke_width, t.shadow_blur, t.shadow_offset_x, t.shadow_offset_y] {
                    h.write_u32(v.to_bits());
                }
            }
            LayerKind::Path(p) => {
                h.write_u8(2);
                p.points.len().hash(&mut h);
                p.closed.hash(&mut h);
                p.fill_color.hash(&mut h);
                p.stroke_color.hash(&mut h);
                h.write_u32(p.stroke_width.to_bits());
                for pt in p.points.first().into_iter().chain(p.points.last()) {
                    h.write_u32(pt.x.to_bits());
                    h.write_u32(pt.y.to_bits());
                }
            }
            LayerKind::Brush(b) => {
                h.write_u8(3);
                b.strokes.len().hash(&mut h);
                b.color.hash(&mut h);
                b.custom_path.hash(&mut h);
                std::mem::discriminant(&b.shape).hash(&mut h);
                h.write_u32(b.width.to_bits());
                h.write_u32(b.spacing.to_bits());
                if let Some(s) = b.strokes.last() {
                    s.points.len().hash(&mut h);
                    if let Some(p) = s.points.last() {
                        h.write_u32(p.0.to_bits());
                        h.write_u32(p.1.to_bits());
                    }
                }
            }
            LayerKind::Raster(r) => {
                h.write_u8(4);
                r.width.hash(&mut h);
                r.height.hash(&mut h);
                h.write_u32(r.x.to_bits());
                h.write_u32(r.y.to_bits());
                h.write_u32(r.scale.to_bits());
                if !r.pixels.is_empty() {
                    r.pixels.len().hash(&mut h);
                    for i in 0..8 {
                        h.write_u8(r.pixels[i * r.pixels.len() / 8]);
                    }
                }
            }
            LayerKind::Adjust(a) => {
                h.write_u8(5);
                for v in [a.adjust.brightness, a.adjust.contrast, a.adjust.saturation, a.adjust.warmth] {
                    h.write_u32(v.to_bits());
                }
                format!("{:?}", a.adjust.levels).hash(&mut h);
                format!("{:?}", a.adjust.curves).hash(&mut h);
                a.invert.hash(&mut h);
                // Every mask change bumps the version; the selection hash
                // only samples 8 bytes of a raster mask, so two wand masks of
                // the same size can collide without it.
                a.mask_version.hash(&mut h);
                hash_selection(&mut h, a.mask.as_ref());
            }
        }
    }
    h.finish()
}

/// Identity of the pixels a render was produced from: the preview generation
/// and the RAW proxy generation.
pub(crate) fn source_fingerprint(photo_gen: u64, proxy_gen: u64) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u64(photo_gen);
    h.write_u64(proxy_gen);
    h.finish()
}

pub(crate) fn video_export_dims(edit: &EditParams, w: usize, h: usize) -> (usize, usize, usize, usize, usize, usize) {
    let (mut cw, mut ch) = (w, h);
    if edit.rot90 % 2 == 1 {
        std::mem::swap(&mut cw, &mut ch);
    }
    if edit.fine_angle.abs() > 0.01 {
        let (iw, ih) = working_dims(w, h, edit);
        cw = iw;
        ch = ih;
    }
    let (_x, _y, px_w, px_h) = crop_px(edit, cw, ch);
    let (ew, eh) = edit.aspect.export_dims(px_w, px_h);
    (cw, ch, px_w, px_h, ew, eh)
}

/// iPhone-style crop zoom: scale the crop rect about a fixed screen point
/// (pinch centroid / cursor), then translate by the centroid's movement.
/// Zooming in shrinks the crop rect — the photo appears to enlarge inside
/// the frame. `ratio` locks the rect's on-image aspect when set.
pub(crate) fn zoom_crop(
    orig: state::CropRect,
    zoom_from: (f32, f32),
    pan: (f32, f32),
    scale: f32,
    ratio: Option<f32>,
    img_aspect: f32,
) -> state::CropRect {
    let scale = scale.clamp(0.1, 16.0);
    let (w, h) = match ratio {
        Some(r) => {
            let w = (orig.w / scale).clamp(0.02, 1.0f32.min(r / img_aspect));
            (w, w * img_aspect / r)
        }
        None => (
            (orig.w / scale).clamp(0.02, 1.0),
            (orig.h / scale).clamp(0.02, 1.0),
        ),
    };
    let x = (zoom_from.0 - (zoom_from.0 - orig.x) * (w / orig.w) + pan.0).clamp(0.0, 1.0 - w);
    let y = (zoom_from.1 - (zoom_from.1 - orig.y) * (h / orig.h) + pan.1).clamp(0.0, 1.0 - h);
    state::CropRect { x, y, w, h }
}

/// Largest centered exact-ratio box inside a crop box (preview and developed dims can drift).
pub(crate) fn aspect_fit(cx: usize, cy: usize, cw: usize, ch: usize, aspect: Aspect) -> (usize, usize, usize, usize) {
    let Some((rw, rh)) = aspect.ratio() else {
        return (cx, cy, cw, ch);
    };
    let r = rw as f64 / rh as f64;
    let by_h = (ch as f64 * r).round().max(1.0) as usize;
    let (nw, nh) = if by_h <= cw {
        (by_h, ch)
    } else {
        let by_w = ((cw as f64 / r).round().max(1.0) as usize).min(ch);
        (cw, by_w)
    };
    (cx + (cw - nw) / 2, cy + (ch - nh) / 2, nw, nh)
}

pub(crate) fn selection_to_layer(state: AppState, cut: bool) {
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    if item.edit.selection.is_none() {
        return;
    }
    if !full_present(item.id) {
        ensure_full(state, item.id, Some(Box::new(move || selection_to_layer(state, cut))));
        return;
    }
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            let Some(sel) = item.edit.selection.clone() else { return false };
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let item_id = item.id;
            pworker::extract(item_id, &recipe, &sel, cut, move |res| {
                match res {
                    Ok(Some(r)) => {
                        state.items.update(|v| {
                            if let Some(m) = v.iter_mut().find(|m| m.id == item_id) {
                                let nid = m.next_layer_id;
                                m.next_layer_id += 1;
                                state::insert_overlay(&mut m.layers, Layer::new_raster(nid, r.layer, r.lw, r.lh));
                            }
                        });
                        if let Some(c) = r.cut {
                            destructive_apply(state, item_id, c);
                        }
                    }
                    Ok(None) => {}
                    Err(()) => {}
                }
            })
        },
    );
}

pub(crate) fn delete_selection(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    if item.edit.selection.is_none() {
        return;
    }
    if !full_present(item.id) {
        ensure_full(state, item.id, Some(Box::new(move || delete_selection(state))));
        return;
    }
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            let Some(sel) = item.edit.selection.clone() else { return false };
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let item_id = item.id;
            pworker::cut(item_id, &recipe, &sel, move |res| {
                match res {
                    Ok(Some(r)) => destructive_apply(state, item_id, r),
                    Ok(None) => {}
                    Err(()) => {}
                }
            })
        },
    );
}

/// Content-aware fill: synthesize the selection from surrounding texture
/// (PatchMatch-style), destructive on the base pixels like delete_selection.
pub(crate) fn content_fill_selection(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    if item.edit.selection.is_none() {
        return;
    }
    if !full_present(item.id) {
        ensure_full(state, item.id, Some(Box::new(move || content_fill_selection(state))));
        return;
    }
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            let Some(sel) = item.edit.selection.clone() else { return false };
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let item_id = item.id;
            pworker::fill(item_id, &recipe, &sel, move |res| {
                match res {
                    Ok(Some(r)) => destructive_apply(state, item_id, r),
                    Ok(None) => {}
                    Err(()) => {}
                }
            })
        },
    );
}

/// Swap an item's cached pixels after a destructive op (worker or sync).
/// Rotation is baked into the new pixels, so it resets; the crop rect is a
/// render-time window over the working frame and is deliberately preserved.
pub(crate) fn destructive_apply(state: AppState, item_id: usize, r: pworker::DestructiveResult) {
    camera_loupe_invalidate(item_id);
    CACHE.with(|c| {
        if let Some(pd) = c.borrow_mut().photos.get_mut(&item_id) {
            let pd = Rc::make_mut(pd);
            // The worker committed the result into its own buffer; only the
            // dimensions and flags change here.
            pd.full = true;
            pd.full_dims = (r.w, r.h);
            pd.preview = (r.preview, r.pw, r.ph);
            pd.dirty = true;
            pd.neutral = false;
            pd.gen += 1;
        }
    });
    state.items.update(|v| {
        if let Some(m) = v.iter_mut().find(|m| m.id == item_id) {
            m.edit.rot90 = 0;
            m.edit.fine_angle = 0.0;
            m.edit.selection = None;
        }
    });
}

pub(crate) fn heal_mode_u8(m: heal::HealMode) -> u8 {
    match m {
        heal::HealMode::ContentAware => 0,
        heal::HealMode::SmoothFill => 1,
        heal::HealMode::ProximityMatch => 2,
    }
}

/// Spot heal: stamp soft discs along the stroke (normalized coords) into a
/// coverage mask at full-res, run the patch-synthesis solver, and swap the
/// cached pixels — destructive, mirroring delete_selection.
pub(crate) fn apply_heal(state: AppState, pts: &[(f32, f32)]) {
    if pts.is_empty() {
        return;
    }
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    if !full_present(item.id) {
        let pts = pts.to_vec();
        ensure_full(state, item.id, Some(Box::new(move || apply_heal(state, &pts))));
        return;
    }
    let pts = pts.to_vec();
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            if item.kind != MediaKind::Photo {
                return false;
            }
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let radius = state.heal_radius.get_untracked();
            let mode = heal_mode_u8(state.heal_mode.get_untracked());
            let item_id = item.id;
            pworker::heal(item_id, &recipe, &pts, radius, mode, move |res| {
                match res {
                    Ok(Some(r)) => destructive_apply(state, item_id, r),
                    Ok(None) => {}
                    Err(()) => {}
                }
            })
        },
    );
}

/// Clone stamp: copy from the source point through the stroke's coverage mask
/// at full-res. Aligned strokes keep the first stroke's offset; unaligned
/// strokes re-derive it from the source each time. Destructive like apply_heal.
pub(crate) fn apply_clone(state: AppState, pts: &[(f32, f32)]) {
    if pts.is_empty() {
        return;
    }
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    if state.clone_source.get_untracked().is_none() {
        return;
    }
    if !full_present(item.id) {
        let pts = pts.to_vec();
        ensure_full(state, item.id, Some(Box::new(move || apply_clone(state, &pts))));
        return;
    }
    let pts = pts.to_vec();
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            if item.kind != MediaKind::Photo {
                return false;
            }
            let Some(photo) = get_photo(item.id) else { return false };
            let Some(src) = state.clone_source.get_untracked() else { return false };
            let aligned = state.clone_aligned.get_untracked();
            let offset = if aligned {
                match state.clone_offset.get_untracked() {
                    Some(off) => off,
                    None => {
                        let off = (src.0 - pts[0].0, src.1 - pts[0].1);
                        state.clone_offset.set(Some(off));
                        off
                    }
                }
            } else {
                (src.0 - pts[0].0, src.1 - pts[0].1)
            };
            let (ww, wh) = working_dims(photo.full_dims.0, photo.full_dims.1, &item.edit);
            let dx = (offset.0 * ww as f32).round() as i32;
            let dy = (offset.1 * wh as f32).round() as i32;
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let radius = state.clone_radius.get_untracked();
            let item_id = item.id;
            pworker::clone_stamp(item_id, &recipe, &pts, radius, dx, dy, move |res| {
                match res {
                    Ok(Some(r)) => destructive_apply(state, item_id, r),
                    Ok(None) => {}
                    Err(()) => {}
                }
            })
        },
    );
}

/// Magic wand: flood-select pixels similar to the clicked one, sampled from
/// the color-adjusted preview (what the user sees, minus selection masking).
pub(crate) fn wand_select(state: AppState, nx: f32, ny: f32) {
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(photo) = get_photo(item.id) else { return };
    let (pix, pw, ph) = &photo.preview;
    if *pw == 0 || *ph == 0 {
        return;
    }
    let (mut p, w, h) = geometry(pix, *pw, *ph, &item.edit);
    if !item.edit.raw.is_identity() {
        rawops::apply(p.to_mut(), w, h, &item.edit.raw);
    }
    let rendered = state::render_edit(&item.edit, &item.layers);
    if item.edit.base_color_touched() {
        blur::blur_apply(p.to_mut(), w, h, item.edit.blur);
        color::apply_color(p.to_mut(), &item.edit);
    }
    if !rendered.adjust_layers.is_empty() {
        pipeline::apply_adjust_layers_u8(p.to_mut(), w, h, &rendered);
    }
    let sx = ((nx * w as f32) as usize).min(w - 1);
    let sy = ((ny * h as f32) as usize).min(h - 1);
    let tol = state.wand_tolerance.get_untracked();
    let contig = state.wand_contiguous.get_untracked();
    let (mask, count) = wand::wand_mask(&p, w, h, sx, sy, 0, tol, contig);
    state.update_current(|e| {
        e.selection = if count > 0 {
            Some(state::Selection {
                kind: state::SelectionKind::Mask { data: Rc::new(mask), width: w, height: h },
                feather: 0.0,
            })
        } else {
            None
        };
    });
}

pub(crate) fn isolate_subject(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(photo) = get_photo(item.id) else { return; };
    let (pix, pw, ph) = photo.preview.clone();
    if pw == 0 || ph == 0 {
        return;
    }
    state.busy.set(Some("Segmenting subject…".into()));
    spawn_local(async move {
        let canvas = web::create_canvas(pw as u32, ph as u32);
        web::put_pixels(&canvas, &pix, pw as u32, ph as u32);
        let Ok(blob) = web::canvas_to_blob(&canvas, "image/jpeg").await else {
            state.busy.set(None);
            return;
        };
        let Ok((img, url)) = web::load_image(&blob).await else {
            state.busy.set(None);
            return;
        };
        match web::segment_selfie(&img).await {
            Ok(mask) => {
                let _ = Url::revoke_object_url(&url);
                if mask.len() != pw * ph {
                    web::log("segmentation mask size mismatch");
                    state.busy.set(None);
                    return;
                }
                let mut bg = pix.clone();
                ops::box_blur_rgba(&mut bg, pw, ph, 8);
                ops::darken(&mut bg, 0.3);
                let inv_mask: Vec<u8> = mask.iter().map(|m| 255 - m).collect();
                let bg = ops::extract_masked(&bg, &inv_mask);
                let subject = ops::extract_masked(&pix, &mask);
                let mut next_id = 0;
                state.update_current_item(|m| {
                    let id0 = m.next_layer_id;
                    m.next_layer_id += 1;
                    let id1 = m.next_layer_id;
                    m.next_layer_id += 1;
                    state::insert_overlay(&mut m.layers, Layer::new_raster(id0, bg, pw, ph));
                    state::insert_overlay(&mut m.layers, Layer::new_raster(id1, subject, pw, ph));
                    next_id = id1;
                });
                state.selected_layer.set(Some(next_id));
                state.busy.set(None);
            }
            Err(e) => {
                let _ = Url::revoke_object_url(&url);
                web::log_err(&format!("segmentation failed: {e:?}"));
                state.busy.set(None);
            }
        }
    });
}
