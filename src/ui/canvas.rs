//! Canvas edit helpers: pen/select drag state, hit testing, path/brush history and overlay painting.

use crate::prelude::*;
use crate::*;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum PenHandle {
    Point,
    Out,
    In,
}

// Variants name the handle being dragged; the shared prefix is the point.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum PenDrag {
    MovePoint(usize),
    MoveOut(usize),
    MoveIn(usize),
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) enum SelectDrag {
    Rect((f32, f32), (f32, f32)),
    Lasso(Vec<(f32, f32)>),
    /// Subject prompt drag; a release below the box threshold is a tap.
    Subject((f32, f32), (f32, f32)),
}

/// In-flight selection-brush stroke: the target's mask captured at
/// pointerdown plus the pointer path in normalized working coords.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct BrushDrag {
    pub item_id: usize,
    pub target: BrushTarget,
    pub w: usize,
    pub h: usize,
    pub base: Rc<Vec<u8>>,
    pub points: Vec<(f32, f32)>,
    /// Stroke radius in mask pixels.
    pub radius: f32,
    pub softness: f32,
    pub add: bool,
}

pub(crate) fn crop_is_full(c: &state::CropRect) -> bool {
    c.x <= 0.0005 && c.y <= 0.0005 && c.w >= 0.999 && c.h >= 0.999
}

// Pointer position in full-image normalized coords. The preview canvas shows
// only the crop region outside the Crop tab, so canvas-local coords map
// through the crop rect (identity when uncropped).
pub(crate) fn layer_norm_pos(ev: &web_sys::MouseEvent, crop: state::CropRect) -> Option<(f32, f32)> {
    let el = web::document().query_selector(".canvas-wrap").ok().flatten()?;
    let rect = el.get_bounding_client_rect();
    let nx = ((ev.client_x() as f32 - rect.left() as f32) / rect.width() as f32).clamp(0.0, 1.0);
    let ny = ((ev.client_y() as f32 - rect.top() as f32) / rect.height() as f32).clamp(0.0, 1.0);
    Some((crop.x + nx * crop.w, crop.y + ny * crop.h))
}

pub(crate) fn dist2(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    dx * dx + dy * dy
}

pub(crate) fn hit_test_path(points: &[PathPoint], nx: f32, ny: f32) -> Option<(usize, PenHandle)> {
    let point_r2 = 0.03_f32 * 0.03;
    let handle_r2 = 0.025_f32 * 0.025;
    // Points take priority over handles.
    for (i, p) in points.iter().enumerate().rev() {
        if dist2((p.x, p.y), (nx, ny)) < point_r2 {
            return Some((i, PenHandle::Point));
        }
    }
    for (i, p) in points.iter().enumerate().rev() {
        if dist2((p.x + p.out_x, p.y + p.out_y), (nx, ny)) < handle_r2 {
            return Some((i, PenHandle::Out));
        }
        if dist2((p.x + p.in_x, p.y + p.in_y), (nx, ny)) < handle_r2 {
            return Some((i, PenHandle::In));
        }
    }
    None
}

pub(crate) fn push_path_history(layer: &mut state::PathLayer) {
    if layer.history.len() >= 50 {
        layer.history.remove(0);
    }
    layer.history.push(layer.points.clone());
}

pub(crate) fn push_brush_history(layer: &mut state::BrushLayer) {
    if layer.history.len() >= 50 {
        layer.history.remove(0);
    }
    layer.history.push(layer.strokes.clone());
}

pub(crate) fn draw_path_edit_handles(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Path(p) = &layer.kind else { return };
    for pt in &p.points {
        let px = (pt.x * w as f32) as f64;
        let py = (pt.y * h as f32) as f64;
        let ox = ((pt.x + pt.out_x) * w as f32) as f64;
        let oy = ((pt.y + pt.out_y) * h as f32) as f64;
        let ix = ((pt.x + pt.in_x) * w as f32) as f64;
        let iy = ((pt.y + pt.in_y) * h as f32) as f64;

        ctx.set_stroke_style_str("rgba(255,255,255,0.6)");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(px, py);
        ctx.line_to(ox, oy);
        ctx.move_to(px, py);
        ctx.line_to(ix, iy);
        ctx.stroke();

        ctx.set_fill_style_str(if pt.smooth { "#0a84ff" } else { "#ffcc00" });
        ctx.begin_path();
        ctx.arc(px, py, 6.0, 0.0, std::f64::consts::PI * 2.0).unwrap();
        ctx.fill();

        ctx.set_fill_style_str("#fff");
        for (hx, hy) in [(ox, oy), (ix, iy)] {
            ctx.begin_path();
            ctx.arc(hx, hy, 4.0, 0.0, std::f64::consts::PI * 2.0).unwrap();
            ctx.fill();
        }
    }
}

pub(crate) fn draw_selection_overlay(ctx: &web_sys::CanvasRenderingContext2d, sel: &Selection, w: f64, h: f64) {
    ctx.save();
    ctx.set_stroke_style_str("#0a84ff");
    ctx.set_line_width(2.0);
    match &sel.kind {
        SelectionKind::Rect { x, y, w: rw, h: rh } => {
            ctx.stroke_rect(
                (*x * w as f32) as f64,
                (*y * h as f32) as f64,
                (*rw * w as f32) as f64,
                (*rh * h as f32) as f64,
            );
        }
        SelectionKind::Lasso(poly) => {
            if poly.len() < 2 {
                ctx.restore();
                return;
            }
            ctx.begin_path();
            let (x0, y0) = poly[0];
            ctx.move_to((x0 * w as f32) as f64, (y0 * h as f32) as f64);
            for &(x, y) in &poly[1..] {
                ctx.line_to((x * w as f32) as f64, (y * h as f32) as f64);
            }
            ctx.close_path();
            ctx.stroke();
        }
        SelectionKind::Mask { .. } => {
            let (wi, hi) = (w as usize, h as usize);
            let mask = ops::selection_mask(sel, wi, hi);
            paint_mask_tint(ctx, &mask, wi, hi, [10, 132, 255]);
        }
    }
    ctx.restore();
}

fn paint_mask_tint(ctx: &web_sys::CanvasRenderingContext2d, mask: &[u8], w: usize, h: usize, rgb: [u8; 3]) {
    let mut tint = vec![0u8; w * h * 4];
    for (i, &m) in mask.iter().enumerate() {
        if m == 0 {
            continue;
        }
        tint[i * 4] = rgb[0];
        tint[i * 4 + 1] = rgb[1];
        tint[i * 4 + 2] = rgb[2];
        tint[i * 4 + 3] = m / 3;
    }
    let tmp = web::create_canvas(w as u32, h as u32);
    web::put_pixels(&tmp, &tint, w as u32, h as u32);
    let _ = ctx.draw_image_with_html_canvas_element(&tmp, 0.0, 0.0);
}

/// Editor-only tint of an adjustment layer's mask. An unmasked layer has no
/// shape to show, and a fully-inverted empty mask would only wash the frame.
pub(crate) fn draw_layer_mask_overlay(
    ctx: &web_sys::CanvasRenderingContext2d,
    sel: Option<&Selection>,
    invert: bool,
    w: f64,
    h: f64,
) {
    let Some(sel) = sel else { return };
    let (wi, hi) = (w as usize, h as usize);
    if wi == 0 || hi == 0 {
        return;
    }
    let mut mask = ops::selection_mask(sel, wi, hi);
    if invert {
        for v in &mut mask {
            *v = 255 - *v;
        }
    }
    paint_mask_tint(ctx, &mask, wi, hi, [255, 59, 48]);
}

pub(crate) fn draw_select_drag(ctx: &web_sys::CanvasRenderingContext2d, drag: &SelectDrag, w: f64, h: f64) {
    ctx.save();
    ctx.set_stroke_style_str("rgba(10,132,255,0.8)");
    ctx.set_line_width(2.0);
    match drag {
        SelectDrag::Rect((x0, y0), (x1, y1)) | SelectDrag::Subject((x0, y0), (x1, y1)) => {
            let x = x0.min(*x1);
            let y = y0.min(*y1);
            let rw = (x1 - x0).abs();
            let rh = (y1 - y0).abs();
            ctx.stroke_rect(
                (x * w as f32) as f64,
                (y * h as f32) as f64,
                (rw * w as f32) as f64,
                (rh * h as f32) as f64,
            );
        }
        SelectDrag::Lasso(poly) => {
            if poly.len() < 2 {
                ctx.restore();
                return;
            }
            ctx.begin_path();
            let (x0, y0) = poly[0];
            ctx.move_to((x0 * w as f32) as f64, (y0 * h as f32) as f64);
            for &(x, y) in &poly[1..] {
                ctx.line_to((x * w as f32) as f64, (y * h as f32) as f64);
            }
            ctx.stroke();
        }
    }
    ctx.restore();
}

/// Live selection-brush tint: the captured mask plus the in-progress stroke.
pub(crate) fn draw_brush_drag(ctx: &web_sys::CanvasRenderingContext2d, drag: &BrushDrag) {
    let mask = brush_live_mask(drag);
    let rgb = match drag.target {
        BrushTarget::Global => [10, 132, 255],
        BrushTarget::Layer(_) => [255, 59, 48],
    };
    paint_mask_tint(ctx, &mask, drag.w, drag.h, rgb);
}

/// Brush outline: a white ring with a dark backing so it reads on any photo.
pub(crate) fn draw_brush_outline(
    ctx: &web_sys::CanvasRenderingContext2d,
    nx: f32,
    ny: f32,
    radius: f64,
    w: f64,
    h: f64,
) {
    let px = nx as f64 * w;
    let py = ny as f64 * h;
    ctx.save();
    ctx.set_line_width(2.0);
    ctx.set_stroke_style_str("rgba(0,0,0,0.55)");
    ctx.begin_path();
    let _ = ctx.arc(px, py, radius + 1.0, 0.0, std::f64::consts::PI * 2.0);
    ctx.stroke();
    ctx.set_line_width(1.5);
    ctx.set_stroke_style_str("rgba(255,255,255,0.95)");
    ctx.begin_path();
    let _ = ctx.arc(px, py, radius, 0.0, std::f64::consts::PI * 2.0);
    ctx.stroke();
    ctx.restore();
}

/// Heal-brush preview: soft translucent discs along the in-progress stroke.
pub(crate) fn draw_heal_overlay(
    ctx: &web_sys::CanvasRenderingContext2d,
    pts: Option<&[(f32, f32)]>,
    radius_frac: f32,
    w: f64,
    h: f64,
) {
    let Some(pts) = pts else { return };
    if pts.is_empty() {
        return;
    }
    let r = radius_frac as f64 * (w * w + h * h).sqrt();
    ctx.save();
    ctx.set_fill_style_str("rgba(255,255,255,0.35)");
    ctx.set_stroke_style_str("rgba(255,255,255,0.9)");
    ctx.set_line_width(1.5);
    for &(nx, ny) in pts {
        ctx.begin_path();
        let _ = ctx.arc(nx as f64 * w, ny as f64 * h, r, 0.0, std::f64::consts::TAU);
        ctx.fill();
        ctx.stroke();
    }
    ctx.restore();
}

pub(crate) fn draw_crosshair(ctx: &web_sys::CanvasRenderingContext2d, x: f32, y: f32) {
    ctx.save();
    ctx.set_stroke_style_str("rgba(10,132,255,0.9)");
    ctx.set_line_width(1.5);
    ctx.begin_path();
    ctx.move_to(x as f64 - 9.0, y as f64);
    ctx.line_to(x as f64 + 9.0, y as f64);
    ctx.move_to(x as f64, y as f64 - 9.0);
    ctx.line_to(x as f64, y as f64 + 9.0);
    ctx.stroke();
    ctx.begin_path();
    let _ = ctx.arc(x as f64, y as f64, 4.0, 0.0, std::f64::consts::TAU);
    ctx.stroke();
    ctx.restore();
}

pub(crate) fn text_overlay_style(t: &state::TextLayer, opacity: f32, crop: &state::CropRect) -> String {
    let px = format!("{:.2}%", t.font_size * 100.0);
    // Overlay coords are % of the (possibly cropped) preview; layer coords are
    // full-image normalized, so map through the crop rect.
    let left = format!("{:.2}%", (t.x - crop.x) / crop.w * 100.0);
    let top = format!("{:.2}%", (t.y - crop.y) / crop.h * 100.0);
    let align = t.alignment.canvas_value();
    let translate = match t.alignment {
        TextAlign::Left => "translate(0,-50%)",
        TextAlign::Center => "translate(-50%,-50%)",
        TextAlign::Right => "translate(-100%,-50%)",
    };
    let transform = format!("{} rotate({}deg)", translate, t.angle);
    let shadow = format!(
        "{}px {}px {}px {}",
        t.shadow_offset_x, t.shadow_offset_y, t.shadow_blur, t.shadow_color
    );
    let stroke = if t.stroke_width > 0.0 {
        format!("-webkit-text-stroke: {}em {}", t.stroke_width, t.stroke_color)
    } else {
        String::new()
    };
    format!(
        "position:absolute;left:{};top:{};transform:{};font-family:'{}',sans-serif;\
         font-weight:{};font-size:{};color:{};text-align:{};text-shadow:{};opacity:{};white-space:nowrap;{}",
        left, top, transform, t.font_family, t.font_weight, px, t.color, align, shadow, opacity, stroke
    )
}

pub(crate) fn selected_text_overlay_style(t: &state::TextLayer, opacity: f32, crop: &state::CropRect) -> String {
    let px = format!("{:.2}%", t.font_size * 100.0);
    let left = format!("{:.2}%", (t.x - crop.x) / crop.w * 100.0);
    let top = format!("{:.2}%", (t.y - crop.y) / crop.h * 100.0);
    let align = t.alignment.canvas_value();
    let translate = match t.alignment {
        TextAlign::Left => "translate(0,-50%)",
        TextAlign::Center => "translate(-50%,-50%)",
        TextAlign::Right => "translate(-100%,-50%)",
    };
    let transform = format!("{} rotate({}deg)", translate, t.angle);
    format!(
        "position:absolute;left:{};top:{};transform:{};font-family:'{}',sans-serif;\
         font-weight:{};font-size:{};color:transparent;text-align:{};opacity:{};white-space:nowrap;\
         border:2px dashed #0a84ff;border-radius:4px;background:rgba(10,132,255,0.08);",
        left, top, transform, t.font_family, t.font_weight, px, align, opacity
    )
}
