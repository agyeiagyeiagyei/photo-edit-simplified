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
/// pointerdown plus incrementally rasterized stroke state. `w`/`h` is the
/// committed mask space (up to the 3200 px raster cap); `view_*` is the
/// preview-resolution copy that paints the live tint, so the drag never pays
/// for a full-resolution canvas blit.
#[derive(Clone)]
pub(crate) struct BrushDrag {
    pub item_id: usize,
    pub target: BrushTarget,
    pub w: usize,
    pub h: usize,
    /// `None` when the target had no mask at pointerdown; the zero base is
    /// virtual so starting a stroke allocates no full-frame buffer.
    pub base: Option<Rc<Vec<u8>>>,
    pub live: Rc<RefCell<brush::LiveStroke>>,
    pub view_w: usize,
    pub view_h: usize,
    pub view_base: Option<Rc<Vec<u8>>>,
    pub view_live: Rc<RefCell<brush::LiveStroke>>,
    pub tint: Rc<RefCell<Option<BrushTint>>>,
    /// Overlay key of the mask this drag paints on, so the drag can start from
    /// the cached committed tint instead of rebuilding it.
    pub tint_key: u64,
    /// Frame the drag last painted fully. Matching frames paint only the dirty
    /// tint rect: re-blitting base + tint for every pointermove is tens of
    /// milliseconds of raster on a 24 MP preview in software canvas.
    pub frame: Rc<RefCell<Option<DragFrame>>>,
    /// The committed mask for this target is empty, so the tint canvas starts
    /// transparent and needs no base blit on the first stroke.
    pub view_empty: bool,
    /// The committed overlay is a vector outline, not a tint, so the first
    /// frame must paint the baked mask onto the canvas. A raster mask is
    /// already tinted on screen and blitting it again would double the alpha.
    pub base_blit: bool,
    /// Tight bounds of the committed mask in view pixels, for that blit.
    pub base_bbox: Option<(usize, usize, usize, usize)>,
}

#[derive(Clone)]
pub(crate) struct DragFrame {
    pub canvas: web_sys::HtmlCanvasElement,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub xf: [f64; 6],
    pub hi_gen: u32,
    pub viewport_gen: u32,
    /// Full paints since load. Any other paint invalidates the frame, so an
    /// incremental ring/dab can never land on a canvas showing something else.
    pub paint_gen: u32,
    /// The photo (plus layers) as painted in the same full pass, before the
    /// selection overlay. Incremental frames restore their dirty rect from it
    /// so tint can be removed, which drawImage alone cannot do.
    pub base: Option<web_sys::HtmlCanvasElement>,
}

thread_local! {
    static PAINT_GEN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub(crate) fn bump_paint_gen() {
    PAINT_GEN.with(|g| g.set(g.get().wrapping_add(1)));
}

fn paint_gen() -> u32 {
    PAINT_GEN.with(|g| g.get())
}

thread_local! {
    /// Frame (and canvas transform) on which the committed Select overlay was
    /// last painted in full. A brush drag that starts on that exact frame can
    /// paint its first dab incrementally instead of re-blitting the base.
    static LAST_FULL_FRAME: RefCell<Option<(usize, u64, DragFrame)>> =
        const { RefCell::new(None) };
}

pub(crate) fn remember_full_frame(
    item_id: usize,
    key: u64,
    ctx: &web_sys::CanvasRenderingContext2d,
    canvas: &web_sys::HtmlCanvasElement,
    hi_gen: u32,
    viewport_gen: u32,
) {
    let Ok(m) = ctx.get_transform() else { return };
    perf::record_detail("overlay.frame.remember", 0.0, Some(key.to_string()));
    let frame = DragFrame {
        canvas: canvas.clone(),
        canvas_w: canvas.width(),
        canvas_h: canvas.height(),
        xf: [m.a(), m.b(), m.c(), m.d(), m.e(), m.f()],
        hi_gen,
        viewport_gen,
        paint_gen: paint_gen(),
        base: frame_base(canvas),
    };
    LAST_FULL_FRAME.with(|f| *f.borrow_mut() = Some((item_id, key, frame)));
}

thread_local! {
    /// Photo pixels from the full paint that recorded the current brush frame,
    /// captured just before the selection overlay is composited. An incremental
    /// drag frame restores its dirty rect from here first: drawImage composites
    /// but cannot erase, so a Remove stroke would otherwise leave old tint on
    /// the canvas until an unrelated full repaint.
    static BASE_SNAPSHOT: RefCell<Option<FrameBase>> = const { RefCell::new(None) };
}

struct FrameBase {
    canvas: web_sys::HtmlCanvasElement,
    w: u32,
    h: u32,
    paint_gen: u32,
}

/// Copy the finished base layer of this paint. Call once per full paint, after
/// the photo and its layers are on the canvas and before any overlay.
pub(crate) fn snapshot_frame_base(canvas: &web_sys::HtmlCanvasElement) {
    let (w, h) = (canvas.width(), canvas.height());
    if w == 0 || h == 0 {
        clear_frame_base();
        return;
    }
    let gen = paint_gen();
    BASE_SNAPSHOT.with(|s| {
        let mut slot = s.borrow_mut();
        let scratch = match slot.take() {
            Some(old) if old.w == w && old.h == h => old.canvas,
            _ => web::create_canvas(w, h),
        };
        let ctx = web::ctx2d(&scratch);
        let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        ctx.clear_rect(0.0, 0.0, w as f64, h as f64);
        if ctx
            .draw_image_with_html_canvas_element(canvas, 0.0, 0.0)
            .is_ok()
        {
            *slot = Some(FrameBase { canvas: scratch, w, h, paint_gen: gen });
        }
    });
}

pub(crate) fn clear_frame_base() {
    BASE_SNAPSHOT.with(|s| {
        if s.borrow().is_some() {
            *s.borrow_mut() = None;
        }
    });
}

fn frame_base(canvas: &web_sys::HtmlCanvasElement) -> Option<web_sys::HtmlCanvasElement> {
    BASE_SNAPSHOT.with(|s| {
        s.borrow().as_ref().and_then(|b| {
            (b.w == canvas.width() && b.h == canvas.height() && b.paint_gen == paint_gen())
                .then(|| b.canvas.clone())
        })
    })
}

/// The remembered frame for this exact mask, if the canvas still shows it.
pub(crate) fn last_full_frame(
    item_id: usize,
    key: u64,
    hi_gen: u32,
    viewport_gen: u32,
) -> Option<DragFrame> {
    LAST_FULL_FRAME.with(|f| {
        let gen = paint_gen();
        f.borrow().as_ref().and_then(|(i, k, frame)| {
            (*i == item_id
                && *k == key
                && frame.hi_gen == hi_gen
                && frame.viewport_gen == viewport_gen
                && frame.paint_gen == gen)
                .then(|| frame.clone())
        })
    })
}

/// A commit that skipped its repaint leaves the adopted tint on screen under
/// the new selection revision; retag so the next drag still starts
/// incrementally.
pub(crate) fn retag_full_frame(key: u64) {
    LAST_FULL_FRAME.with(|f| {
        if let Some((_, k, _)) = f.borrow_mut().as_mut() {
            *k = key;
        }
    });
    // The idle ring can already be back on the canvas for this frame.
    RING_BACKUP.with(|b| {
        if let Some((rb, _)) = b.borrow_mut().as_mut() {
            rb.key = key;
        }
    });
}

/// Pixels the idle brush ring covers, captured just before it is stroked.
/// A drag's first incremental frame puts them back: the ring sits on the brush
/// edge, where the tint's alpha is zero, so tint alone cannot erase it.
struct RingBackup {
    item_id: usize,
    key: u64,
    canvas: web_sys::HtmlCanvasElement,
    canvas_w: u32,
    canvas_h: u32,
    hi_gen: u32,
    viewport_gen: u32,
    x: f64,
    y: f64,
}

thread_local! {
    static RING_BACKUP: RefCell<Option<(RingBackup, web_sys::HtmlCanvasElement)>> =
        const { RefCell::new(None) };
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn backup_brush_ring(
    item_id: usize,
    key: u64,
    ctx: &web_sys::CanvasRenderingContext2d,
    canvas: &web_sys::HtmlCanvasElement,
    px: f64,
    py: f64,
    radius: f64,
    hi_gen: u32,
    viewport_gen: u32,
) {
    let Ok(m) = ctx.get_transform() else { return };
    let sx = (m.a() * m.a() + m.b() * m.b()).sqrt();
    let sy = (m.c() * m.c() + m.d() * m.d()).sqrt();
    let cx = m.a() * px + m.c() * py + m.e();
    let cy = m.b() * px + m.d() * py + m.f();
    let cw = canvas.width() as f64;
    let ch = canvas.height() as f64;
    // +4 px: ring radius is the outer stroke edge plus antialiasing.
    let x0 = (cx - radius * sx - 4.0).floor().max(0.0);
    let y0 = (cy - radius * sy - 4.0).floor().max(0.0);
    let x1 = (cx + radius * sx + 4.0).ceil().min(cw);
    let y1 = (cy + radius * sy + 4.0).ceil().min(ch);
    if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
        RING_BACKUP.with(|b| *b.borrow_mut() = None);
        return;
    }
    let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    RING_BACKUP.with(|b| {
        let mut slot = b.borrow_mut();
        let scratch = match slot.take() {
            Some((_, c)) if c.width() == w && c.height() == h => c,
            _ => web::create_canvas(w, h),
        };
        let sctx = web::ctx2d(&scratch);
        let _ = sctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
            canvas,
            x0,
            y0,
            w as f64,
            h as f64,
            0.0,
            0.0,
            w as f64,
            h as f64,
        );
        *slot = Some((
            RingBackup {
                item_id,
                key,
                canvas: canvas.clone(),
                canvas_w: canvas.width(),
                canvas_h: canvas.height(),
                hi_gen,
                viewport_gen,
                x: x0,
                y: y0,
            },
            scratch,
        ));
    });
}

pub(crate) fn clear_brush_ring_backup() {
    RING_BACKUP.with(|b| *b.borrow_mut() = None);
}

fn restore_brush_ring(
    item_id: usize,
    key: u64,
    canvas: &web_sys::HtmlCanvasElement,
    hi_gen: u32,
    viewport_gen: u32,
) {
    RING_BACKUP.with(|b| {
        let Some((rb, pixels)) = b.borrow_mut().take() else { return };
        if rb.item_id != item_id
            || rb.key != key
            || !js_sys::Object::is(rb.canvas.as_ref(), canvas.as_ref())
            || rb.canvas_w != canvas.width()
            || rb.canvas_h != canvas.height()
            || rb.hi_gen != hi_gen
            || rb.viewport_gen != viewport_gen
        {
            return;
        }
        let ctx = web::ctx2d(canvas);
        let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        let _ = ctx.draw_image_with_html_canvas_element(&pixels, rb.x, rb.y);
    });
}

/// Repaint an idle brush cursor without touching the committed pixels: erase
/// the previous ring from its backup and stroke the new one. Returns false when
/// the canvas no longer shows the remembered frame, so the caller does a full
/// repaint. A 24 MP hover otherwise re-blits the whole sharp frame per move.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_brush_hover_incremental(
    canvas: &web_sys::HtmlCanvasElement,
    item_id: usize,
    key: u64,
    hover: Option<(f32, f32)>,
    radius: f64,
    w: f64,
    h: f64,
    hi_gen: u32,
    viewport_gen: u32,
) -> bool {
    let Some(frame) = last_full_frame(item_id, key, hi_gen, viewport_gen) else {
        return false;
    };
    if !js_sys::Object::is(frame.canvas.as_ref(), canvas.as_ref())
        || canvas.width() != frame.canvas_w
        || canvas.height() != frame.canvas_h
    {
        return false;
    }
    restore_brush_ring(item_id, key, canvas, hi_gen, viewport_gen);
    let Some((nx, ny)) = hover else { return true };
    let ctx = web::ctx2d(canvas);
    let [a, b, c, d, e, f] = frame.xf;
    let _ = ctx.set_transform(a, b, c, d, e, f);
    backup_brush_ring(
        item_id,
        key,
        &ctx,
        canvas,
        nx as f64 * w,
        ny as f64 * h,
        radius,
        hi_gen,
        viewport_gen,
    );
    draw_brush_outline(&ctx, nx, ny, radius, w, h);
    true
}

impl BrushDrag {
    /// Record the frame just painted, so later drag frames can repaint only
    /// their dirty rects on the same canvas size and transform.
    pub(crate) fn record_frame(
        &self,
        ctx: &web_sys::CanvasRenderingContext2d,
        canvas: &web_sys::HtmlCanvasElement,
        hi_gen: u32,
        viewport_gen: u32,
    ) {
        let m = ctx.get_transform().ok();
        // get_transform is infallible in practice; keep the last frame if not.
        let Some(m) = m else { return };
        *self.frame.borrow_mut() = Some(DragFrame {
            canvas: canvas.clone(),
            canvas_w: canvas.width(),
            canvas_h: canvas.height(),
            xf: [m.a(), m.b(), m.c(), m.d(), m.e(), m.f()],
            hi_gen,
            viewport_gen,
            paint_gen: paint_gen(),
            base: frame_base(canvas),
        });
    }
}

/// Paint only the tint pixels that changed since the last frame. Returns false
/// when the canvas no longer matches the recorded drag frame, so the caller
/// falls through to a full repaint.
pub(crate) fn draw_brush_drag_incremental(
    canvas: &web_sys::HtmlCanvasElement,
    drag: &BrushDrag,
    hi_gen: u32,
    viewport_gen: u32,
) -> bool {
    let _sp = perf::Span::new("brush.drag.incremental");
    let frame = { drag.frame.borrow().clone() };
    let Some(frame) = frame else {
        perf::record("brush.drag.miss", 0.0);
        return false;
    };
    if !js_sys::Object::is(frame.canvas.as_ref(), canvas.as_ref())
        || canvas.width() != frame.canvas_w
        || canvas.height() != frame.canvas_h
        || frame.hi_gen != hi_gen
        || frame.viewport_gen != viewport_gen
    {
        let reason = if !js_sys::Object::is(frame.canvas.as_ref(), canvas.as_ref()) {
            "canvas"
        } else if canvas.width() != frame.canvas_w || canvas.height() != frame.canvas_h {
            "dims"
        } else if frame.hi_gen != hi_gen {
            "hi"
        } else {
            "viewport"
        };
        perf::record("brush.drag.miss", 0.0);
        perf::record_detail("brush.drag.miss.why", 0.0, Some(reason.to_string()));
        return false;
    }
    let Some(base) = frame.base.clone() else {
        perf::record("brush.drag.miss", 0.0);
        perf::record_detail("brush.drag.miss.why", 0.0, Some("base".to_string()));
        return false;
    };
    let rgb = match drag.target {
        BrushTarget::Global => [10, 132, 255],
        BrushTarget::Layer(_) => [255, 59, 48],
    };
    let mut slot = drag.tint.borrow_mut();
    if slot.is_none() {
        let _s = perf::Span::new("brush.drag.tint_new");
        *slot = Some(BrushTint::new(drag.view_w, drag.view_h, rgb));
    }
    let Some(tint) = slot.as_mut() else { return false };
    {
        let _s = perf::Span::new("brush.drag.seed");
        seed_brush_tint(drag, tint, rgb);
    }
    restore_brush_ring(drag.item_id, drag.tint_key, canvas, hi_gen, viewport_gen);
    let ctx = web::ctx2d(canvas);
    let [a, b, c, d, e, f] = frame.xf;
    if !tint.base_blitted {
        if drag.base_blit {
            // The committed selection was a vector outline; bake its mask in
            // now, bounded to its coverage.
            if let Some((x, y, w, h)) = drag.base_bbox {
                let _s = perf::Span::new("brush.drag.base_blit");
                let _ = ctx.set_transform(a, b, c, d, e, f);
                let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    &tint.canvas,
                    x as f64,
                    y as f64,
                    w as f64,
                    h as f64,
                    x as f64,
                    y as f64,
                    w as f64,
                    h as f64,
                );
                let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
            }
        }
        tint.base_blitted = true;
    }
    let mut live = drag.view_live.borrow_mut();
    let Some(r) = live.take_dirty() else { return true };
    let mut vals = vec![0u8; r.w * r.h];
    {
        let _s = perf::Span::new("brush.drag.fill");
        live.fill_live_rect(r, &mut vals);
        tint.paint_mask(&vals, r.x, r.y, r.w, r.h);
    }
    // Put the untouched photo back under the dirty rect, then composite the
    // tint's full state there. Drawing the tint alone leaves removed pixels
    // untouched (transparent source paints nothing).
    let (bx, by, bw, bh) = device_rect(frame.xf, r, frame.canvas_w, frame.canvas_h);
    let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
    if bw > 0.0 && bh > 0.0 {
        let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
            &base, bx, by, bw, bh, bx, by, bw, bh,
        );
    }
    let _s = perf::Span::new("brush.drag.blit");
    let _ = ctx.set_transform(a, b, c, d, e, f);
    let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
        &tint.canvas,
        r.x as f64,
        r.y as f64,
        r.w as f64,
        r.h as f64,
        r.x as f64,
        r.y as f64,
        r.w as f64,
        r.h as f64,
    );
    let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
    true
}

/// Map a view-space dirty rect onto canvas backing pixels through the overlay
/// transform, clamped to the canvas.
fn device_rect(
    [a, b, c, d, e, f]: [f64; 6],
    r: brush::Rect,
    cw: u32,
    ch: u32,
) -> (f64, f64, f64, f64) {
    let x0 = (a * r.x as f64 + c * r.y as f64 + e).max(0.0);
    let y0 = (b * r.x as f64 + d * r.y as f64 + f).max(0.0);
    let x1 = (a * (r.x + r.w) as f64 + c * (r.y + r.h) as f64 + e).min(cw as f64);
    let y1 = (b * (r.x + r.w) as f64 + d * (r.y + r.h) as f64 + f).min(ch as f64);
    (x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
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

/// Cache key for a mask overlay: the selection owner and its version, so a
/// fresh mask repaints once and unrelated renders (hover moves, thumbnails)
/// reuse the tint canvas instead of allocating a full-frame RGBA buffer each
/// pass.
pub(crate) fn overlay_key(owner: u64, layer: u64, version: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    owner.hash(&mut h);
    layer.hash(&mut h);
    version.hash(&mut h);
    h.finish()
}

pub(crate) fn draw_selection_overlay(
    ctx: &web_sys::CanvasRenderingContext2d,
    sel: &Selection,
    w: f64,
    h: f64,
    key: u64,
) {
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
            paint_mask_tint(ctx, &mask, wi, hi, [10, 132, 255], key);
        }
    }
    ctx.restore();
}

struct TintCache {
    key: u64,
    w: usize,
    h: usize,
    rgb: [u8; 3],
    canvas: web_sys::HtmlCanvasElement,
}

thread_local! {
    static TINT_CACHE: std::cell::RefCell<Option<TintCache>> = std::cell::RefCell::new(None);
}

/// The cached tint canvas for exactly this overlay, if the last paint matched.
pub(crate) fn cached_mask_tint(
    key: u64,
    w: usize,
    h: usize,
    rgb: [u8; 3],
) -> Option<web_sys::HtmlCanvasElement> {
    TINT_CACHE.with(|cache| {
        cache.borrow().as_ref().and_then(|c| {
            (c.key == key && c.w == w && c.h == h && c.rgb == rgb).then(|| c.canvas.clone())
        })
    })
}

/// Hand a finished stroke's tint canvas to the overlay cache. The live tint
/// already shows the committed mask, so the commit repaint can blit it instead
/// of rebuilding a full-frame RGBA buffer.
pub(crate) fn adopt_mask_tint(
    key: u64,
    w: usize,
    h: usize,
    rgb: [u8; 3],
    canvas: web_sys::HtmlCanvasElement,
) {
    TINT_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(TintCache { key, w, h, rgb, canvas });
    });
}

fn paint_mask_tint(
    ctx: &web_sys::CanvasRenderingContext2d,
    mask: &[u8],
    w: usize,
    h: usize,
    rgb: [u8; 3],
    key: u64,
) {
    let canvas = TINT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(c) = cache.as_ref() {
            if c.key == key && c.w == w && c.h == h && c.rgb == rgb {
                return c.canvas.clone();
            }
        }
        let _sp = perf::Span::new("overlay.tint.build");
        let mut tint = vec![0u8; w * h * 4];
        for (m, px) in mask.iter().zip(tint.as_chunks_mut::<4>().0) {
            if *m == 0 {
                continue;
            }
            px[0] = rgb[0];
            px[1] = rgb[1];
            px[2] = rgb[2];
            px[3] = *m / 3;
        }
        let canvas = web::create_canvas(w as u32, h as u32);
        web::put_pixels(&canvas, &tint, w as u32, h as u32);
        *cache = Some(TintCache { key, w, h, rgb, canvas: canvas.clone() });
        canvas
    });
    let _sp = perf::Span::new("overlay.tint.draw");
    let _ = ctx.draw_image_with_html_canvas_element(&canvas, 0.0, 0.0);
}

/// Editor-only tint of an adjustment layer's mask. An unmasked layer has no
/// shape to show, and a fully-inverted empty mask would only wash the frame.
pub(crate) fn draw_layer_mask_overlay(
    ctx: &web_sys::CanvasRenderingContext2d,
    sel: Option<&Selection>,
    invert: bool,
    w: f64,
    h: f64,
    key: u64,
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
    paint_mask_tint(ctx, &mask, wi, hi, [255, 59, 48], key ^ invert as u64);
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

/// Cached tint canvas for an in-flight stroke: the base mask is painted once,
/// then each frame only the stroke's dirty rect is repainted.
pub(crate) struct BrushTint {
    canvas: web_sys::HtmlCanvasElement,
    ctx: web_sys::CanvasRenderingContext2d,
    rgb: [u8; 3],
    ready: bool,
    /// The committed base mask has been painted onto the canvas since the
    /// tint was created.
    base_blitted: bool,
}

impl BrushTint {
    fn new(w: usize, h: usize, rgb: [u8; 3]) -> Self {
        let canvas = web::create_canvas(w as u32, h as u32);
        let ctx = web::ctx2d(&canvas);
        Self { canvas, ctx, rgb, ready: false, base_blitted: false }
    }

    pub(crate) fn canvas(&self) -> web_sys::HtmlCanvasElement {
        self.canvas.clone()
    }

    /// Paint a w×h mask at (x, y) in canvas pixels.
    fn paint_mask(&self, mask: &[u8], x: usize, y: usize, w: usize, h: usize) {
        let mut rgba = vec![0u8; w * h * 4];
        for (i, &m) in mask.iter().enumerate() {
            if m == 0 {
                continue;
            }
            rgba[i * 4] = self.rgb[0];
            rgba[i * 4 + 1] = self.rgb[1];
            rgba[i * 4 + 2] = self.rgb[2];
            rgba[i * 4 + 3] = m / 3;
        }
        let arr = js_sys::Uint8ClampedArray::from(&rgba[..]);
        if let Ok(data) = web_sys::ImageData::new_with_js_u8_clamped_array_and_sh(
            &arr,
            w as u32,
            h as u32,
        ) {
            let _ = self.ctx.put_image_data(&data, x as f64, y as f64);
        }
    }
}

/// Live selection-brush tint: the captured mask plus the in-progress stroke.
pub(crate) fn draw_brush_drag(ctx: &web_sys::CanvasRenderingContext2d, drag: &BrushDrag) {
    let rgb = match drag.target {
        BrushTarget::Global => [10, 132, 255],
        BrushTarget::Layer(_) => [255, 59, 48],
    };
    let mut slot = drag.tint.borrow_mut();
    let tint = slot.get_or_insert_with(|| BrushTint::new(drag.view_w, drag.view_h, rgb));
    seed_brush_tint(drag, tint, rgb);
    let mut live = drag.view_live.borrow_mut();
    if let Some(r) = live.take_dirty() {
        let mut vals = vec![0u8; r.w * r.h];
        live.fill_live_rect(r, &mut vals);
        tint.paint_mask(&vals, r.x, r.y, r.w, r.h);
    }
    let _ = ctx.draw_image_with_html_canvas_element(&tint.canvas, 0.0, 0.0);
    tint.base_blitted = true;
}

/// First paint of a drag: copy the committed overlay tint when it is cached
/// for exactly this mask, else rasterize the base mask. Copying one canvas
/// avoids a full-frame RGBA build on every pointerdown.
fn seed_brush_tint(drag: &BrushDrag, tint: &mut BrushTint, rgb: [u8; 3]) {
    if tint.ready {
        return;
    }
    if drag.view_empty {
        tint.ready = true;
        return;
    }
    match cached_mask_tint(drag.tint_key, drag.view_w, drag.view_h, rgb) {
        Some(src) => {
            let _ = tint
                .ctx
                .draw_image_with_html_canvas_element(&src, 0.0, 0.0);
        }
        None => {
            if let Some(view_base) = &drag.view_base {
                tint.paint_mask(view_base, 0, 0, drag.view_w, drag.view_h);
            }
        }
    }
    tint.ready = true;
}

/// Paint any live dirt still pending into the drag's tint canvas (the commit
/// adopts that canvas, so it must include the final tip).
pub(crate) fn flush_brush_tint(drag: &BrushDrag) {
    let rgb = match drag.target {
        BrushTarget::Global => [10, 132, 255],
        BrushTarget::Layer(_) => [255, 59, 48],
    };
    let mut slot = drag.tint.borrow_mut();
    let Some(tint) = slot.as_mut() else { return };
    seed_brush_tint(drag, tint, rgb);
    let mut live = drag.view_live.borrow_mut();
    if let Some(r) = live.take_dirty() {
        let mut vals = vec![0u8; r.w * r.h];
        live.fill_live_rect(r, &mut vals);
        tint.paint_mask(&vals, r.x, r.y, r.w, r.h);
    }
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
