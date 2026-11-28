//! The Select tab's Pen: anchors with Bézier handles that close into a
//! selection, which the mask panel then adds to or removes from a mask.

use crate::prelude::*;
use crate::state::{flatten_path, PenPath};

/// An anchor or handle being dragged: where the press landed and the anchor
/// as it was then, so every move sets the position from the whole drag.
struct PenDragState {
    idx: usize,
    handle: PenHandle,
    start: (f32, f32),
    orig: PathPoint,
    /// Alt/Option held: a handle moves alone and its anchor becomes a corner.
    alone: bool,
    moved: bool,
}

thread_local! {
    static DRAG: RefCell<Option<PenDragState>> = const { RefCell::new(None) };
}

/// An open path of this photo, or a closed one whose outline is still the
/// photo's selection. Any other selection, a Clear, or adding the selection
/// to a mask retires a closed path.
pub(crate) fn pen_is_live(p: &PenPath, item: &MediaItem) -> bool {
    p.item_id == item.id
        && (!p.closed
            || matches!(&item.edit.selection, Some(Selection { kind: SelectionKind::Lasso(pts), .. }) if *pts == p.poly))
}

/// `Some(can close)` while the current photo has an open path.
pub(crate) fn pen_open(state: AppState) -> Option<bool> {
    let id = state.selected.get();
    state.pen_path.with(|p| p.as_ref().filter(|p| !p.closed && Some(p.item_id) == id).map(|p| pen_can_close(&p.points)))
}

/// Three anchors, or two with a curve between them.
fn pen_can_close(points: &[PathPoint]) -> bool {
    points.len() >= 3
        || points.len() == 2 && points.iter().any(|p| [p.in_x, p.in_y, p.out_x, p.out_y] != [0.0; 4])
}

/// The anchor or handle within `r` screen pixels of `at`, at the current
/// view. Anchors win over handles, later ones over earlier ones, and a handle
/// still on its anchor is not a handle.
fn pen_hit(state: AppState, item: &MediaItem, points: &[PathPoint], at: (f32, f32), r: f32) -> Option<(usize, PenHandle)> {
    let (cw, ch) = canvas_css_size()?;
    let (win, c) = (state.view_zoom.get_untracked().window(), item.edit.crop);
    let (sx, sy) = (cw / (win.w * c.w).max(1e-6), ch / (win.h * c.h).max(1e-6));
    let near = |x: f32, y: f32| ((x - at.0) * sx).hypot((y - at.1) * sy) <= r;
    if let Some(i) = points.iter().rposition(|p| near(p.x, p.y)) {
        return Some((i, PenHandle::Point));
    }
    points.iter().enumerate().rev().find_map(|(i, p)| {
        if (p.out_x, p.out_y) != (0.0, 0.0) && near(p.x + p.out_x, p.y + p.out_y) {
            Some((i, PenHandle::Out))
        } else if (p.in_x, p.in_y) != (0.0, 0.0) && near(p.x + p.in_x, p.y + p.in_y) {
            Some((i, PenHandle::In))
        } else {
            None
        }
    })
}

/// A press with the Pen. The first anchor of an open path closes it; another
/// anchor or a handle starts a drag (Alt/Option on an anchor pulls out new
/// handles). Elsewhere it adds an anchor, starting a new path once the last
/// one is closed, and a drag from there pulls out its handles.
pub(crate) fn pen_down(state: AppState, item: &MediaItem, at: (f32, f32), r: f32, alt: bool) {
    let mut p = state
        .pen_path
        .get_untracked()
        .filter(|p| pen_is_live(p, item))
        .unwrap_or(PenPath { item_id: item.id, points: Vec::new(), closed: true, poly: Vec::new() });
    let open = !p.closed;
    let drag = if open && pen_can_close(&p.points) && pen_hit(state, item, &p.points[..1], at, r).is_some() {
        close_pen(state);
        None
    } else if let Some((idx, handle)) = pen_hit(state, item, &p.points, at, r) {
        let orig = p.points[idx].clone();
        Some(if handle == PenHandle::Point && alt {
            PenDragState { idx, handle: PenHandle::Out, start: at, orig: PathPoint::new(orig.x, orig.y), alone: false, moved: false }
        } else {
            PenDragState { idx, handle, start: at, orig, alone: alt, moved: false }
        })
    } else {
        if !open {
            p = PenPath { item_id: item.id, points: Vec::new(), closed: false, poly: Vec::new() };
        }
        p.points.push(PathPoint::new(at.0, at.1));
        let idx = p.points.len() - 1;
        state.pen_path.set(Some(p));
        Some(PenDragState { idx, handle: PenHandle::Out, start: at, orig: PathPoint::new(at.0, at.1), alone: false, moved: false })
    };
    DRAG.with(|d| *d.borrow_mut() = drag);
}

pub(crate) fn pen_dragging() -> bool {
    DRAG.with(|d| d.borrow().is_some())
}

pub(crate) fn pen_move(state: AppState, at: (f32, f32)) {
    DRAG.with(|d| {
        let mut d = d.borrow_mut();
        let Some(d) = d.as_mut() else { return };
        let (dx, dy) = (at.0 - d.start.0, at.1 - d.start.1);
        if !d.moved && dx == 0.0 && dy == 0.0 {
            return;
        }
        d.moved = true;
        let o = &d.orig;
        state.pen_path.update(|p| {
            let Some(pt) = p.as_mut().and_then(|p| p.points.get_mut(d.idx)) else { return };
            if d.handle == PenHandle::Point {
                // Handles are relative, so they travel with their anchor.
                pt.x = (o.x + dx).clamp(0.0, 1.0);
                pt.y = (o.y + dy).clamp(0.0, 1.0);
                return;
            }
            let out = d.handle == PenHandle::Out;
            let (hx, hy) = if out { (o.out_x, o.out_y) } else { (o.in_x, o.in_y) };
            let moved = ((hx + dx).clamp(-1.0, 1.0), (hy + dy).clamp(-1.0, 1.0));
            pt.smooth = o.smooth && !d.alone;
            let other = match (pt.smooth, out) {
                (true, _) => (-moved.0, -moved.1),
                (false, true) => (o.in_x, o.in_y),
                (false, false) => (o.out_x, o.out_y),
            };
            let (a, b) = if out { (moved, other) } else { (other, moved) };
            (pt.out_x, pt.out_y, pt.in_x, pt.in_y) = (a.0, a.1, b.0, b.1);
        });
    });
}

/// End a drag; a reshaped closed path makes the selection again.
pub(crate) fn pen_up(state: AppState) {
    let moved = DRAG.with(|d| d.borrow_mut().take()).is_some_and(|d| d.moved);
    if moved && state.pen_path.with_untracked(|p| p.as_ref().is_some_and(|p| p.closed)) {
        write_pen_selection(state, None);
    }
}

/// Close the open path: its outline becomes the photo's selection, hard-edged
/// like a fresh lasso.
pub(crate) fn close_pen(state: AppState) {
    if pen_open(state) == Some(true) {
        state.pen_path.update(|p| {
            if let Some(p) = p {
                p.closed = true;
            }
        });
        write_pen_selection(state, Some(0.0));
    }
}

/// The closed path's outline as the selection, keeping the selection's
/// feather unless one is given.
fn write_pen_selection(state: AppState, feather: Option<f32>) {
    let Some(item) = state.current() else { return };
    let Some(mut p) = state.pen_path.get_untracked() else { return };
    let feather = feather.unwrap_or_else(|| item.edit.selection.as_ref().map_or(0.0, |s| s.feather));
    // A quarter pixel at 8192 px on the long side: finer than any frame the
    // selection is drawn at, whatever size the photo reports.
    let (w, h) = working_dims(item.width, item.height, &item.edit);
    let s = 8192.0 / w.max(h).max(1) as f32;
    p.poly = flatten_path(&p.points, true, w as f32 * s, h as f32 * s);
    let poly = p.poly.clone();
    batch(|| {
        state.pen_path.set(Some(p));
        clear_subject_brush_ops(state);
        state.subject_photo.set(None);
        state.update_current(|e| {
            e.selection = Some(Selection { kind: SelectionKind::Lasso(poly), feather });
            e.selection_rev += 1;
        });
    });
}

/// Take back the open path's last anchor; taking back the only one drops it.
pub(crate) fn pen_undo_point(state: AppState) {
    if pen_open(state).is_some() {
        state.pen_path.update(|slot| {
            if slot.as_mut().is_some_and(|p| p.points.pop().is_some() && p.points.is_empty()) {
                *slot = None;
            }
        });
    }
}

/// Double-click on an anchor: drop its handles, for a sharp corner.
pub(crate) fn pen_sharpen(state: AppState, item: &MediaItem, at: (f32, f32)) {
    let Some(mut p) = state.pen_path.get_untracked().filter(|p| pen_is_live(p, item)) else { return };
    let Some((i, PenHandle::Point)) = pen_hit(state, item, &p.points, at, 12.0) else { return };
    p.points[i] = PathPoint::new(p.points[i].x, p.points[i].y);
    let closed = p.closed;
    state.pen_path.set(Some(p));
    if closed {
        write_pen_selection(state, None);
    }
}

/// With the Pen on, Enter closes the open path, Backspace or Delete takes
/// back its last anchor, and Escape drops it. True when the key was the Pen's.
pub(crate) fn pen_key(state: AppState, key: &str) -> bool {
    if state.selected_select_tool.get_untracked() != SelectTool::Pen || pen_open(state).is_none() {
        return false;
    }
    match key {
        "Enter" => close_pen(state),
        "Backspace" | "Delete" => pen_undo_point(state),
        "Escape" => state.pen_path.set(None),
        _ => return false,
    }
    true
}

/// The path, its anchors and handles; an open path's first anchor is ringed
/// as the place that closes it.
pub(crate) fn draw_pen_path(ctx: &web_sys::CanvasRenderingContext2d, p: &PenPath, w: f64, h: f64, ui: f64) {
    ctx.save();
    ctx.set_stroke_style_str("#0a84ff");
    ctx.set_line_width(2.0 * ui);
    trace_bezier(ctx, &p.points, p.closed, w, h);
    ctx.stroke();
    if let (false, Some(first)) = (p.closed, p.points.first()) {
        ctx.set_stroke_style_str("#fff");
        ctx.begin_path();
        let _ = ctx.arc(first.x as f64 * w, first.y as f64 * h, 9.0 * ui, 0.0, std::f64::consts::TAU);
        ctx.stroke();
    }
    ctx.restore();
    draw_pen_handles(ctx, &p.points, w, h, ui);
}
