//! Hover and canvas loupes: pointer tracking, camera/canvas window decode, blitting and styling.

use crate::prelude::*;
use crate::*;

/// Normalized (0..1) cursor position within the thumb under the cursor.
/// Bubbling events are delegated in Leptos, so current_target is not the
/// thumb — resolve via target + closest instead.
/// Cursor position normalized inside the hovered ancestor matching `sel`.
/// (Leptos delegates bubbling events, so current_target is unusable.)
pub(crate) fn norm_pos_in(ev: &web_sys::MouseEvent, sel: &str) -> Option<(f64, f64)> {
    let el: web_sys::Element = ev.target()?.dyn_into().ok()?;
    let tile = el.closest(sel).ok().flatten()?;
    let r = tile.get_bounding_client_rect();
    Some((
        ((ev.client_x() as f64 - r.left()) / r.width()).clamp(0.0, 1.0),
        ((ev.client_y() as f64 - r.top()) / r.height()).clamp(0.0, 1.0),
    ))
}

pub(crate) fn thumb_norm_pos(ev: &web_sys::MouseEvent) -> Option<(f64, f64)> {
    norm_pos_in(ev, ".thumb")
}

/// CSS width of the hovered element matching `sel` — how large the image is
/// already shown at, which sets the magnification the loupe needs.
pub(crate) fn hovered_width(ev: &web_sys::MouseEvent, sel: &str) -> f64 {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| el.closest(sel).ok().flatten())
        .map(|el| el.get_bounding_client_rect().width())
        .unwrap_or(0.0)
}

/// Resting the cursor on a thumb opens the focus loupe after a short dwell,
/// so sweeping across the strip doesn't strobe full-res decodes.
pub(crate) fn loupe_soon(state: AppState, id: usize, ev: &web_sys::MouseEvent) {
    let Some((nx, ny)) = thumb_norm_pos(ev) else { return };
    let (cx, cy) = (ev.client_x() as f64, ev.client_y() as f64);
    state.loupe_gen.update(|g| *g += 1);
    let gen = state.loupe_gen.get_untracked();
    let cb = wasm_bindgen::closure::Closure::once(move || {
        if state.loupe_gen.get_untracked() == gen {
            ensure_proxy(state, id, false);
            state.loupe.set(Some((id, nx, ny, cx, cy)));
        }
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        250,
    );
    cb.forget();
}

pub(crate) fn loupe_move(state: AppState, id: usize, ev: &web_sys::MouseEvent) {
    let showing = state.loupe.with(|l| matches!(l, Some((lid, ..)) if *lid == id));
    if showing {
        let Some((nx, ny)) = thumb_norm_pos(ev) else { return };
        state
            .loupe
            .set(Some((id, nx, ny, ev.client_x() as f64, ev.client_y() as f64)));
    }
}

pub(crate) fn loupe_clear(state: AppState) {
    state.loupe_gen.update(|g| *g += 1);
    state.loupe.set(None);
}

// Scratch canvas for the loupe's pre-decode placeholder (native-res paints
// copy the 1:1 crop straight out of the cache instead, so hovering a 25MP
// photo never materializes a third full-size buffer). Keyed by item and a
// source version so a proxy upgrade rebuilds it.
thread_local! {
    pub(crate) static LOUPE_SRC: RefCell<Option<(usize, u64, web_sys::HtmlCanvasElement, u32, u32)>> =
        RefCell::new(None);
    /// Latest focus-loupe crop request; older worker replies are dropped.
    pub(crate) static FOCUS_LOUPE_REQ: Cell<u32> = const { Cell::new(0) };
}

/// Filmstrip focus loupe: a floating 1:1 pixel window into the hovered thumb,
/// for judging focus/sharpness without selecting each item.
#[component]
pub(crate) fn FocusLoupe(state: AppState) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let tag = create_rw_signal("");

    create_effect(move |_| {
        state.items.track(); // repaint when a full-res buffer lands
        let Some((id, nx, ny, _, _)) = state.loupe.get() else {
            LOUPE_SRC.with(|s| *s.borrow_mut() = None);
            return;
        };
        let Some(canvas) = canvas_ref.get() else { return };
        let dpr = web::window().device_pixel_ratio().max(1.0);
        let bw = (480.0 * dpr) as usize;
        let bh = (360.0 * dpr) as usize;
        if canvas.width() != bw as u32 || canvas.height() != bh as u32 {
            canvas.set_width(bw as u32);
            canvas.set_height(bh as u32);
        }
        let Some(photo) = get_photo(id) else { return };
        let proxy = proxy_data(id);
        // Native-resolution crop copied row-wise out of whichever buffer the
        // item has: sensor pixels when loaded, else the display proxy.
        let copy_crop = |px: &Pixels, w: usize, h: usize| -> bool {
            if w < bw || h < bh {
                return false;
            }
            let sx = ((nx * w as f64 - bw as f64 / 2.0) as isize)
                .clamp(0, (w - bw) as isize) as usize;
            let sy = ((ny * h as f64 - bh as f64 / 2.0) as isize)
                .clamp(0, (h - bh) as isize) as usize;
            let crop = px.crop_rows(w, sy, sx, bw, bh);
            web::put_pixels(&canvas, &crop, bw as u32, bh as u32);
            true
        };
        if photo.full {
            let (fw, fh) = photo.full_dims;
            if fw >= bw && fh >= bh {
                // The sensor buffer lives in the worker; ask it for just the
                // 1:1 window instead of keeping a main-thread copy.
                let sx = ((nx * fw as f64 - bw as f64 / 2.0) as isize)
                    .clamp(0, (fw - bw) as isize) as usize;
                let sy = ((ny * fh as f64 - bh as f64 / 2.0) as isize)
                    .clamp(0, (fh - bh) as isize) as usize;
                tag.set("1:1");
                FOCUS_LOUPE_REQ.with(|c| c.set(c.get() + 1));
                let rgen = FOCUS_LOUPE_REQ.with(|c| c.get());
                let ok = pworker::crop_window(id, sx, sy, bw, bh, move |reply| {
                    if FOCUS_LOUPE_REQ.with(|c| c.get()) != rgen {
                        return;
                    }
                    let Some(r) = reply else { return };
                    let Some(canvas) = canvas_ref.get() else { return };
                    web::put_pixels_view(&canvas, &r.pixels, r.w, r.h);
                });
                if ok {
                    return;
                }
            }
        } else if let Some(p) = &proxy {
            tag.set("1:1 proxy");
            if copy_crop(&p.pixels, p.w, p.h) {
                return;
            }
        } else {
            tag.set("preview…");
        }
        // Placeholder path (preview-res, or image smaller than the loupe):
        // paint from a small scratch canvas with scaling.
        let ver = photo.gen ^ (proxy_gen(id) << 32);
        let rebuild = LOUPE_SRC.with(|s| {
            !matches!(&*s.borrow(), Some((sid, sver, ..)) if *sid == id && *sver == ver)
        });
        if rebuild {
            let scratch = match &proxy {
                Some(p) => {
                    let c = web::create_canvas(p.w as u32, p.h as u32);
                    p.pixels.put_on_canvas(&c, p.w as u32, p.h as u32);
                    (c, p.w as u32, p.h as u32)
                }
                None => {
                    let (px, w, h) = &photo.preview;
                    if *w == 0 || *h == 0 {
                        return;
                    }
                    let c = web::create_canvas(*w as u32, *h as u32);
                    web::put_pixels(&c, px, *w as u32, *h as u32);
                    (c, *w as u32, *h as u32)
                }
            };
            LOUPE_SRC.with(|s| *s.borrow_mut() = Some((id, ver, scratch.0, scratch.1, scratch.2)));
        }
        LOUPE_SRC.with(|s| {
            let b = s.borrow();
            let Some((_, _, scratch, w, h)) = &*b else { return };
            let (w, h) = (*w as f64, *h as f64);
            let sw = (bw as f64).min(w);
            let sh = (bh as f64).min(h);
            let sx = (nx * w - sw / 2.0).clamp(0.0, (w - sw).max(0.0));
            let sy = (ny * h - sh / 2.0).clamp(0.0, (h - sh).max(0.0));
            let _ = web::ctx2d(&canvas)
                .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    scratch, sx, sy, sw, sh, 0.0, 0.0, bw as f64, bh as f64,
                );
        });
    });

    view! {
        <Show when=move || state.loupe.with(|l| l.is_some()) fallback=|| ()>
            <div
                class="loupe-hover"
                style=move || {
                    state
                        .loupe
                        .get()
                        .map(|(_, _, _, cx, cy)| loupe_style(cx, cy))
                        .unwrap_or_default()
                }
            >
                <canvas node_ref=canvas_ref width="480" height="360"></canvas>
                <span class="loupe-tag">{move || tag.get()}</span>
            </div>
        </Show>
    }
}

// Full-res *edited* render feeding the canvas loupe. Separate from HI_CACHE
// (display-sized) — the loupe wants native pixels, so its render targets the
// full working resolution (still HI_MAX_EDGE-capped) and runs in the worker.
thread_local! {
    pub(crate) static CANVAS_LOUPE_HI: RefCell<Option<HiCache>> = RefCell::new(None);
    pub(crate) static CANVAS_LOUPE_REQ: Cell<u32> = const { Cell::new(0) };
    /// Native-resolution camera JPEG for an item whose sensor pixels were not
    /// developed (lazy RAW). One entry — a 26MP frame is ~100MB of RGBA.
    pub(crate) static CAMERA_LOUPE: RefCell<Option<(usize, Rc<(Pixels, usize, usize)>)>> = RefCell::new(None);
    pub(crate) static CAMERA_LOUPE_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    /// Items with no usable camera JPEG (or no stored original) — don't
    /// re-read a 29MB blob on every repaint.
    pub(crate) static CAMERA_LOUPE_FAILED: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

pub(crate) fn camera_loupe_source(id: usize) -> Option<Rc<(Pixels, usize, usize)>> {
    CAMERA_LOUPE.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(cid, _)| *cid == id)
            .map(|(_, px)| px.clone())
    })
}

pub(crate) fn camera_loupe_drop(id: usize) {
    CAMERA_LOUPE.with(|c| {
        let hit = c.borrow().as_ref().map(|(cid, _)| *cid == id).unwrap_or(false);
        if hit {
            *c.borrow_mut() = None;
        }
    });
}

pub(crate) fn camera_loupe_clear(id: usize) {
    camera_loupe_drop(id);
    CAMERA_LOUPE_INFLIGHT.with(|s| {
        s.borrow_mut().remove(&id);
    });
    CAMERA_LOUPE_FAILED.with(|s| {
        s.borrow_mut().remove(&id);
    });
}

/// A destructive op baked new pixels for the item: its embedded JPEG no
/// longer describes what is on screen, so drop the decode and never serve
/// it again (the regenerated proxy covers the evicted-full case).
pub(crate) fn camera_loupe_invalidate(id: usize) {
    camera_loupe_drop(id);
    CAMERA_LOUPE_FAILED.with(|s| {
        s.borrow_mut().insert(id);
    });
}

/// Decode the camera's full-size embedded JPEG for the loupe. Import defers
/// the sensor develop, so without this the loupe would upscale the 3200px
/// display proxy. Reads the original from IndexedDB, keeps only the decoded
/// pixels, and lets the effect repaint when they land.
pub(crate) fn request_camera_loupe(state: AppState, id: usize) {
    if camera_loupe_source(id).is_some() {
        return;
    }
    if CAMERA_LOUPE_FAILED.with(|s| s.borrow().contains(&id)) {
        return;
    }
    // A sensor develop is running; its pixels are about to supersede the
    // camera JPEG, and adding ~100MB alongside it buys nothing.
    if FULL_INFLIGHT.with(|f| f.borrow().contains(&id)) {
        return;
    }
    if !CAMERA_LOUPE_INFLIGHT.with(|s| s.borrow_mut().insert(id)) {
        return;
    }
    spawn_local(async move {
        let decoded = async {
            let blob = session::load_raw_blob(id).await?;
            let meta = raw_meta_async(blob.as_ref(), 0).await?;
            let jpeg = meta.camera_jpeg?;
            // Below ~3k wide the largest JPEG is still a preview, not sensor
            // resolution.
            if meta.camera_w < 3000 {
                return None;
            }
            let (px, w, h) = camera_jpeg_rgba(&jpeg).await?;
            let (px, w, h) = raw::apply_orientation_code(px, w, h, meta.orientation);
            Some(Rc::new((Pixels::owned(px), w, h)))
        }
        .await;
        CAMERA_LOUPE_INFLIGHT.with(|s| {
            s.borrow_mut().remove(&id);
        });
        if let Some(px) = decoded {
            // The sensor develop landed while we were decoding; the loupe
            // reads those pixels now, so don't hold a second full frame.
            if !full_present(id) {
                CAMERA_LOUPE.with(|c| *c.borrow_mut() = Some((id, px)));
                state.items.update(|_| {});
            }
        } else {
            CAMERA_LOUPE_FAILED.with(|s| {
                s.borrow_mut().insert(id);
            });
        }
    });
}

/// Normalized working-image coords of a pointer over the editor canvas. The
/// wrap shows the crop region outside the Crop tab, so map through the crop.
pub(crate) fn canvas_loupe_norm(state: AppState, tab: Tab, ev: &web_sys::PointerEvent) -> Option<(f64, f64)> {
    let el: web_sys::Element = ev.target()?.dyn_into().ok()?;
    let wrap = el.closest(".canvas-wrap").ok().flatten()?;
    let r = wrap.get_bounding_client_rect();
    if r.width() < 1.0 || r.height() < 1.0 {
        return None;
    }
    let nx = ((ev.client_x() as f64 - r.left()) / r.width()).clamp(0.0, 1.0);
    let ny = ((ev.client_y() as f64 - r.top()) / r.height()).clamp(0.0, 1.0);
    let item = state.current()?;
    let crop = item.edit.crop;
    if tab != Tab::Crop && !crop_is_full(&crop) {
        Some((
            crop.x as f64 + nx * crop.w as f64,
            crop.y as f64 + ny * crop.h as f64,
        ))
    } else {
        Some((nx, ny))
    }
}

/// Resting the mouse on the editor canvas opens the 1:1 loupe after a dwell.
/// Re-armed on every move while hidden, so sweeps across the canvas never
/// strobe full-res renders — the loupe only opens once the cursor stops.
pub(crate) fn canvas_loupe_soon(state: AppState, tab: RwSignal<Tab>, ev: &web_sys::PointerEvent) {
    if ev.pointer_type() != "mouse" {
        return;
    }
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some((nx, ny)) = canvas_loupe_norm(state, tab.get_untracked(), ev) else { return };
    let (cx, cy) = (ev.client_x() as f64, ev.client_y() as f64);
    let id = item.id;
    state.canvas_loupe_gen.update(|g| *g += 1);
    let gen = state.canvas_loupe_gen.get_untracked();
    let cb = Closure::once(move || {
        if state.canvas_loupe_gen.get_untracked() == gen {
            ensure_proxy(state, id, false);
            state.canvas_loupe.set(Some((id, nx, ny, cx, cy)));
        }
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        250,
    );
    cb.forget();
}

pub(crate) fn canvas_loupe_move(state: AppState, tab: RwSignal<Tab>, ev: &web_sys::PointerEvent) {
    // Drags (heal/clone/crop/select/layers) never open the loupe.
    if ev.buttons() != 0 {
        canvas_loupe_clear(state);
        return;
    }
    if state.canvas_loupe.with(|l| l.is_some()) {
        let Some(item) = state.current() else { return };
        if let Some((nx, ny)) = canvas_loupe_norm(state, tab.get_untracked(), ev) {
            state.canvas_loupe.set(Some((
                item.id,
                nx,
                ny,
                ev.client_x() as f64,
                ev.client_y() as f64,
            )));
        }
    } else {
        canvas_loupe_soon(state, tab, ev);
    }
}

pub(crate) fn canvas_loupe_clear(state: AppState) {
    state.canvas_loupe_gen.update(|g| *g += 1);
    state.canvas_loupe.set(None);
}

/// Crop a hover window out of the sensor buffer and run the pointwise edit
/// pipeline over it at source density (raw develop → blur → levels → curves →
/// adjust → grain), mirroring render_pipeline. Local ops get a margin so the
/// cropped window's edges do not darken against the border.
pub(crate) fn source_window(
    px: &Pixels,
    fw: usize,
    fh: usize,
    sx: usize,
    sy: usize,
    cw: usize,
    ch: usize,
    edit: &EditParams,
    seed: u32,
) -> Option<Vec<u8>> {
    if cw == 0 || ch == 0 || sx + cw > fw || sy + ch > fh {
        return None;
    }
    let margin = (edit.blur / 100.0 * 8.0 * 3.0).ceil() as usize + 2;
    let x0 = sx.saturating_sub(margin);
    let y0 = sy.saturating_sub(margin);
    let x1 = (sx + cw + margin).min(fw);
    let y1 = (sy + ch + margin).min(fh);
    let (ew, eh) = (x1 - x0, y1 - y0);
    let win = px.crop_rows(fw, y0, x0, ew, eh);
    let mut frame = PipeFrame::new(win, ew, eh);
    // The loupe shows the whole window, not the selection. Layer masks are
    // full-frame, so crop them to the window or they would paint the whole crop.
    let rebased = pipeline::rebase_layer_masks(edit, fw, fh, (x0, y0, ew, eh), ew, eh);
    pipeline::finish(&mut frame, &rebased, seed, None, 1.0);
    let win = frame.pixels;
    let (ix, iy) = (sx - x0, sy - y0);
    let mut out = Vec::with_capacity(cw * ch * 4);
    for row in 0..ch {
        let start = ((iy + row) * ew + ix) * 4;
        out.extend_from_slice(&win[start..start + cw * 4]);
    }
    Some(out)
}

/// Draw an RGBA buffer into the loupe backing, scaled to fill it.
pub(crate) fn blit_window(canvas: &web_sys::HtmlCanvasElement, px: &[u8], w: u32, h: u32, bw: u32, bh: u32) {
    let scratch = web::create_canvas(w, h);
    web::put_pixels(&scratch, px, w, h);
    blit_scratch(canvas, &scratch, w, h, bw, bh);
}

/// [`blit_window`] for a worker reply: the pixels are already a JS view.
pub(crate) fn blit_window_view(
    canvas: &web_sys::HtmlCanvasElement,
    px: &Uint8Array,
    w: u32,
    h: u32,
    bw: u32,
    bh: u32,
) {
    let scratch = web::create_canvas(w, h);
    web::put_pixels_view(&scratch, px, w, h);
    blit_scratch(canvas, &scratch, w, h, bw, bh);
}

pub(crate) fn blit_scratch(
    canvas: &web_sys::HtmlCanvasElement,
    scratch: &web_sys::HtmlCanvasElement,
    w: u32,
    h: u32,
    bw: u32,
    bh: u32,
) {
    let ctx = web::ctx2d(canvas);
    ctx.set_image_smoothing_enabled(true);
    high_quality(&ctx);
    let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
        scratch,
        0.0,
        0.0,
        w as f64,
        h as f64,
        0.0,
        0.0,
        bw as f64,
        bh as f64,
    );
}

/// Editor-canvas hover loupe: a floating 1:1 window into the *edited* image —
/// a full-res render through the whole pipeline, so it shows what an export
/// would. Untouched photos take the cheap source-pixel path instead.
#[component]
pub(crate) fn CanvasLoupe(
    state: AppState,
    tab: RwSignal<Tab>,
    main_canvas: NodeRef<html::Canvas>,
) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let tag = create_rw_signal(String::new());

    // Clear on any press inside the wrap. Capture phase: overlay children
    // (crop rect, selected text) stop propagation, so the wrap's delegated
    // on:pointerdown never sees those presses.
    let pd = Closure::wrap(Box::new(move |ev: web_sys::PointerEvent| {
        let hit = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest(".canvas-wrap").ok().flatten())
            .is_some();
        if hit {
            canvas_loupe_clear(state);
        }
    }) as Box<dyn FnMut(_)>);
    let pd_opts = web_sys::AddEventListenerOptions::new();
    pd_opts.set_capture(true);
    web::window()
        .add_event_listener_with_callback_and_add_event_listener_options(
            "pointerdown",
            pd.as_ref().unchecked_ref(),
            &pd_opts,
        )
        .ok();
    on_cleanup(move || {
        web::window()
            .remove_event_listener_with_callback_and_bool(
                "pointerdown",
                pd.as_ref().unchecked_ref(),
                true,
            )
            .ok();
    });

    create_effect(move |_| {
        state.items.track(); // edits, full-res decodes, render landings
        state.selected.track();
        let Some((id, nx, ny, _, _)) = state.canvas_loupe.get() else { return };
        let Some(item) = state.current() else {
            canvas_loupe_clear(state);
            return;
        };
        if item.id != id || item.kind != MediaKind::Photo {
            canvas_loupe_clear(state);
            return;
        }
        let Some(photo) = get_photo(id) else { return };
        let Some(canvas) = canvas_ref.get() else { return };
        let dpr = web::window().device_pixel_ratio().max(1.0);
        let bw = (480.0 * dpr) as usize;
        let bh = (360.0 * dpr) as usize;
        if canvas.width() != bw as u32 || canvas.height() != bh as u32 {
            canvas.set_width(bw as u32);
            canvas.set_height(bh as u32);
        }

        // Window size relative to what the screen already shows: magnify ~4x
        // over the main view, never downsampling below 1 device px per source
        // px. A fixed-size window stops being a closeup once the view is a
        // tight crop — it would frame about the same region as the canvas.
        let (ww, wh) = working_dims(photo.full_dims.0, photo.full_dims.1, &item.edit);
        let cropped_view = tab.get() != Tab::Crop && !crop_is_full(&item.edit.crop);
        let crop = item.edit.crop;
        let (vx, vy, vw, vh) = if cropped_view {
            (
                crop.x as f64 * ww as f64,
                crop.y as f64 * wh as f64,
                (crop.w as f64 * ww as f64).max(1.0),
                (crop.h as f64 * wh as f64).max(1.0),
            )
        } else {
            (0.0, 0.0, ww as f64, wh as f64)
        };
        let disp_css_w = main_canvas
            .get()
            .map(|c| c.client_width() as f64)
            .filter(|w| *w > 1.0)
            .unwrap_or(480.0);
        let zoom = (4.0 * disp_css_w / vw).max(1.0 / dpr).min(4.0);
        let cw = ((bw as f64 / (dpr * zoom)).round() as usize).max(1).min(ww);
        let ch = ((bh as f64 / (dpr * zoom)).round() as usize).max(1).min(wh);
        let mag = (dpr * zoom).round().max(1.0) as u32;
        let sx = ((nx * ww as f64 - cw as f64 / 2.0) as isize)
            .clamp(0, (ww - cw) as isize) as usize;
        let sy = ((ny * wh as f64 - ch as f64 / 2.0) as isize)
            .clamp(0, (wh - ch) as isize) as usize;

        // Direct path: crop the window straight out of the sensor buffer and
        // run the pointwise pipeline over it, so the loupe shows true source
        // pixels instead of a slice of a whole-image render. Exact only with
        // identity geometry and no selection or visible overlay layers;
        // adjustment layers ride the pointwise pipeline and rebase their masks.
        let loupe_edit = state::render_edit(&item.edit, &item.layers);
        let plain = item.edit.rot90.is_multiple_of(4)
            && item.edit.fine_angle.abs() <= 0.01
            && item.edit.selection.is_none()
            && !item.layers.iter().any(|l| {
                l.visible && l.opacity > 0.01 && !matches!(l.kind, LayerKind::Adjust(_))
            });
        if plain {
            if photo.full {
                let (fw, fh) = photo.full_dims;
                if fw >= cw && fh >= ch {
                    // Source pixels live in the worker; it crops the window,
                    // runs the pointwise pipeline and returns only the loupe.
                    let recipe = pworker::render_recipe(&item, id);
                    CANVAS_LOUPE_REQ.with(|c| c.set(c.get() + 1));
                    let rgen = CANVAS_LOUPE_REQ.with(|c| c.get());
                    let tag_cb = tag;
                    let ok = pworker::loupe_window(
                        id,
                        &recipe,
                        sx,
                        sy,
                        cw,
                        ch,
                        id as u32,
                        move |reply| {
                            if CANVAS_LOUPE_REQ.with(|c| c.get()) != rgen {
                                return;
                            }
                            let Some(r) = reply else { return };
                            let Some(canvas) = canvas_ref.get() else { return };
                            blit_window_view(
                                &canvas, &r.pixels, r.w, r.h, bw as u32, bh as u32,
                            );
                            tag_cb.set(format!("{mag}:1"));
                        },
                    );
                    if ok {
                        return;
                    }
                }
            }
            if photo.raw {
                // The sensor develop is deferred on import, so without this
                // the loupe would upscale the 3200px display proxy. The
                // camera's own full-size JPEG is one decode away and covers
                // the same frame.
                request_camera_loupe(state, id);
                if let Some(cam) = camera_loupe_source(id) {
                    let (kx, ky) = (cam.1 as f64 / ww as f64, cam.2 as f64 / wh as f64);
                    let px = ((sx as f64 * kx).round() as usize).min(cam.1.saturating_sub(1));
                    let py = ((sy as f64 * ky).round() as usize).min(cam.2.saturating_sub(1));
                    let pw = ((cw as f64 * kx).round() as usize).max(1).min(cam.1 - px);
                    let phh = ((ch as f64 * ky).round() as usize).max(1).min(cam.2 - py);
                    if let Some(win) =
                        source_window(&cam.0, cam.1, cam.2, px, py, pw, phh, &loupe_edit, id as u32)
                    {
                        blit_window(&canvas, &win, pw as u32, phh as u32, bw as u32, bh as u32);
                        tag.set(format!("{mag}:1 camera"));
                        return;
                    }
                }
            }
            if let Some(p) = proxy_data(id) {
                // The proxy covers the same frame at reduced density: map the
                // window into proxy pixels and let the blit upscale it.
                let (kx, ky) = (p.w as f64 / ww as f64, p.h as f64 / wh as f64);
                let px = ((sx as f64 * kx).round() as usize).min(p.w.saturating_sub(1));
                let py = ((sy as f64 * ky).round() as usize).min(p.h.saturating_sub(1));
                let pw = ((cw as f64 * kx).round() as usize).max(1).min(p.w - px);
                let phh = ((ch as f64 * ky).round() as usize).max(1).min(p.h - py);
                if let Some(win) = source_window(
                    &p.pixels, p.w, p.h, px, py, pw, phh, &loupe_edit, id as u32,
                ) {
                    blit_window(&canvas, &win, pw as u32, phh as u32, bw as u32, bh as u32);
                    tag.set(format!("{mag}:1 proxy"));
                    return;
                }
            }
        }

        let edit_fp = edit_fingerprint(&item);
        let source_fp = source_fingerprint(photo.gen, proxy_gen(id));
        let hit = CANVAS_LOUPE_HI.with(|c| {
            let b = c.borrow();
            let hi = b.as_ref()?;
            if hi.item_id != id || hi.edit_fp != edit_fp || hi.source_fp != source_fp {
                return None;
            }
            hi.canvas.clone().map(|canvas| (canvas, hi.w, hi.h))
        });
        if let Some((hi_canvas, hw, hh)) = hit {
            // The cached render covers the whole working frame; map the
            // window through its scale, then magnify it into the loupe.
            let kx = hw as f64 / ww as f64;
            let ky = hh as f64 / wh as f64;
            let ctx = web::ctx2d(&canvas);
            ctx.set_image_smoothing_enabled(true);
            let _ = ctx
                .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    &hi_canvas,
                    sx as f64 * kx,
                    sy as f64 * ky,
                    cw as f64 * kx,
                    ch as f64 * ky,
                    0.0,
                    0.0,
                    bw as f64,
                    bh as f64,
                );
            // Whole-image renders are capped, so past that cap this is not
            // sensor 1:1 — say so rather than overstate the detail.
            let full_density = hw >= ww && hh >= wh;
            tag.set(if full_density {
                format!("{mag}:1")
            } else {
                format!("{mag}:1 preview")
            });
            return;
        }

        // Placeholder until the full decode or the rendered window lands:
        // show the same framing, magnified out of the on-screen canvas. Its
        // backing shows the visible region (the crop, outside the Crop tab).
        tag.set(String::from("rendering…"));
        if let Some(main) = main_canvas.get() {
            let (mw, mh) = (main.width() as f64, main.height() as f64);
            if mw > 1.0 && mh > 1.0 {
                let kx = mw / vw;
                let ky = mh / vh;
                let dx = (sx as f64 - vx) * kx;
                let dy = (sy as f64 - vy) * ky;
                let x0 = dx.max(0.0).min(mw);
                let y0 = dy.max(0.0).min(mh);
                let x1 = (dx + cw as f64 * kx).max(0.0).min(mw);
                let y1 = (dy + ch as f64 * ky).max(0.0).min(mh);
                if x1 > x0 && y1 > y0 {
                    let ctx = web::ctx2d(&canvas);
                    ctx.set_image_smoothing_enabled(true);
                    let _ = ctx
                        .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                            &main, x0, y0, x1 - x0, y1 - y0, 0.0, 0.0, bw as f64, bh as f64,
                        );
                }
            }
        }

        // Kick the full-res edited render. Every kick invalidates the one
        // before it; the store nudges items, re-running this effect.
        if !full_present(id) && !proxy_in_mem(id) {
            ensure_proxy(state, id, true);
            return;
        }
        CANVAS_LOUPE_REQ.with(|c| c.set(c.get() + 1));
        let rgen = CANVAS_LOUPE_REQ.with(|c| c.get());
        // Size from the source actually being rendered — a proxy render must
        // not be asked for sensor dimensions it can never produce.
        let (src_id, (sw, sh)) = render_source(id, &photo);
        let (fw, fh) = working_dims(sw, sh, &item.edit);
        let recipe = pworker::render_recipe(&item, src_id);
        let sel = item.edit.selection.clone();
        let item_cb = item.clone();
        let accepted = pworker::render(
            src_id,
            &recipe,
            fw as f64,
            fh as f64,
            HI_MAX_EDGE as u32,
            sel.as_ref(),
            move |reply| {
                if CANVAS_LOUPE_REQ.with(|c| c.get()) != rgen {
                    return;
                }
                if let Ok(r) = reply {
                    let hi_canvas = web::create_canvas(r.w, r.h);
                    web::put_pixels_view(&hi_canvas, &r.pixels, r.w, r.h);
                    composite_stack(&hi_canvas, id, &item_cb.layers, r.w as usize, r.h as usize);
                    CANVAS_LOUPE_HI.with(|c| {
                        *c.borrow_mut() = Some(HiCache {
                            item_id: id,
                            edit_fp,
                            source_fp,
                            canvas: Some(hi_canvas),
                            bitmap: None,
                            w: r.w as usize,
                            h: r.h as usize,
                            prefetched: false,
                            stand_in: false,
                        })
                    });
                    state.items.update(|_| {});
                }
            },
        );
        let _ = accepted;
    });

    view! {
        <Show when=move || state.canvas_loupe.with(|l| l.is_some()) fallback=|| ()>
            <div
                class="loupe-hover"
                style=move || {
                    state
                        .canvas_loupe
                        .get()
                        .map(|(_, _, _, cx, cy)| loupe_style(cx, cy))
                        .unwrap_or_default()
                }
            >
                <canvas node_ref=canvas_ref width="480" height="360"></canvas>
                <span class="loupe-tag">{move || tag.get()}</span>
            </div>
        </Show>
    }
}

/// Fixed-position style for the loupe panel, kept inside the viewport.
pub(crate) fn loupe_style(x: f64, y: f64) -> String {
    let w = 496.0_f64;
    let h = 398.0_f64;
    let vw = web::window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1280.0);
    let vh = web::window().inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
    let left = if x + 24.0 + w > vw { (x - w - 14.0).max(8.0) } else { x + 24.0 };
    let top = (y - h / 2.0).max(8.0).min((vh - h - 8.0).max(8.0));
    format!("left:{left:.0}px;top:{top:.0}px")
}
