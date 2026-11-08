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

/// Pivot (bounding-box centre) of a path or brush layer, in device pixels.
fn pivot_px(kind: &LayerKind, w: f64, h: f64) -> Option<(f64, f64)> {
    let bbox = match kind {
        LayerKind::Path(p) => p.points_bbox(),
        LayerKind::Brush(b) => b.points_bbox(),
        _ => None,
    }?;
    Some((((bbox.0 + bbox.2) / 2.0) as f64 * w, ((bbox.1 + bbox.3) / 2.0) as f64 * h))
}

fn apply_pivot_transform(ctx: &web_sys::CanvasRenderingContext2d, kind: &LayerKind, w: f64, h: f64) {
    let (angle, scale) = kind.pivot_transform();
    if angle == 0.0 && (scale - 1.0).abs() < 1e-4 {
        return;
    }
    let Some((cx, cy)) = pivot_px(kind, w, h) else { return };
    let _ = ctx.translate(cx, cy);
    if angle != 0.0 {
        let _ = ctx.rotate((angle as f64).to_radians());
    }
    if (scale - 1.0).abs() >= 1e-4 {
        let _ = ctx.scale(scale as f64, scale as f64);
    }
    let _ = ctx.translate(-cx, -cy);
}

pub(crate) fn draw_path_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Path(p) = &layer.kind else { return; };
    if p.points.len() < 2 {
        return;
    }
    ctx.save();
    apply_pivot_transform(ctx, &layer.kind, w, h);
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
    ctx.restore();
}

pub(crate) fn draw_brush_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Brush(b) = &layer.kind else { return; };
    ctx.save();
    apply_pivot_transform(ctx, &layer.kind, w, h);
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
        ctx.restore();
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
    ctx.restore();
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
    ctx.save();
    if r.angle != 0.0 {
        let (cx, cy) = (r.x as f64 * w, r.y as f64 * h);
        let _ = ctx.translate(cx, cy);
        let _ = ctx.rotate((r.angle as f64).to_radians());
        let _ = ctx.translate(-cx, -cy);
    }
    let _ = ctx.draw_image_with_html_canvas_element_and_dw_and_dh(&canvas, dx, dy, dw, dh);
    ctx.restore();
}

struct MaskBytes {
    key: (usize, usize, u64, usize, usize, bool),
    data: Vec<u8>,
}

struct MaskAlphaCanvas {
    w: usize,
    h: usize,
    canvas: web_sys::HtmlCanvasElement,
}

struct MaskScratch {
    w: usize,
    h: usize,
    canvas: web_sys::HtmlCanvasElement,
}

struct OverlaySegment {
    item_id: usize,
    w: usize,
    h: usize,
    fp: u64,
    canvas: web_sys::HtmlCanvasElement,
}

/// Alpha of a layer that a clipped adjustment targets. Cleaned at the layer's
/// own opacity, mask and transform, so a clipped colour edit changes exactly
/// the pixels that layer contributes.
struct ClipAlpha {
    item_id: usize,
    layer_id: usize,
    w: usize,
    h: usize,
    fp: u64,
    data: Vec<u8>,
}

/// Rasterised masks kept across frames. One entry per masked layer, so two
/// masked overlays no longer evict each other every frame; below the cap a
/// feather or brush stroke only rebuilds the mask that changed.
const MASK_BYTES_MAX: usize = 3;
/// Composited overlay runs. An adjustment above the overlays re-uses the run
/// instead of rebuilding every overlay mask on each slider tick.
const OVERLAY_SEGMENTS_MAX: usize = 2;
/// Above this frame size masks and runs are not cached: a full-res export
/// composites once, and keeping several 25 MP scratch frames alive costs more
/// than rebuilding a mask for the single pass that reads it.
const OVERLAY_CACHE_MAX_PX: usize = 4_000_000;

thread_local! {
    static MASK_BYTES: std::cell::RefCell<Vec<MaskBytes>> =
        std::cell::RefCell::new(Vec::new());
    /// One full-frame RGBA cut-out canvas, rebuilt from cached mask bytes only
    /// when a masked overlay is actually drawn.
    static MASK_ALPHA_CANVAS: std::cell::RefCell<Option<MaskAlphaCanvas>> =
        std::cell::RefCell::new(None);
    /// One reusable full-frame canvas for compositing a masked overlay before
    /// it is blitted, shared by every masked layer in a frame.
    static MASK_SCRATCH: std::cell::RefCell<Option<MaskScratch>> =
        std::cell::RefCell::new(None);
    static OVERLAY_SEGMENTS: std::cell::RefCell<Vec<OverlaySegment>> =
        std::cell::RefCell::new(Vec::new());
    static CLIP_ALPHA: std::cell::RefCell<Vec<ClipAlpha>> =
        std::cell::RefCell::new(Vec::new());
    /// Rasterising a clip target is separate from the overlay scratch: a masked
    /// target draws through the overlay scratch inside this pass.
    static CLIP_SCRATCH: std::cell::RefCell<Option<MaskScratch>> =
        std::cell::RefCell::new(None);
}

fn mask_bytes(
    item_id: usize,
    layer_id: usize,
    version: u64,
    sel: &Selection,
    invert: bool,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let key = (item_id, layer_id, version, w, h, invert);
    let cacheable = w * h <= OVERLAY_CACHE_MAX_PX;
    MASK_BYTES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(i) = cache.iter().position(|c| c.key == key) {
            let hit = cache.remove(i);
            let data = hit.data.clone();
            cache.push(hit);
            return data;
        }
        let _sp = perf::Span::new("overlay.mask.build");
        let mut mask = ops::selection_mask(sel, w, h);
        if invert {
            for v in &mut mask {
                *v = 255 - *v;
            }
        }
        if cacheable {
            cache.push(MaskBytes { key, data: mask.clone() });
            if cache.len() > MASK_BYTES_MAX {
                cache.remove(0);
            }
        }
        mask
    })
}

fn mask_alpha_canvas(
    item_id: usize,
    layer_id: usize,
    version: u64,
    sel: &Selection,
    invert: bool,
    w: usize,
    h: usize,
) -> web_sys::HtmlCanvasElement {
    let mask = mask_bytes(item_id, layer_id, version, sel, invert, w, h);
    let _sp = perf::Span::new("overlay.mask.canvas");
    let mut rgba = vec![0u8; w * h * 4];
    for (m, px) in mask.iter().zip(rgba.as_chunks_mut::<4>().0) {
        px[3] = *m;
    }
    let canvas = MASK_ALPHA_CANVAS.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(a) = c.as_ref() {
            if a.w == w && a.h == h {
                return a.canvas.clone();
            }
        }
        let canvas = web::create_canvas(w as u32, h as u32);
        *c = Some(MaskAlphaCanvas { w, h, canvas: canvas.clone() });
        canvas
    });
    web::put_pixels(&canvas, &rgba, w as u32, h as u32);
    canvas
}

fn mask_scratch(w: usize, h: usize) -> web_sys::HtmlCanvasElement {
    MASK_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if let Some(s) = scratch.as_ref() {
            if s.w == w && s.h == h {
                let ctx = web::ctx2d(&s.canvas);
                ctx.set_global_composite_operation("source-over").ok();
                ctx.set_global_alpha(1.0);
                ctx.clear_rect(0.0, 0.0, w as f64, h as f64);
                return s.canvas.clone();
            }
        }
        let canvas = web::create_canvas(w as u32, h as u32);
        *scratch = Some(MaskScratch { w, h, canvas: canvas.clone() });
        canvas
    })
}

fn draw_overlay(
    ctx: &web_sys::CanvasRenderingContext2d,
    item_id: usize,
    layer: &state::Layer,
    w: usize,
    h: usize,
) {
    if !layer.visible || layer.opacity <= 0.01 {
        return;
    }
    let Some(mask) = &layer.mask else {
        ctx.set_global_alpha(layer.opacity as f64);
        match &layer.kind {
            LayerKind::Text(_) => draw_text_layer(ctx, layer, w as f64, h as f64),
            LayerKind::Path(_) => draw_path_layer(ctx, layer, w as f64, h as f64),
            LayerKind::Brush(_) => draw_brush_layer(ctx, layer, w as f64, h as f64),
            LayerKind::Raster(_) => draw_raster_layer(ctx, layer, w as f64, h as f64),
            LayerKind::Adjust(_) => {}
        }
        ctx.set_global_alpha(1.0);
        return;
    };
    if w == 0 || h == 0 {
        return;
    }
    // A masked overlay must not erase what is below it, so destination-in
    // runs on a scratch copy and only the cut-out is blitted.
    let scratch = mask_scratch(w, h);
    let sctx = web::ctx2d(&scratch);
    match &layer.kind {
        LayerKind::Text(_) => draw_text_layer(&sctx, layer, w as f64, h as f64),
        LayerKind::Path(_) => draw_path_layer(&sctx, layer, w as f64, h as f64),
        LayerKind::Brush(_) => draw_brush_layer(&sctx, layer, w as f64, h as f64),
        LayerKind::Raster(_) => draw_raster_layer(&sctx, layer, w as f64, h as f64),
        LayerKind::Adjust(_) => return,
    }
    let alpha = mask_alpha_canvas(item_id, layer.id, layer.mask_version, mask, layer.invert, w, h);
    sctx.set_global_composite_operation("destination-in").ok();
    sctx.draw_image_with_html_canvas_element(&alpha, 0.0, 0.0).ok();
    sctx.set_global_composite_operation("source-over").ok();
    ctx.set_global_alpha(layer.opacity as f64);
    ctx.draw_image_with_html_canvas_element(&scratch, 0.0, 0.0).ok();
    ctx.set_global_alpha(1.0);
}

fn segment_fp(item_id: usize, layers: &[state::Layer], w: usize, h: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hsh = std::collections::hash_map::DefaultHasher::new();
    item_id.hash(&mut hsh);
    w.hash(&mut hsh);
    h.hash(&mut hsh);
    for l in layers {
        hash_layer(&mut hsh, l);
    }
    hsh.finish()
}

/// A run of overlay layers composited onto a transparent full-frame canvas,
/// cached by its content fingerprint. Slider edits to an adjustment above the
/// run (or to the base photo) reuse it without touching a single mask.
fn segment_canvas(
    item_id: usize,
    layers: &[state::Layer],
    w: usize,
    h: usize,
) -> web_sys::HtmlCanvasElement {
    if w * h > OVERLAY_CACHE_MAX_PX {
        let _sp = perf::Span::new("overlay.segment.build");
        let canvas = web::create_canvas(w as u32, h as u32);
        let ctx = web::ctx2d(&canvas);
        for l in layers {
            draw_overlay(&ctx, item_id, l, w, h);
        }
        return canvas;
    }
    let fp = segment_fp(item_id, layers, w, h);
    OVERLAY_SEGMENTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(i) = cache
            .iter()
            .position(|s| s.item_id == item_id && s.w == w && s.h == h && s.fp == fp)
        {
            let hit = cache.remove(i);
            let canvas = hit.canvas.clone();
            cache.push(hit);
            return canvas;
        }
        let _sp = perf::Span::new("overlay.segment.build");
        let canvas = web::create_canvas(w as u32, h as u32);
        let ctx = web::ctx2d(&canvas);
        for l in layers {
            draw_overlay(&ctx, item_id, l, w, h);
        }
        cache.push(OverlaySegment { item_id, w, h, fp, canvas: canvas.clone() });
        if cache.len() > OVERLAY_SEGMENTS_MAX {
            cache.remove(0);
        }
        canvas
    })
}

fn clip_alpha(item_id: usize, target: &state::Layer, w: usize, h: usize) -> Vec<u8> {
    let fp = segment_fp(item_id, std::slice::from_ref(target), w, h);
    let cacheable = w * h <= OVERLAY_CACHE_MAX_PX;
    if cacheable {
        let hit = CLIP_ALPHA.with(|c| {
            let mut c = c.borrow_mut();
            let i = c.iter().position(|a| {
                a.item_id == item_id && a.layer_id == target.id && a.w == w && a.h == h && a.fp == fp
            })?;
            let hit = c.remove(i);
            let data = hit.data.clone();
            c.push(hit);
            Some(data)
        });
        if let Some(data) = hit {
            return data;
        }
    }
    let _sp = perf::Span::new("stack.clip.build");
    let canvas = CLIP_SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(c) = s.as_ref() {
            if c.w == w && c.h == h {
                let ctx = web::ctx2d(&c.canvas);
                ctx.set_global_composite_operation("source-over").ok();
                ctx.set_global_alpha(1.0);
                ctx.clear_rect(0.0, 0.0, w as f64, h as f64);
                return c.canvas.clone();
            }
        }
        let canvas = web::create_canvas(w as u32, h as u32);
        *s = Some(MaskScratch { w, h, canvas: canvas.clone() });
        canvas
    });
    let ctx = web::ctx2d(&canvas);
    draw_overlay(&ctx, item_id, target, w, h);
    let (data, _, _) = web::image_data_from_ctx(&ctx, w as u32, h as u32);
    let alpha: Vec<u8> = data.chunks(4).map(|px| px[3]).collect();
    if cacheable {
        CLIP_ALPHA.with(|c| {
            let mut c = c.borrow_mut();
            c.push(ClipAlpha { item_id, layer_id: target.id, w, h, fp, data: alpha.clone() });
            if c.len() > 2 {
                c.remove(0);
            }
        });
    }
    alpha
}

fn apply_adjust_on_canvas(
    canvas: &web_sys::HtmlCanvasElement,
    ctx: &web_sys::CanvasRenderingContext2d,
    item_id: usize,
    layers: &[state::Layer],
    li: usize,
    w: usize,
    h: usize,
) {
    let layer = &layers[li];
    if !layer.visible || layer.opacity <= 0.01 || w == 0 || h == 0 {
        return;
    }
    let LayerKind::Adjust(a) = &layer.kind else { return };
    let _sp = perf::Span::new("stack.adjust.apply");
    let own_mask = layer.mask.as_ref().map(|sel| {
        mask_bytes(item_id, layer.id, layer.mask_version, sel, layer.invert, w, h)
    });
    let clip = match a.clip_to {
        Some(tid) => {
            // A clipped adjustment only affects its target. If the target is
            // gone or no longer below it, it affects nothing — never the
            // whole picture.
            let Some(ti) = layers[..li].iter().position(|l| l.id == tid) else { return };
            let target = &layers[ti];
            if !target.visible || target.opacity <= 0.01 {
                return;
            }
            Some(clip_alpha(item_id, target, w, h))
        }
        None => None,
    };
    let mask = match (own_mask, clip) {
        (None, None) => None,
        (Some(m), None) => Some(m),
        (None, Some(c)) => Some(c),
        (Some(m), Some(c)) => Some(
            m.iter()
                .zip(c.iter())
                .map(|(a, b)| ((*a as u16 * *b as u16) / 255) as u8)
                .collect(),
        ),
    };
    let (mut data, _, _) = web::image_data_from_ctx(ctx, w as u32, h as u32);
    pes_pixel::color::apply_point_adjust_u8(
        &mut data,
        &a.adjust,
        mask.as_deref(),
        layer.opacity,
    );
    // put_pixels resets the canvas backing, so the caller re-uses `ctx` only
    // for further whole-frame draws (alpha/transform state is set explicitly
    // before each overlay).
    web::put_pixels(canvas, &data, w as u32, h as u32);
}

/// Composite the layer stack above the photo: overlays and the adjustments
/// that sit above them, bottom-up. Adjustments below the first drawn overlay
/// are already in the worker/CPU pixels and are skipped here.
pub(crate) fn composite_stack(
    canvas: &web_sys::HtmlCanvasElement,
    item_id: usize,
    layers: &[state::Layer],
    w: usize,
    h: usize,
) {
    let split = state::stack_split(layers);
    let ctx = web::ctx2d(canvas);
    let mut i = split.min(layers.len());
    while i < layers.len() {
        let start = i;
        while i < layers.len() && !matches!(layers[i].kind, LayerKind::Adjust(_)) {
            i += 1;
        }
        let run = &layers[start..i];
        if run.iter().any(|l| l.visible && l.opacity > 0.01) && w > 0 && h > 0 {
            let seg = segment_canvas(item_id, run, w, h);
            ctx.draw_image_with_html_canvas_element(&seg, 0.0, 0.0).ok();
        }
        if i < layers.len() {
            apply_adjust_on_canvas(canvas, &ctx, item_id, layers, i, w, h);
            i += 1;
        }
    }
}

pub(crate) fn clear_overlay_segments() {
    OVERLAY_SEGMENTS.with(|c| c.borrow_mut().clear());
}

/// Drop every full-frame overlay scratch. Used after an export, where masks
/// and segments would otherwise linger at native resolution.
pub(crate) fn clear_overlay_caches() {
    clear_overlay_segments();
    MASK_BYTES.with(|c| c.borrow_mut().clear());
    MASK_ALPHA_CANVAS.with(|c| *c.borrow_mut() = None);
    MASK_SCRATCH.with(|c| *c.borrow_mut() = None);
    CLIP_ALPHA.with(|c| c.borrow_mut().clear());
    CLIP_SCRATCH.with(|c| *c.borrow_mut() = None);
}

pub(crate) async fn render_text_overlay(t: &state::TextLayer, w: usize, h: usize) -> Result<web_sys::Blob, JsValue> {
    let canvas = web::create_canvas(w as u32, h as u32);
    let mut layer = state::Layer::new_text(0);
    layer.kind = state::LayerKind::Text(t.clone());
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
    // The render only consumes the global selection as an effect mask when a
    // colour stage is active; painting one on an otherwise untouched photo
    // must not invalidate the hi-res render (a full 3200 px re-render and blit
    // per brush stroke).
    if e.base_color_touched() {
        hash_selection(&mut h, e.selection.as_ref());
        h.write_u64(e.selection_rev);
    }
    item.layers.len().hash(&mut h);
    for l in &item.layers {
        hash_layer(&mut h, l);
    }
    h.finish()
}

fn hash_layer(h: &mut std::collections::hash_map::DefaultHasher, l: &state::Layer) {
    use std::hash::{Hash, Hasher};
    l.id.hash(h);
    l.visible.hash(h);
    h.write_u32(l.opacity.to_bits());
    l.invert.hash(h);
    // Every mask change bumps the version; the selection hash only samples
    // 8 bytes of a raster mask, so two wand masks of the same size can
    // collide without it.
    l.mask_version.hash(h);
    hash_selection(h, l.mask.as_ref());
    match &l.kind {
        LayerKind::Text(t) => {
            h.write_u8(1);
            t.text.hash(h);
            t.font_family.hash(h);
            t.color.hash(h);
            t.stroke_color.hash(h);
            t.shadow_color.hash(h);
            t.font_weight.hash(h);
            std::mem::discriminant(&t.alignment).hash(h);
            for v in [t.x, t.y, t.font_size, t.angle, t.stroke_width, t.shadow_blur, t.shadow_offset_x, t.shadow_offset_y] {
                h.write_u32(v.to_bits());
            }
        }
        LayerKind::Path(p) => {
            h.write_u8(2);
            p.points.len().hash(h);
            p.closed.hash(h);
            p.fill_color.hash(h);
            p.stroke_color.hash(h);
            h.write_u32(p.stroke_width.to_bits());
            h.write_u32(p.angle.to_bits());
            h.write_u32(p.scale.to_bits());
            for pt in p.points.first().into_iter().chain(p.points.last()) {
                h.write_u32(pt.x.to_bits());
                h.write_u32(pt.y.to_bits());
            }
        }
        LayerKind::Brush(b) => {
            h.write_u8(3);
            b.strokes.len().hash(h);
            b.color.hash(h);
            b.custom_path.hash(h);
            std::mem::discriminant(&b.shape).hash(h);
            h.write_u32(b.width.to_bits());
            h.write_u32(b.spacing.to_bits());
            h.write_u32(b.angle.to_bits());
            h.write_u32(b.scale.to_bits());
            if let Some(s) = b.strokes.last() {
                s.points.len().hash(h);
                if let Some(p) = s.points.last() {
                    h.write_u32(p.0.to_bits());
                    h.write_u32(p.1.to_bits());
                }
            }
        }
        LayerKind::Raster(r) => {
            h.write_u8(4);
            r.width.hash(h);
            r.height.hash(h);
            h.write_u32(r.x.to_bits());
            h.write_u32(r.y.to_bits());
            h.write_u32(r.scale.to_bits());
            h.write_u32(r.angle.to_bits());
            r.rev.hash(h);
            if !r.pixels.is_empty() {
                r.pixels.len().hash(h);
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
            format!("{:?}", a.adjust.levels).hash(h);
            format!("{:?}", a.adjust.curves).hash(h);
            a.clip_to.hash(h);
        }
    }
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
                                m.layers.push(Layer::new_raster(nid, r.layer, r.lw, r.lh));
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
            m.edit.selection_rev += 1;
        }
    });
    state.subject_photo.set(None);
    clear_subject_brush_ops_for(item_id);
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

/// Map a full-image normalized point into an image layer's own normalized
/// pixel space, undoing its placement and rotation.
pub(crate) fn raster_layer_point(
    r: &state::RasterLayer,
    nx: f32,
    ny: f32,
    w: usize,
    h: usize,
) -> Option<(f32, f32)> {
    let dw = r.scale * w as f32;
    if dw <= 0.0 || r.width == 0 || r.height == 0 {
        return None;
    }
    let dh = dw * r.height as f32 / r.width as f32;
    let dx = (nx - r.x) * w as f32;
    let dy = (ny - r.y) * h as f32;
    let (s, c) = r.angle.to_radians().sin_cos();
    let lx = (dx * c + dy * s) / dw + 0.5;
    let ly = (-dx * s + dy * c) / dh + 0.5;
    Some((lx, ly))
}

/// Stroke coverage in an image layer's pixel space, limited to opaque layer
/// pixels so healing and cloning never write into the transparent margin.
fn layer_stroke_coverage(
    r: &state::RasterLayer,
    pts: &[(f32, f32)],
    w: usize,
    h: usize,
    radius_frac: f32,
) -> Option<Vec<u8>> {
    let dw = r.scale * w as f32;
    if dw <= 0.0 {
        return None;
    }
    let mapped: Vec<(f32, f32)> = pts
        .iter()
        .filter_map(|&(nx, ny)| raster_layer_point(r, nx, ny, w, h))
        .collect();
    if mapped.is_empty() {
        return None;
    }
    let diag = ((w * w + h * h) as f32).sqrt();
    let r_px = (radius_frac * diag * (r.width as f32 / dw)).max(1.0);
    let mut cov = vec![0u8; r.width * r.height];
    pes_pixel::geo::stamp_stroke(&mut cov, r.width, r.height, &mapped, r_px);
    for (i, px) in r.pixels.chunks(4).enumerate() {
        if px[3] == 0 {
            cov[i] = 0;
        }
    }
    Some(cov)
}

fn swap_layer_pixels(state: AppState, layer_id: usize, px: Vec<u8>) {
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == layer_id) {
            if let LayerKind::Raster(r) = &mut l.kind {
                r.pixels = Rc::new(px);
                r.rev += 1;
            }
        }
    });
}

/// Spot-heal an image layer's own pixels. The stroke arrives in full-image
/// normalized coords and runs at the layer's native resolution — the photo
/// underneath is untouched.
pub(crate) fn apply_layer_heal(state: AppState, layer_id: usize, pts: &[(f32, f32)]) {
    if pts.is_empty() {
        return;
    }
    let Some(item) = state.current() else { return };
    let Some(layer) = item.layers.iter().find(|l| l.id == layer_id) else { return };
    let LayerKind::Raster(r) = &layer.kind else { return };
    let (w, h) = working_dims(item.width, item.height, &item.edit);
    let Some(cov) = layer_stroke_coverage(r, pts, w, h, state.heal_radius.get_untracked()) else {
        return;
    };
    let mut px = (*r.pixels).clone();
    let seed = (layer_id as u32).wrapping_mul(2654435761) ^ r.rev as u32;
    if !heal::spot_heal(&mut px, &cov, r.width, r.height, 1.0, state.heal_mode.get_untracked(), seed) {
        return;
    }
    swap_layer_pixels(state, layer_id, px);
}

/// Clone-stamp an image layer's own pixels from a source point on the canvas.
pub(crate) fn apply_layer_clone(state: AppState, layer_id: usize, pts: &[(f32, f32)]) {
    if pts.is_empty() {
        return;
    }
    let Some(item) = state.current() else { return };
    let Some(src) = state.clone_source.get_untracked() else { return };
    let Some(layer) = item.layers.iter().find(|l| l.id == layer_id) else { return };
    let LayerKind::Raster(r) = &layer.kind else { return };
    let (w, h) = working_dims(item.width, item.height, &item.edit);
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
    let Some(cov) = layer_stroke_coverage(r, pts, w, h, state.clone_radius.get_untracked()) else {
        return;
    };
    let Some((lx, ly)) = raster_layer_point(r, pts[0].0, pts[0].1, w, h) else { return };
    let Some((sx, sy)) = raster_layer_point(r, pts[0].0 + offset.0, pts[0].1 + offset.1, w, h)
    else {
        return;
    };
    let dx = ((sx - lx) * r.width as f32).round() as i32;
    let dy = ((sy - ly) * r.height as f32).round() as i32;
    let mut px = (*r.pixels).clone();
    pes_pixel::clone::clone_stamp(&mut px, r.width, r.height, &cov, dx, dy);
    swap_layer_pixels(state, layer_id, px);
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
    state.subject_photo.set(None);
    clear_subject_brush_ops(state);
    state.update_current(|e| {
        e.selection = if count > 0 {
            Some(state::Selection {
                kind: state::SelectionKind::Mask { data: Rc::new(mask), width: w, height: h },
                feather: 0.0,
            })
        } else {
            None
        };
        e.selection_rev += 1;
    });
}

/// Select › Inverse: what was outside the selection becomes selected. A raster
/// selection (wand, subject, brushed) inverts at its own size, so inverting
/// twice gives back the same mask; a rect or lasso is rasterised hard-edged at
/// the brush's mask size first. Feather carries over, so a soft edge stays
/// soft. A tapped subject's refine state belongs to the old selection, so it
/// is dropped, as Clear does.
pub(crate) fn invert_selection(state: AppState) {
    let Some(item) = state.current() else { return };
    let Some(sel) = item.edit.selection.clone() else { return };
    let (w, h) = match &sel.kind {
        SelectionKind::Mask { width, height, .. } if *width > 0 && *height > 0 => (*width, *height),
        _ => match brush_dims(&item) {
            Some(d) => d,
            None => return,
        },
    };
    let hard = Selection { kind: sel.kind.clone(), feather: 0.0 };
    let mut data = ops::selection_mask(&hard, w, h);
    for v in &mut data {
        *v = 255 - *v;
    }
    state.subject_photo.set(None);
    clear_subject_brush_ops(state);
    state.update_current(|e| {
        e.selection = Some(Selection {
            kind: SelectionKind::Mask { data: Rc::new(data), width: w, height: h },
            feather: sel.feather,
        });
        e.selection_rev += 1;
    });
}

// --- selection brush ---------------------------------------------------------

/// One stroke's undo budget: dirty-tile patches, never full-mask snapshots.
const BRUSH_HISTORY_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Where the brush writes: the global selection, or an adjustment layer's mask
/// when that layer is the active one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BrushTarget {
    Global,
    Layer(usize),
}

#[derive(Clone, Debug)]
pub(crate) struct MaskPatch {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

impl MaskPatch {
    fn bytes(&self) -> usize {
        self.before.len() + self.after.len()
    }
}

/// A global brush stroke made on a subject-derived selection, in normalized
/// image coords. `subject_run` replays these onto every fresh decode so a
/// refinement tap refines the subject instead of discarding the strokes.
#[derive(Clone, Debug)]
pub(crate) struct GlobalBrushOp {
    pub points: Vec<(f32, f32)>,
    /// Brush size as a fraction of the image diagonal, same as the UI slider.
    pub size: f32,
    pub softness: f32,
    pub add: bool,
}

thread_local! {
    static SUBJECT_BRUSH_OPS: RefCell<HashMap<usize, Vec<GlobalBrushOp>>> =
        RefCell::new(HashMap::new());
    /// The latest decoded subject mask per photo, before any strokes. Undo and
    /// redo rebuild from it once a refinement tap has replaced the mask their
    /// patches were taken from.
    static SUBJECT_DECODE: RefCell<HashMap<usize, (Vec<u8>, usize, usize)>> =
        RefCell::new(HashMap::new());
}

fn subject_brush_ops(item_id: usize) -> Vec<GlobalBrushOp> {
    SUBJECT_BRUSH_OPS.with(|m| m.borrow().get(&item_id).cloned().unwrap_or_default())
}

fn push_subject_brush_op(item_id: usize, op: GlobalBrushOp) {
    SUBJECT_BRUSH_OPS.with(|m| m.borrow_mut().entry(item_id).or_default().push(op));
}

fn pop_subject_brush_op(item_id: usize) {
    SUBJECT_BRUSH_OPS.with(|m| {
        if let Some(v) = m.borrow_mut().get_mut(&item_id) {
            v.pop();
        }
    });
}

/// The selection those strokes refined is gone (wand, Clear, vector select,
/// handed to a layer, destructive edit): drop the replay list with it.
pub(crate) fn clear_subject_brush_ops(state: AppState) {
    if let Some(item) = state.current() {
        clear_subject_brush_ops_for(item.id);
    }
}

pub(crate) fn clear_subject_brush_ops_for(item_id: usize) {
    SUBJECT_BRUSH_OPS.with(|m| {
        m.borrow_mut().remove(&item_id);
    });
    SUBJECT_DECODE.with(|m| {
        m.borrow_mut().remove(&item_id);
    });
}

/// A tap that starts a new subject selection replaces the old one, so the
/// strokes made on the old one go with it: no replay and no undo steps.
fn start_new_subject_selection(state: AppState, item_id: usize) {
    clear_subject_brush_ops_for(item_id);
    state.brush_history.update(|h| {
        let stale = |s: &BrushStep| s.item_id == item_id && s.target == BrushTarget::Global;
        h.undo.retain(|s| !stale(s));
        h.redo.retain(|s| !stale(s));
        h.recount();
    });
}

/// Undo or redo a subject stroke whose patches no longer match the mask because
/// a refinement tap rebuilt it: change the replay list and rebuild from the
/// latest decode. False when the step is not a stroke on the live subject.
fn rederive_subject_step(state: AppState, step: &BrushStep, undo: bool) -> bool {
    let Some(op) = &step.op else { return false };
    if step.target != BrushTarget::Global
        || state.subject_photo.get_untracked() != Some(step.item_id)
    {
        return false;
    }
    let Some((mut mask, w, h)) =
        SUBJECT_DECODE.with(|m| m.borrow().get(&step.item_id).cloned())
    else {
        return false;
    };
    if undo {
        pop_subject_brush_op(step.item_id);
    } else {
        push_subject_brush_op(step.item_id, op.clone());
    }
    replay_subject_brush_ops(step.item_id, &mut mask, w, h);
    brush_store(state, step.item_id, BrushTarget::Global, mask, w, h)
}

fn advance_brush_history(state: AppState, undo: bool) {
    state.brush_history.update(|h| {
        if undo {
            if let Some(s) = h.undo.pop() {
                h.redo.push(s);
            }
        } else if let Some(s) = h.redo.pop() {
            h.undo.push(s);
        }
    });
}

fn replay_subject_brush_ops(item_id: usize, mask: &mut [u8], w: usize, h: usize) {
    let ops = subject_brush_ops(item_id);
    if ops.is_empty() || w == 0 || h == 0 || mask.len() != w * h {
        return;
    }
    let diag = ((w * w + h * h) as f32).sqrt();
    for op in ops {
        let pts: Vec<(f32, f32)> = op
            .points
            .iter()
            .map(|&(x, y)| (x * w as f32, y * h as f32))
            .collect();
        let radius = (op.size * diag * 0.5).max(0.5);
        if let Some(s) = brush::stroke_mask(w, h, &pts, radius, op.softness) {
            brush::apply_stroke(mask, w, h, &s, op.add);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BrushStep {
    pub item_id: usize,
    pub target: BrushTarget,
    pub w: usize,
    pub h: usize,
    pub patches: Vec<MaskPatch>,
    pub op: Option<GlobalBrushOp>,
}

impl BrushStep {
    fn bytes(&self) -> usize {
        self.patches.iter().map(MaskPatch::bytes).sum()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BrushHistory {
    pub undo: Vec<BrushStep>,
    pub redo: Vec<BrushStep>,
    pub bytes: usize,
}

impl BrushHistory {
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn recount(&mut self) {
        self.bytes = self.undo.iter().chain(self.redo.iter()).map(BrushStep::bytes).sum();
    }
}

fn push_brush_step(state: AppState, step: BrushStep) {
    if let Some(op) = &step.op {
        push_subject_brush_op(step.item_id, op.clone());
    }
    state.brush_history.update(|h| {
        h.redo.clear();
        h.undo.push(step);
        h.recount();
        while h.bytes > BRUSH_HISTORY_MAX_BYTES && h.undo.len() > 1 {
            h.undo.remove(0);
            h.recount();
        }
    });
}

fn invalidate_brush_history(state: AppState) {
    state.brush_history.update(|h| {
        h.undo.clear();
        h.redo.clear();
        h.bytes = 0;
    });
}

/// The layer whose mask the Select tab's Brush and mask panel edit: the mask
/// armed by its row thumbnail, else the selected adjustment layer. `None`
/// means they edit the photo's own selection.
pub(crate) fn mask_layer_for(
    mask_target: Option<(usize, usize)>,
    selected: Option<usize>,
    item: &MediaItem,
) -> Option<usize> {
    if let Some((item_id, layer_id)) = mask_target {
        if item_id == item.id && item.layers.iter().any(|l| l.id == layer_id) {
            return Some(layer_id);
        }
    }
    let id = selected?;
    item.layers
        .iter()
        .any(|l| l.id == id && matches!(l.kind, LayerKind::Adjust(_)))
        .then_some(id)
}

/// What the brush paints: the Select tab's mask layer, else the global selection.
pub(crate) fn brush_target(state: AppState, item: &MediaItem) -> BrushTarget {
    match mask_layer_for(state.mask_target.get_untracked(), state.selected_layer.get_untracked(), item) {
        Some(id) => BrushTarget::Layer(id),
        None => BrushTarget::Global,
    }
}

/// Add a selection (rect, lasso, wand or subject) to a layer's mask, or cut
/// it out. A layer without a mask shows its effect everywhere, so Add
/// starts from an empty mask (only the shape) and Remove from a full one.
/// Stored like a brush stroke, so Undo/Redo in the Select tab cover it.
pub(crate) fn combine_into_layer_mask(state: AppState, layer_id: usize, shape: &Selection, remove: bool) -> bool {
    let Some(item) = state.current() else { return false };
    let Some(layer) = item.layers.iter().find(|l| l.id == layer_id) else { return false };
    let Some((w, h)) = brush_dims(&item) else { return false };
    let base = match &layer.mask {
        Some(sel) => ops::selection_mask(sel, w, h),
        None => vec![if remove { 255u8 } else { 0u8 }; w * h],
    };
    let shape_px = ops::selection_mask(shape, w, h);
    let mut next = base.clone();
    for (n, &s) in next.iter_mut().zip(shape_px.iter()) {
        *n = if remove { (*n).min(255 - s) } else { (*n).max(s) };
    }
    // One patch over the changed rows and columns, for undo.
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            if base[row + x] != next[row + x] {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    // A layer without a mask still gets one, even if the shape changed nothing.
    if x1 < x0 || y1 < y0 {
        if layer.mask.is_none() {
            return brush_store(state, item.id, BrushTarget::Layer(layer_id), next, w, h);
        }
        return false;
    }
    let (pw, ph) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut before = Vec::with_capacity(pw * ph);
    let mut after = Vec::with_capacity(pw * ph);
    for y in y0..=y1 {
        before.extend_from_slice(&base[y * w + x0..y * w + x1 + 1]);
        after.extend_from_slice(&next[y * w + x0..y * w + x1 + 1]);
    }
    if !brush_store(state, item.id, BrushTarget::Layer(layer_id), next, w, h) {
        return false;
    }
    push_brush_step(
        state,
        BrushStep {
            item_id: item.id,
            target: BrushTarget::Layer(layer_id),
            w,
            h,
            patches: vec![MaskPatch { x: x0, y: y0, w: pw, h: ph, before, after }],
            op: None,
        },
    );
    true
}

/// "Add selection to mask" / "Remove selection from mask": fold the photo's
/// selection (a subject tap and its refinements, or one made earlier) into
/// the mask being edited, then clear it.
pub(crate) fn commit_selection_to_mask(state: AppState, remove: bool) {
    let Some(item) = state.current() else { return };
    let Some(id) = mask_layer_for(state.mask_target.get_untracked(), state.selected_layer.get_untracked(), &item) else {
        return;
    };
    let Some(sel) = item.edit.selection.clone() else { return };
    combine_into_layer_mask(state, id, &sel, remove);
    state.subject_photo.set(None);
    clear_subject_brush_ops(state);
    state.update_current(|e| {
        e.selection = None;
        e.selection_rev += 1;
    });
}

/// "Invert mask": flip the edited mask's Invert setting, the same one as in
/// the Layers tab's Mask section.
pub(crate) fn invert_layer_mask(state: AppState) {
    let Some(item) = state.current() else { return };
    let Some(id) = mask_layer_for(state.mask_target.get_untracked(), state.selected_layer.get_untracked(), &item) else {
        return;
    };
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
            l.invert = !l.invert;
            l.mask_version += 1;
        }
    });
}

/// "Clear mask": drop the edited mask, so the layer applies everywhere again.
pub(crate) fn clear_layer_mask(state: AppState) {
    let Some(item) = state.current() else { return };
    let Some(id) = mask_layer_for(state.mask_target.get_untracked(), state.selected_layer.get_untracked(), &item) else {
        return;
    };
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
            l.set_mask(None);
        }
    });
}

/// Overlay cache key of a brush target's committed mask.
pub(crate) fn brush_overlay_key(item: &MediaItem, target: BrushTarget) -> u64 {
    match target {
        BrushTarget::Global => overlay_key(item.id as u64, 0, item.edit.selection_rev),
        BrushTarget::Layer(id) => item
            .layers
            .iter()
            .find(|l| l.id == id)
            .map(|l| overlay_key(item.id as u64, id as u64, l.mask_version))
            .unwrap_or(0),
    }
}

/// Preview-resolution mask space: the geometry-corrected preview, the frame
/// the live tint paints in.
fn brush_view_dims(item: &MediaItem) -> Option<(usize, usize)> {
    let photo = get_photo(item.id)?;
    if photo.preview.1 == 0 || photo.preview.2 == 0 {
        return None;
    }
    Some(match preview_geometry_cached(item.id, photo.gen, &photo.preview, &item.edit) {
        Some((_, w, h)) => (w, h),
        None => (photo.preview.1, photo.preview.2),
    })
}

/// Committed mask space for brush edits: the geometry-corrected working frame
/// capped at the 3200 px raster cap. A 1024 px raster softens a lasso edge to
/// ~5 px when upsampled to a 6192 px export; 3200 keeps it near the source
/// resolution. The live tint stays at `brush_view_dims` so the drag cost does
/// not grow with this.
fn brush_dims(item: &MediaItem) -> Option<(usize, usize)> {
    let photo = get_photo(item.id)?;
    let view = brush_view_dims(item)?;
    let (fw, fh) = photo.full_dims;
    if fw == 0 || fh == 0 {
        return Some(view);
    }
    let (fw, fh) = working_dims(fw, fh, &item.edit);
    let scale = (HI_MAX_EDGE as f32 / fw.max(fh) as f32).min(1.0);
    let w = (fw as f32 * scale).round().max(1.0) as usize;
    let h = (fh as f32 * scale).round().max(1.0) as usize;
    Some((w, h))
}

fn brush_selection(item: &MediaItem, target: BrushTarget) -> Option<&Selection> {
    match target {
        BrushTarget::Global => item.edit.selection.as_ref(),
        BrushTarget::Layer(id) => item.layers.iter().find(|l| l.id == id)?.mask.as_ref(),
    }
}

/// Rasterize the target into mask bytes; a vector selection is baked (feather
/// included) on the first stroke.
fn brush_base_mask(item: &MediaItem, target: BrushTarget, w: usize, h: usize) -> Vec<u8> {
    match brush_selection(item, target) {
        Some(sel) => ops::selection_mask(sel, w, h),
        None => vec![0u8; w * h],
    }
}

/// Tight bounds of nonzero mask coverage, as (x, y, w, h).
fn mask_bounds(mask: &[u8], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0;
    let mut max_y = 0;
    for y in 0..h {
        let row = &mask[y * w..(y + 1) * w];
        let Some(first) = row.iter().position(|&v| v != 0) else { continue };
        let last = row.iter().rposition(|&v| v != 0).unwrap_or(first);
        min_x = min_x.min(first);
        max_x = max_x.max(last);
        min_y = min_y.min(y);
        max_y = y;
    }
    if max_x < min_x || max_y < min_y {
        return None;
    }
    Some((min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
}

pub(crate) fn brush_begin(state: AppState, nx: f32, ny: f32) -> Option<BrushDrag> {
    let mut begin = perf::Span::new("brush.begin");
    let item = state.current()?;
    if item.kind != MediaKind::Photo {
        return None;
    }
    let (w, h) = brush_dims(&item)?;
    let (vw, vh) = brush_view_dims(&item)?;
    let target = brush_target(state, &item);
    // No committed selection: keep the base masks virtual. An empty target is
    // the common case (first stroke) and full-frame zero buffers here are tens
    // of megabytes of pointerdown allocation.
    let base_sel = brush_selection(&item, target);
    // An unmasked layer shows its effect everywhere, so a Remove stroke on it
    // starts from a fully revealed mask; an Add stroke starts empty.
    let reveal_all = matches!(target, BrushTarget::Layer(_))
        && base_sel.is_none()
        && state.brush_remove.get_untracked();
    let view_empty = base_sel.is_none() && !reveal_all;
    let base_at = |bw: usize, bh: usize| {
        if reveal_all { vec![255u8; bw * bh] } else { brush_base_mask(&item, target, bw, bh) }
    };
    let base = if view_empty { None } else { Some(Rc::new(base_at(w, h))) };
    let view_base = if view_empty { None } else { Some(Rc::new(base_at(vw, vh))) };
    // A global vector selection is shown as an outline until the first stroke
    // bakes it; only then does the canvas need the mask tint painted in.
    let base_blit = matches!(
        (target, base_sel),
        (BrushTarget::Global, Some(sel)) if !matches!(&sel.kind, SelectionKind::Mask { .. })
    );
    let base_bbox = if base_blit {
        view_base.as_ref().and_then(|b| mask_bounds(b, vw, vh))
    } else {
        None
    };
    let size = state.brush_size.get_untracked();
    let diag = ((w * w + h * h) as f32).sqrt();
    let radius = (size * diag * 0.5).max(0.5);
    let view_diag = ((vw * vw + vh * vh) as f32).sqrt();
    let view_radius = (size * view_diag * 0.5).max(0.5);
    let softness = state.brush_softness.get_untracked().clamp(0.0, 1.0);
    let add = !state.brush_remove.get_untracked();
    let tint_key = brush_overlay_key(&item, target);
    let live = match &base {
        Some(b) => brush::LiveStroke::new(w, h, b.clone(), radius, softness, add, (nx * w as f32, ny * h as f32))
            .track_committed(),
        // Empty base: coverage is the final mask, so there is nothing to merge.
        None => brush::LiveStroke::new_empty(w, h, radius, softness, add, (nx * w as f32, ny * h as f32)),
    };
    let view_live = match &view_base {
        Some(b) => brush::LiveStroke::new(vw, vh, b.clone(), view_radius, softness, add, (nx * vw as f32, ny * vh as f32)),
        None => brush::LiveStroke::new_empty(vw, vh, view_radius, softness, add, (nx * vw as f32, ny * vh as f32)),
    };
    let frame = last_full_frame(
        item.id,
        tint_key,
        state.hi_built.get_untracked(),
        state.viewport_gen.get_untracked(),
    );
    begin.set_detail(if frame.is_some() { "frame" } else { "noframe" });
    Some(BrushDrag {
        item_id: item.id,
        target,
        w,
        h,
        base,
        live: Rc::new(RefCell::new(live)),
        view_w: vw,
        view_h: vh,
        view_base,
        view_live: Rc::new(RefCell::new(view_live)),
        tint: Rc::new(RefCell::new(None)),
        tint_key,
        frame: Rc::new(RefCell::new(frame)),
        view_empty,
        base_blit,
        base_bbox,
    })
}

fn copy_rect(src: &[u8], sw: usize, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h);
    for row in y..y + h {
        out.extend_from_slice(&src[row * sw + x..row * sw + x + w]);
    }
    out
}

pub(crate) fn brush_commit(state: AppState, drag: BrushDrag) {
    let Some(item) = state.current() else { return };
    if item.id != drag.item_id || item.kind != MediaKind::Photo {
        return;
    }
    let (after, changed) = {
        let _sp = perf::Span::new("brush.commit.finish");
        drag.live.borrow_mut().finish()
    };
    let Some(b) = changed else { return };
    let patch = {
        let _sp = perf::Span::new("brush.commit.snapshot");
        MaskPatch {
            x: b.x,
            y: b.y,
            w: b.w,
            h: b.h,
            before: match drag.base.as_deref() {
                Some(base) => copy_rect(base, drag.w, b.x, b.y, b.w, b.h),
                None => vec![0u8; b.w * b.h],
            },
            after: copy_rect(&after, drag.w, b.x, b.y, b.w, b.h),
        }
    };
    // The live tint already shows this stroke. For a global selection with no
    // colour stage reading it, the selection is pure overlay: adopt that tint
    // for the new mask and skip the full-frame repaint the selection bump
    // would trigger (the next pointer move paints as usual).
    let overlay_only = drag.target == BrushTarget::Global
        && state.current().is_some_and(|m| !m.edit.base_color_touched())
        && drag.tint.borrow().is_some();
    if overlay_only {
        skip_next_preview_paint();
    }
    let _sp = perf::Span::new("brush.commit.store");
    if !brush_store(state, drag.item_id, drag.target, after, drag.w, drag.h) {
        cancel_skipped_preview_paint();
        return;
    }
    if overlay_only {
        flush_brush_tint(&drag);
        let rgb = [10, 132, 255];
        if let (Some(m), Some(tint)) = (state.current(), drag.tint.borrow().as_ref()) {
            adopt_mask_tint(
                overlay_key(m.id as u64, 0, m.edit.selection_rev),
                drag.view_w,
                drag.view_h,
                rgb,
                tint.canvas(),
            );
            retag_full_frame(overlay_key(m.id as u64, 0, m.edit.selection_rev));
        }
    }
    let op = if drag.target == BrushTarget::Global
        && state.subject_photo.get_untracked() == Some(drag.item_id)
    {
        let live = drag.live.borrow();
        let diag = ((drag.w * drag.w + drag.h * drag.h) as f32).sqrt();
        Some(GlobalBrushOp {
            points: live
                .points()
                .iter()
                .map(|&(x, y)| (x / drag.w as f32, y / drag.h as f32))
                .collect(),
            size: live.radius() * 2.0 / diag,
            softness: live.softness(),
            add: live.add(),
        })
    } else {
        None
    };
    push_brush_step(
        state,
        BrushStep {
            item_id: drag.item_id,
            target: drag.target,
            w: drag.w,
            h: drag.h,
            patches: vec![patch],
            op,
        },
    );
}

fn brush_store(
    state: AppState,
    item_id: usize,
    target: BrushTarget,
    data: Vec<u8>,
    w: usize,
    h: usize,
) -> bool {
    let sel = Selection {
        kind: SelectionKind::Mask { data: Rc::new(data), width: w, height: h },
        feather: 0.0,
    };
    match target {
        BrushTarget::Global => {
            state.update_item_by_id(item_id, |m| {
                m.edit.selection = Some(sel);
                m.edit.selection_rev += 1;
            });
            true
        }
        BrushTarget::Layer(id) => {
            let mut ok = false;
            state.update_item_by_id(item_id, |m| {
                if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                    l.set_mask(Some(sel));
                    ok = true;
                }
            });
            ok
        }
    }
}

fn patch_region_matches(mask: &[u8], mask_w: usize, p: &MaskPatch, expected: &[u8]) -> bool {
    if mask_w == 0 || expected.len() != p.w * p.h {
        return false;
    }
    let mask_h = mask.len() / mask_w;
    if p.x + p.w > mask_w || p.y + p.h > mask_h {
        return false;
    }
    for row in 0..p.h {
        let start = (p.y + row) * mask_w + p.x;
        if mask[start..start + p.w] != expected[row * p.w..(row + 1) * p.w] {
            return false;
        }
    }
    true
}

fn apply_patch_region(mask: &mut [u8], mask_w: usize, p: &MaskPatch, src: &[u8]) {
    for row in 0..p.h {
        let start = (p.y + row) * mask_w + p.x;
        mask[start..start + p.w].copy_from_slice(&src[row * p.w..(row + 1) * p.w]);
    }
}

/// Undo/redo replays dirty tiles. The stacks are only trusted while the mask
/// still matches the recorded bytes — any other selection edit (wand, Clear, a
/// fresh layer mask) invalidates them rather than corrupting the mask.
fn brush_history_step(state: AppState, undo: bool) -> bool {
    let step = state.brush_history.with(|h| {
        if undo { h.undo.last().cloned() } else { h.redo.last().cloned() }
    });
    let Some(step) = step else { return false };
    let Some(item) = state.current() else { return false };
    if item.id != step.item_id {
        return false;
    }
    let Some((w, h)) = brush_dims(&item) else { return false };
    if (w, h) != (step.w, step.h) {
        invalidate_brush_history(state);
        return false;
    }
    let Some(current) = brush_selection(&item, step.target) else {
        invalidate_brush_history(state);
        return false;
    };
    let current = ops::selection_mask(current, w, h);
    let all_valid = step.patches.iter().all(|p| {
        patch_region_matches(&current, w, p, if undo { &p.after } else { &p.before })
    });
    if !all_valid {
        if rederive_subject_step(state, &step, undo) {
            advance_brush_history(state, undo);
            return true;
        }
        invalidate_brush_history(state);
        return false;
    }
    let mut next = current;
    for p in &step.patches {
        apply_patch_region(&mut next, w, p, if undo { &p.before } else { &p.after });
    }
    if !brush_store(state, item.id, step.target, next, w, h) {
        invalidate_brush_history(state);
        return false;
    }
    match (undo, &step.op) {
        (true, Some(_)) => pop_subject_brush_op(step.item_id),
        (false, Some(op)) if state.subject_photo.get_untracked() == Some(step.item_id) => {
            push_subject_brush_op(step.item_id, op.clone());
        }
        _ => {}
    }
    advance_brush_history(state, undo);
    true
}

pub(crate) fn brush_undo(state: AppState) -> bool {
    brush_history_step(state, true)
}

pub(crate) fn brush_redo(state: AppState) -> bool {
    brush_history_step(state, false)
}

/// MobileSAM subject selection. `nx`/`ny` are full-image normalized coords
/// (post-geometry, pre-crop). Taps refine the live prompt set for this photo;
/// `negative` removes from the mask instead of adding.
pub(crate) fn subject_select(state: AppState, nx: f32, ny: f32, negative: bool) {
    subject_run(state, move |w, h| {
        let op = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&op, &"type".into(), &"tap".into());
        let _ = js_sys::Reflect::set(&op, &"x".into(), &JsValue::from_f64((nx * w as f32) as f64));
        let _ = js_sys::Reflect::set(&op, &"y".into(), &JsValue::from_f64((ny * h as f32) as f64));
        let label = if negative { 0.0 } else { 1.0 };
        let _ = js_sys::Reflect::set(&op, &"label".into(), &JsValue::from_f64(label));
        op.into()
    });
}

/// Drag-box subject prompt: one box replaces the prompt set (the worker sends
/// the top-left / bottom-right corner pair).
pub(crate) fn subject_box(state: AppState, x0: f32, y0: f32, x1: f32, y1: f32) {
    let (x0, x1) = (x0.min(x1), x0.max(x1));
    let (y0, y1) = (y0.min(y1), y0.max(y1));
    subject_run(state, move |w, h| {
        let op = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&op, &"type".into(), &"box".into());
        for (k, v) in [("x0", x0), ("y0", y0), ("x1", x1), ("y1", y1)] {
            let px = match k {
                "x0" | "x1" => v * w as f32,
                _ => v * h as f32,
            };
            let _ = js_sys::Reflect::set(&op, &k.into(), &JsValue::from_f64(px as f64));
        }
        op.into()
    });
}

fn subject_run(state: AppState, make_op: impl FnOnce(usize, usize) -> JsValue + 'static) {
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(photo) = get_photo(item.id) else { return };
    let (pix, pw, ph) = photo.preview.clone();
    if pw == 0 || ph == 0 {
        return;
    }
    // Segment the geometry-corrected preview: the tap coords are in that space
    // and the model should see the same frame the canvas shows.
    let (p, w, h) = geometry(&pix, pw, ph, &item.edit);
    let pix = p.into_owned();
    state.busy.set(Some("Segmenting subject…".into()));
    state.progress.set(0.0);
    spawn_local(async move {
        let canvas = web::create_canvas(w as u32, h as u32);
        web::put_pixels(&canvas, &pix, w as u32, h as u32);
        let Ok(blob) = web::canvas_to_blob(&canvas, "image/jpeg").await else {
            state.busy.set(None);
            return;
        };
        let Ok((img, url)) = web::load_image(&blob).await else {
            state.busy.set(None);
            return;
        };
        let fresh = state.subject_photo.get_untracked() != Some(item.id);
        let op = make_op(w, h);
        let on_progress = Closure::<dyn FnMut(f64, String)>::new(move |frac: f64, label: String| {
            state.progress.set(frac as f32);
            state.busy.set(Some(label));
        });
        let res = web::segment_subject(&img, item.id, &op, fresh, &on_progress).await;
        let _ = Url::revoke_object_url(&url);
        state.busy.set(None);
        state.progress.set(0.0);
        match res {
            Ok(m) if m.width > 0 && m.height > 0 && m.mask.len() == m.width * m.height => {
                if fresh {
                    start_new_subject_selection(state, item.id);
                }
                let mut mask = m.mask;
                SUBJECT_DECODE.with(|d| {
                    d.borrow_mut().insert(item.id, (mask.clone(), m.width, m.height));
                });
                replay_subject_brush_ops(item.id, &mut mask, m.width, m.height);
                let set = mask.iter().filter(|&&v| v > 0).count();
                web::log(&format!(
                    "subject mask iou={:.3} crop={} {}x{} set={}",
                    m.iou, m.used_crop, m.width, m.height, set
                ));
                state.update_current(|e| {
                    e.selection = Some(state::Selection {
                        kind: state::SelectionKind::Mask {
                            data: Rc::new(mask),
                            width: m.width,
                            height: m.height,
                        },
                        feather: 0.0,
                    });
                    e.selection_rev += 1;
                });
                state.subject_photo.set(Some(item.id));
            }
            Ok(_) => web::log("subject mask size mismatch"),
            Err(e) => web::log_err(&format!("subject segmentation failed: {e:?}")),
        }
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
                    m.layers.push(Layer::new_raster(id0, bg, pw, ph));
                    m.layers.push(Layer::new_raster(id1, subject, pw, ph));
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
