//! Main canvas preview: render scheduling, masks, overlays, crop/select interaction and video text overlays.

use crate::prelude::*;
use crate::*;

#[component]
pub(crate) fn Preview(state: AppState, tab: RwSignal<Tab>) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let working = create_rw_signal((1usize, 1usize));
    let layer_drag = create_rw_signal(None::<(usize, f32, f32, f32, f32)>);
    let pen_drag = create_rw_signal(None::<(usize, PenDrag, f32, f32)>);
    let brush_draw = create_rw_signal(None::<usize>);
    let select_drag = create_rw_signal(None::<SelectDrag>);
    let heal_drag = create_rw_signal(None::<Vec<(f32, f32)>>);
    let clone_drag = create_rw_signal(None::<Vec<(f32, f32)>>);
    // Hoisted out of CropOverlay: crop edits re-render the preview, which
    // remounts the overlay — a drag signal created inside it would be
    // disposed mid-gesture, killing the drag after one pointermove.
    let crop_drag = create_rw_signal(None::<(u8, f32, f32, state::CropRect)>);
    // Pinch-zoom state for the crop tab: live pointer positions plus the
    // crop rect / centroid / spread captured when the second finger lands.
    let crop_pinch = create_rw_signal(
        None::<(
            Vec<(i32, f32, f32)>,
            Option<(state::CropRect, (f32, f32), f32)>,
        )>,
    );

    // Window resizes change the canvas display size; bump a signal so the
    // render effect re-checks whether the hi-res backing is still sufficient.
    let rsz = window_event_listener(leptos::ev::resize, move |_| {
        state.viewport_gen.update(|g| *g += 1);
    });
    on_cleanup(move || rsz.remove());

    // The reload prefetch (index.html) decodes the selected item's stored
    // display proxy while the wasm boots. When it lands after the restored
    // session has painted, install and blit it here — waiting for the next
    // edit-driven pass would leave the soft tier up for no reason. If the
    // item or canvas isn't ready, nudge the render effect instead; it pulls
    // the same bitmap on its first preview-less pass.
    {
        let cb = Closure::<dyn FnMut()>::new(move || {
            let mut painted = false;
            if let Some(item) = state.current() {
                if item.kind == MediaKind::Photo {
                    if let Some(photo) = get_photo(item.id) {
                        painted = paint_prefetched_hi(&item, &photo, canvas_ref.get().as_deref());
                    }
                }
            }
            if !painted {
                state.hi_built.update(|g| *g += 1);
            }
        });
        let w = web::window();
        let _ = w.add_event_listener_with_callback("pes-proxy-bmp", cb.as_ref().unchecked_ref());
        on_cleanup(move || {
            let _ = web::window().remove_event_listener_with_callback(
                "pes-proxy-bmp",
                cb.as_ref().unchecked_ref(),
            );
        });
    }

    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some(mut pts)) = clone_drag.try_get() else { return };
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
        if pts.last().map(|last| dist2(*last, (nx, ny)) > 0.00005).unwrap_or(true) {
            pts.push((nx, ny));
            clone_drag.set(Some(pts));
        }
    });

    window_event_listener(leptos::ev::pointerup, move |_| {
        let Some(pts) = clone_drag.try_get().flatten() else { return };
        let _ = clone_drag.try_set(None);
        apply_clone(state, &pts);
    });

    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some(mut pts)) = heal_drag.try_get() else { return };
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
        if pts.last().map(|last| dist2(*last, (nx, ny)) > 0.00005).unwrap_or(true) {
            pts.push((nx, ny));
            heal_drag.set(Some(pts));
        }
    });

    window_event_listener(leptos::ev::pointerup, move |_| {
        let Some(pts) = heal_drag.try_get().flatten() else { return };
        let _ = heal_drag.try_set(None);
        apply_heal(state, &pts);
    });

    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some((id, nx0, ny0, x0, y0))) = layer_drag.try_get() else { return };
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
        let dx = nx - nx0;
        let dy = ny - ny0;
        state.update_current_item(|m| {
            if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                match &mut l.kind {
                    LayerKind::Text(t) => {
                        t.x = (x0 + dx).clamp(0.0, 1.0);
                        t.y = (y0 + dy).clamp(0.0, 1.0);
                    }
                    LayerKind::Raster(r) => {
                        r.x = (x0 + dx).clamp(0.0, 1.0);
                        r.y = (y0 + dy).clamp(0.0, 1.0);
                    }
                    _ => {}
                }
            }
        });
    });

    window_event_listener(leptos::ev::pointerup, move |_| {
        let _ = layer_drag.try_set(None);
    });

    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some((id, drag, nx0, ny0))) = pen_drag.try_get() else { return };
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
        let dx = nx - nx0;
        let dy = ny - ny0;
        state.update_current_item(|m| {
            let Some(l) = m.layers.iter_mut().find(|l| l.id == id) else { return };
            let LayerKind::Path(p) = &mut l.kind else { return };
            match drag {
                PenDrag::MovePoint(i) => {
                    let Some(pt) = p.points.get_mut(i) else { return };
                    let new_x = (pt.x + dx).clamp(0.0, 1.0);
                    let new_y = (pt.y + dy).clamp(0.0, 1.0);
                    pt.in_x -= new_x - pt.x;
                    pt.in_y -= new_y - pt.y;
                    pt.out_x -= new_x - pt.x;
                    pt.out_y -= new_y - pt.y;
                    pt.x = new_x;
                    pt.y = new_y;
                }
                PenDrag::MoveOut(i) => {
                    let Some(pt) = p.points.get_mut(i) else { return };
                    pt.out_x = (pt.out_x + dx).clamp(-1.0, 1.0);
                    pt.out_y = (pt.out_y + dy).clamp(-1.0, 1.0);
                    if pt.smooth {
                        pt.in_x = -pt.out_x;
                        pt.in_y = -pt.out_y;
                    }
                }
                PenDrag::MoveIn(i) => {
                    let Some(pt) = p.points.get_mut(i) else { return };
                    pt.in_x = (pt.in_x + dx).clamp(-1.0, 1.0);
                    pt.in_y = (pt.in_y + dy).clamp(-1.0, 1.0);
                    if pt.smooth {
                        pt.out_x = -pt.in_x;
                        pt.out_y = -pt.in_y;
                    }
                }
            }
        });
    });

    window_event_listener(leptos::ev::pointerup, move |_| {
        let _ = pen_drag.try_set(None);
    });

    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(id) = brush_draw.try_get().flatten() else { return };
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
        state.update_current_item(|m| {
            let Some(l) = m.layers.iter_mut().find(|l| l.id == id) else { return };
            let LayerKind::Brush(b) = &mut l.kind else { return };
            if let Some(stroke) = b.strokes.last_mut() {
                stroke.points.push((nx, ny));
            }
        });
    });

    window_event_listener(leptos::ev::pointerup, move |_| {
        let _ = brush_draw.try_set(None);
    });

    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some(drag)) = select_drag.try_get() else { return };
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
        match drag {
            SelectDrag::Rect(start, _) => {
                select_drag.set(Some(SelectDrag::Rect(start, (nx, ny))));
            }
            SelectDrag::Lasso(mut pts) => {
                if pts.last().map(|last| dist2(*last, (nx, ny)) > 0.00005).unwrap_or(true) {
                    pts.push((nx, ny));
                    select_drag.set(Some(SelectDrag::Lasso(pts)));
                }
            }
        }
    });

    window_event_listener(leptos::ev::pointerup, move |_| {
        let Some(drag) = select_drag.try_get().flatten() else { return };
        let new_sel = match drag {
            SelectDrag::Rect((x0, y0), (x1, y1)) => {
                let x = x0.min(x1);
                let y = y0.min(y1);
                let w = (x1 - x0).abs();
                let h = (y1 - y0).abs();
                if w > 0.01 && h > 0.01 {
                    Some(Selection {
                        kind: SelectionKind::Rect { x, y, w, h },
                        feather: 0.0,
                    })
                } else {
                    None
                }
            }
            SelectDrag::Lasso(pts) => {
                if pts.len() >= 3 {
                    Some(Selection {
                        kind: SelectionKind::Lasso(pts),
                        feather: 0.0,
                    })
                } else {
                    None
                }
            }
        };
        state.update_current_item(|m| {
            m.edit.selection = new_sel;
        });
        let _ = select_drag.try_set(None);
    });

    create_effect(move |_| {
        perf::record("preview.effect", 0.0);
        state.items.track();
        state.selected.track();
        state.selected_layer.track();
        state.selected_tool.track();
        state.selected_select_tool.track();
        state.hi_built.track();
        state.viewport_gen.track();
        heal_drag.track();
        state.heal_radius.track();
        clone_drag.track();
        state.clone_source.track();
        state.clone_offset.track();
        state.clone_radius.track();
        tab.track();
        select_drag.track();
        // Paint on the leading edge so the change that opened this frame shows
        // in it; a burst of further changes this frame coalesces into one
        // trailing rAF pass, so there is still at most one extra paint/frame.
        let render: Rc<dyn Fn()> = Rc::new(move || {
            let Some(item) = state.current() else { return };
            if item.kind != MediaKind::Photo {
                return;
            }
            let Some(photo) = get_photo(item.id) else { return };
            let Some(canvas) = canvas_ref.get() else { return };
            let mut render_span = perf::Span::new("preview.render");

        // A lazily restored item has no preview until the selection effect's
        // decode lands; skip this pass rather than render a 0×0 buffer. The
        // reload prefetch may already hold the stored display proxy, though —
        // paint it now so a restored session reaches its sharp frame one pass
        // earlier (the proxy decode is the slow half of a restore).
        if photo.preview.1 == 0 || photo.preview.2 == 0 {
            ensure_preview(state, item.id);
            paint_prefetched_hi(&item, &photo, Some(&canvas));
            return;
        }
        let (pix, w, h) = &photo.preview;
        let geo = preview_geometry_cached(item.id, photo.gen, &photo.preview, &item.edit);
        let (p, w, h) = match &geo {
            Some((px, gw, gh)) => (Cow::Borrowed(px.as_slice()), *gw, *gh),
            None => geometry(pix, *w, *h, &item.edit),
        };
        working.set((w, h));
        let crop = item.edit.crop;
        let cropped_view = tab.get() != Tab::Crop && !crop_is_full(&crop);

        // Is the 1024px preview finer than what the screen is asking for?
        // During reactive remounts this effect fires while the fresh canvas
        // is built but not yet inserted — client size is 0 then, so fall
        // back to the last real measurement for the hi-res decision.
        let dpr = web::window().device_pixel_ratio();
        let need_w = canvas.client_width() as f64 * dpr;
        let need_h = canvas.client_height() as f64 * dpr;
        let (need_w, need_h) = if need_w < 1.0 || need_h < 1.0 {
            let last = LAST_NEED.with(|n| n.get());
            if last.0 >= 1.0 && last.1 >= 1.0 {
                last
            } else {
                // First paint of a restored session: the canvas is built
                // before its first layout, so it has no client size and
                // LAST_NEED is still empty. Use the viewport as an upper
                // bound — it only decides hi vs preview and sizes the
                // request, which is capped at HI_MAX_EDGE anyway.
                let win = web::window();
                (
                    win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(0.0) * dpr,
                    win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(0.0) * dpr,
                )
            }
        } else {
            LAST_NEED.with(|n| n.set((need_w, need_h)));
            (need_w, need_h)
        };
        let want_hi = need_w > w as f64 * 1.05 || need_h > h as f64 * 1.05;

        // A restored edit can carry a RAW develop its stored proxy never saw;
        // ask for the sensor re-develop so the neutral pipeline can run.
        if want_hi && photo.raw && !full_present(item.id) && !item.edit.raw.is_identity() {
            ensure_proxy(state, item.id, true);
        }

        // Total-image render size needed for the cache: when the view is
        // cropped, the crop region alone must cover the display, so the
        // whole image must be proportionally larger.
        let (req_w, req_h) = if cropped_view {
            (
                need_w / crop.w.max(0.01) as f64,
                need_h / crop.h.max(0.01) as f64,
            )
        } else {
            (need_w, need_h)
        };
        // Exact size the render will produce for this request, sized from the
        // source actually loaded (sensor pixels or the display proxy) — the
        // cache is sufficient iff it matches, which keeps rebuilds from looping.
        let (src_id, (sw, sh)) = render_source(item.id, &photo);
        let (fw, fh) = working_dims(sw, sh, &item.edit);
        let target = hi_target(req_w, req_h, fw, fh, HI_MAX_EDGE as u32);

        // Sharp backing for the canvas. The cropped view prefers a crop-window
        // render (native density up to WINDOW_MAX_EDGE); the whole-frame hi
        // cache still serves the full view and backs the cropped one when a
        // window is not available (worker down → sync fallback).
        let edit_fp = edit_fingerprint(&item);
        let src_fp = source_fingerprint(photo.gen, proxy_gen(item.id));
        let (cx_px, cy_px, cw_p, _) = crop_px(&item.edit, w, h);
        let (cx_p, cy_p) = (cx_px as f64, cy_px as f64);
        let mut sharp: Option<Sharp> = None;
        // A sharp frame from an older source stays on screen while the new
        // one renders, so a RAW proxy upgrade never flashes the soft preview.
        let mut stale_source = false;
        // Source fingerprint of the sharp frame this pass draws, if any.
        let mut drawn_src: Option<u64> = None;
        // GPU parametric preview: one shader draw per paint from the worker
        // base, which is pre-develop and pre-colour, so slider ticks never
        // touch the worker. With no usable base the 1024px preview stays up
        // until one lands; the CPU hi-res path below is then not requested.
        // A RAW with develop edits stays on the GPU only with a developed base
        // (neutral develop + camera tone + detail rendered by the worker), so
        // the shader never approximates the develop; without the neutral frame
        // the CPU neutral path renders it.
        let mut gpu_ok = want_hi
            && !cropped_view
            && gpu::eligible(&item.edit, &item.layers)
            && gpu::active()
            && !(photo.raw && !item.edit.raw.is_identity() && !source_has_neutral(item.id, &photo));
        let mut gkey: Option<gpu::BaseKey> = None;
        if gpu_ok {
            let key = gpu::BaseKey {
                item: item.id as u32,
                src_id: src_id as u32,
                src_fp,
                rot90: item.edit.rot90,
                fine_bits: item.edit.fine_angle.to_bits(),
                need_w: target.0 as u32,
                need_h: target.1 as u32,
                raw_fp: gpu::raw_fingerprint(&item.edit.raw),
            };
            gkey = Some(key);
            if gpu::base_match(&key).is_some() {
                match gpu::render(&item.edit, item.id as u32) {
                    Some((gcanvas, gw, gh)) => {
                        if canvas.width() != gw || canvas.height() != gh {
                            canvas.set_width(gw);
                            canvas.set_height(gh);
                        }
                        let blit_span = perf::Span::new("gpu.blit");
                        let blitted = web::ctx2d(&canvas)
                            .draw_image_with_html_canvas_element(&gcanvas, 0.0, 0.0)
                            .is_ok();
                        drop(blit_span);
                        if blitted {
                            composite_layers(&canvas, &item.layers, gw as usize, gh as usize);
                            sharp = Some(Sharp::Whole { w: gw as usize });
                            drawn_src = gpu::base_src().map(|(_, fp)| fp);
                        } else {
                            gpu::latch_off();
                            gpu_ok = false;
                        }
                    }
                    None => gpu_ok = false,
                }
            }
        }
        if want_hi {
            if cropped_view {
                let (_, _, fcw, fch) = crop_px(&item.edit, fw, fh);
                let win_target = hi_target(need_w, need_h, fcw, fch, WINDOW_MAX_EDGE as u32);
                let key = crop_key(&crop);
                sharp = HI_WINDOW.with(|c| {
                    let b = c.borrow();
                    let win = b.as_ref()?;
                    if win.item_id != item.id || win.edit_fp != edit_fp || win.crop != key {
                        return None;
                    }
                    if win.w < win_target.0 || win.h < win_target.1 {
                        return None;
                    }
                    canvas.set_width(win.w as u32);
                    canvas.set_height(win.h as u32);
                    web::ctx2d(&canvas)
                        .draw_image_with_html_canvas_element(&win.canvas, 0.0, 0.0)
                        .unwrap();
                    stale_source = win.source_fp != src_fp;
                    drawn_src = Some(win.source_fp);
                    Some(Sharp::Window { w: win.w, cw: cw_p })
                });
            }
            if sharp.is_none() {
                seed_hi_from_prefetch(&item, &photo, edit_fp, src_fp);
            }
            if sharp.is_none() {
                sharp = HI_CACHE.with(|c| {
                    let b = c.borrow();
                    let hi = b.as_ref()?;
                    if hi.item_id != item.id || hi.edit_fp != edit_fp {
                        return None;
                    }
                    if hi.w < target.0 || hi.h < target.1 {
                        return None;
                    }
                    if cropped_view {
                        blit_hi_frame(hi, &canvas, Some(crop_px(&item.edit, hi.w, hi.h)));
                    } else {
                        blit_hi_frame(hi, &canvas, None);
                    }
                    stale_source = hi.source_fp != src_fp || hi.stand_in;
                    drawn_src = (!hi.stand_in).then_some(hi.source_fp);
                    Some(Sharp::Whole { w: hi.w })
                });
            }
        }
        if let Some(fp) = drawn_src {
            if state.shown_src.get_untracked() != Some((item.id, fp)) {
                state.shown_src.set(Some((item.id, fp)));
            }
        }

        if sharp.is_none() {
            let mut frame = PipeFrame::new(p.into_owned(), w, h);
            let px_scale = pipeline::px_scale_for(sw, sh, PipeTarget::new(w, h));
            let params = state::render_edit(&item.edit, &item.layers);
            pipeline::finish_edit(&mut frame, &params, item.id as u32, px_scale);
            let p = frame.pixels;
            if cropped_view {
                // Composite at full size offscreen, then blit the crop region —
                // the same pipeline as export, so the preview matches the output.
                // Crop stays a stored rect; the original is never touched.
                let (cx, cy, cw, ch) = crop_px(&item.edit, w, h);
                let off = web::create_canvas(w as u32, h as u32);
                web::put_pixels(&off, &p, w as u32, h as u32);
                composite_layers(&off, &item.layers, w, h);
                canvas.set_width(cw as u32);
                canvas.set_height(ch as u32);
                web::ctx2d(&canvas)
                    .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                        &off, cx as f64, cy as f64, cw as f64, ch as f64,
                        0.0, 0.0, cw as f64, ch as f64,
                    )
                    .unwrap();
            } else {
                web::put_pixels(&canvas, &p, w as u32, h as u32);
                composite_layers(&canvas, &item.layers, w, h);
            }
        }
        if want_hi
            && gpu_ok
            && gkey.as_ref().is_some_and(|k| gpu::base_match(k) != Some(false))
        {
            // 60ms debounce so a fine-angle drag issues one base request, not
            // one per frame; a pending request that already covers this key
            // is left to land.
            GPU_DEBOUNCE.with(|d| d.set(d.get() + 1));
            let gen = GPU_DEBOUNCE.with(|d| d.get());
            let cb = Closure::once(move || {
                if GPU_DEBOUNCE.with(|d| d.get()) != gen {
                    return;
                }
                let Some(item) = state.current() else { return };
                if item.kind != MediaKind::Photo {
                    return;
                }
                if !full_present(item.id) && !proxy_in_mem(item.id) {
                    ensure_proxy(state, item.id, true);
                    return;
                }
                let Some(photo) = get_photo(item.id) else { return };
                let (src_id, (sw2, sh2)) = render_source(item.id, &photo);
                let (fw2, fh2) = working_dims(sw2, sh2, &item.edit);
                let t2 = hi_target(need_w, need_h, fw2, fh2, HI_MAX_EDGE as u32);
                let key = gpu::BaseKey {
                    item: item.id as u32,
                    src_id: src_id as u32,
                    src_fp: source_fingerprint(photo.gen, proxy_gen(item.id)),
                    rot90: item.edit.rot90,
                    fine_bits: item.edit.fine_angle.to_bits(),
                    need_w: t2.0 as u32,
                    need_h: t2.1 as u32,
                    raw_fp: gpu::raw_fingerprint(&item.edit.raw),
                };
                if gpu::base_match(&key) == Some(false) {
                    return;
                }
                let covered = GPU_BASE_REQ.with(|r| {
                    r.borrow().as_ref().is_some_and(|p| {
                        p.item == key.item
                            && p.rot90 == key.rot90
                            && p.fine_bits == key.fine_bits
                            && p.src_id == key.src_id
                            && p.src_fp == key.src_fp
                            && p.raw_fp == key.raw_fp
                            && p.need_w >= key.need_w
                            && p.need_h >= key.need_h
                    })
                });
                if covered {
                    return;
                }
                GPU_BASE_REQ.with(|r| *r.borrow_mut() = Some(key));
                let recipe = pworker::render_recipe(&item);
                let accepted = pworker::render_base(
                    src_id,
                    &recipe,
                    need_w,
                    need_h,
                    HI_MAX_EDGE as u32,
                    move |reply| {
                        let superseded = GPU_BASE_REQ.with(|r| {
                            let mut r = r.borrow_mut();
                            if r.as_ref() == Some(&key) {
                                *r = None;
                                false
                            } else {
                                r.is_some()
                            }
                        });
                        if superseded {
                            return;
                        }
                        match reply {
                            Ok(r) => {
                                if gpu::upload_base(key, &r.pixels.to_vec(), r.w, r.h) {
                                    state.hi_built.update(|g| *g += 1);
                                }
                            }
                            Err(_) => {
                                gpu::latch_off();
                                state.hi_built.update(|g| *g += 1);
                            }
                        }
                    },
                );
                if !accepted {
                    GPU_BASE_REQ.with(|r| *r.borrow_mut() = None);
                    gpu::latch_off();
                    state.hi_built.update(|g| *g += 1);
                }
            });
            let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                60,
            );
            cb.forget();
        }
        if want_hi && !gpu_ok && (sharp.is_none() || stale_source) {
            // Sharp frame lands 60ms after edits settle; until then the
            // cheap preview render keeps drags responsive. The worker's
            // base cache makes the follow-up render cheap, so a short
            // debounce no longer buys a costly re-decode of the frame.
            HI_DEBOUNCE.with(|d| d.set(d.get() + 1));
            let gen = HI_DEBOUNCE.with(|d| d.get());
            let cb = Closure::once(move || {
                if HI_DEBOUNCE.with(|d| d.get()) != gen {
                    return;
                }
                let Some(item) = state.current() else { return };
                if item.kind != MediaKind::Photo {
                    return;
                }
                if !full_present(item.id) && !proxy_in_mem(item.id) {
                    // Pixels not decoded yet — kick the loader; its
                    // completion nudge retriggers this effect and
                    // reschedules the hi-res render.
                    ensure_proxy(state, item.id, true);
                    return;
                }
                let Some(photo) = get_photo(item.id) else { return };
                let (src_id, (sw2, sh2)) = render_source(item.id, &photo);
                let (fw2, fh2) = working_dims(sw2, sh2, &item.edit);
                let t2 = hi_target(req_w, req_h, fw2, fh2, HI_MAX_EDGE as u32);
                HI_REQ_GEN.with(|d| d.set(d.get() + 1));
                let rgen = HI_REQ_GEN.with(|d| d.get());
                let recipe = pworker::render_recipe(&item);
                let sel = item.edit.selection.clone();
                let edit_fp = edit_fingerprint(&item);
                let source_fp = source_fingerprint(photo.gen, proxy_gen(item.id));
                // A direct-painted proxy may already cover this target; rendering
                // again would swap it for a smaller worker copy of the same pixels.
                if !cropped_view
                    && HI_CACHE.with(|c| {
                        c.borrow().as_ref().is_some_and(|hi| {
                            hi.item_id == item.id
                                && hi.edit_fp == edit_fp
                                && hi.source_fp == source_fp
                                && hi.w >= t2.0
                                && hi.h >= t2.1
                        })
                    })
                {
                    return;
                }
                let item_id = item.id;
                let item_cb = item.clone();
                if cropped_view {
                    let (fcx, fcy, fcw, fch) = crop_px(&item.edit, fw2, fh2);
                    let win_target =
                        hi_target(need_w, need_h, fcw, fch, WINDOW_MAX_EDGE as u32);
                    let request = perf::Span::new("hi.request")
                        .detail(format!("{}x{}", win_target.0, win_target.1));
                    let hi_t0 = perf::now();
                    let _accepted = pworker::render_window(
                        src_id,
                        &recipe,
                        need_w,
                        need_h,
                        WINDOW_MAX_EDGE as u32,
                        sel.as_ref(),
                        move |reply| {
                            if HI_REQ_GEN.with(|d| d.get()) != rgen {
                                return;
                            }
                            match reply {
                                Ok(r) => {
                                    perf::record("hi.worker_reply", perf::now() - hi_t0);
                                    let blit = perf::Span::new("hi.blit");
                                    let canvas = web::create_canvas(r.w, r.h);
                                    web::put_pixels_view(&canvas, &r.pixels, r.w, r.h);
                                    // Layers live in working-image coords;
                                    // scale/translate them onto the window.
                                    let ctx = web::ctx2d(&canvas);
                                    let sx = r.w as f64 / fcw as f64;
                                    let sy = r.h as f64 / fch as f64;
                                    let _ = ctx.scale(sx, sy);
                                    let _ = ctx.translate(-(fcx as f64), -(fcy as f64));
                                    composite_layers(&canvas, &item_cb.layers, fw2, fh2);
                                    let _ =
                                        ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
                                    HI_WINDOW.with(|c| {
                                        *c.borrow_mut() = Some(HiWindow {
                                            item_id,
                                            edit_fp,
                                            source_fp,
                                            crop: crop_key(&item_cb.edit.crop),
                                            canvas,
                                            w: r.w as usize,
                                            h: r.h as usize,
                                        })
                                    });
                                    state.hi_built.update(|g| *g += 1);
                                    drop(blit);
                                }
                                _ => {
                                    perf::record("hi.worker_reply", perf::now() - hi_t0);
                                }
                            }
                        },
                    );
                    drop(request);

                } else {
                    let request =
                        perf::Span::new("hi.request").detail(format!("{}x{}", req_w, req_h));
                    let hi_t0 = perf::now();
                    let _accepted = pworker::render(
                        src_id,
                        &recipe,
                        req_w,
                        req_h,
                        HI_MAX_EDGE as u32,
                        sel.as_ref(),
                        move |reply| {
                            if HI_REQ_GEN.with(|d| d.get()) != rgen {
                                return;
                            }
                            match reply {
                                Ok(r) => {
                                    perf::record("hi.worker_reply", perf::now() - hi_t0);
                                    // Keep a strictly larger cached frame for the same
                                    // edit and source instead of downgrading it.
                                    if item_cb.layers.iter().all(|l| !l.visible)
                                        && HI_CACHE.with(|c| {
                                            c.borrow().as_ref().is_some_and(|hi| {
                                                hi.item_id == item_id
                                                    && hi.edit_fp == edit_fp
                                                    && hi.source_fp == source_fp
                                                    && hi.w >= r.w as usize
                                                    && hi.h >= r.h as usize
                                            })
                                        })
                                    {
                                        return;
                                    }
                                    let blit = perf::Span::new("hi.blit");
                                    let canvas = web::create_canvas(r.w, r.h);
                                    web::put_pixels_view(&canvas, &r.pixels, r.w, r.h);
                                    composite_layers(
                                        &canvas,
                                        &item_cb.layers,
                                        r.w as usize,
                                        r.h as usize,
                                    );
                                    HI_CACHE.with(|c| {
                                        *c.borrow_mut() = Some(HiCache {
                                            item_id,
                                            edit_fp,
                                            source_fp,
                                            canvas: Some(canvas),
                                            bitmap: None,
                                            w: r.w as usize,
                                            h: r.h as usize,
                                            prefetched: false,
                                            stand_in: false,
                                        })
                                    });
                                    state.hi_built.update(|g| *g += 1);
                                    drop(blit);
                                }
                                _ => {
                                    perf::record("hi.worker_reply", perf::now() - hi_t0);
                                }
                            }
                        },
                    );
                    drop(request);

                }
            });
            let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                60,
            );
            cb.forget();
        }

        // Overlay draws below use full-image preview pixel coords; map them
        // onto the canvas backing (sharp-frame scale and/or the crop offset).
        let ctx = web::ctx2d(&canvas);
        let overlay_open = match (&sharp, cropped_view) {
            (Some(Sharp::Window { w: win_w, cw }), _) => {
                ctx.save();
                let s = *win_w as f64 / *cw as f64;
                let _ = ctx.scale(s, s);
                let _ = ctx.translate(-cx_p, -cy_p);
                true
            }
            (Some(Sharp::Whole { w: hw }), true) => {
                ctx.save();
                let s = *hw as f64 / w as f64;
                let _ = ctx.scale(s, s);
                let _ = ctx.translate(-cx_p, -cy_p);
                true
            }
            (Some(Sharp::Whole { w: hw }), false) => {
                ctx.save();
                let s = *hw as f64 / w as f64;
                let _ = ctx.scale(s, s);
                true
            }
            (None, true) => {
                ctx.save();
                let _ = ctx.translate(-cx_p, -cy_p);
                true
            }
            (None, false) => false,
        };

        // Draw editing handles on top for the selected path layer.
        if tab.get() == Tab::Layers && state.selected_tool.get() == Tool::Pen {
            if let Some(sel) = state.selected_layer.get() {
                if let Some(layer) = item.layers.iter().find(|l| l.id == sel) {
                    draw_path_edit_handles(&web::ctx2d(&canvas), layer, w as f64, h as f64);
                }
            }
        }

        // Adjustment-layer mask tint: editor-only, drawn for every layer whose
        // show_mask toggle is on (usually just the one being edited).
        if tab.get() == Tab::Layers {
            let ctx = web::ctx2d(&canvas);
            for layer in item.layers.iter() {
                if let LayerKind::Adjust(a) = &layer.kind {
                    if a.show_mask {
                        draw_layer_mask_overlay(&ctx, a.mask.as_ref(), a.invert, w as f64, h as f64);
                    }
                }
            }
        }

        // Draw selection overlay on the Select tab.
        if tab.get() == Tab::Select {
            let ctx = web::ctx2d(&canvas);
            if let Some(sel) = &item.edit.selection {
                draw_selection_overlay(&ctx, sel, w as f64, h as f64);
            }
            if let Some(drag) = select_drag.get() {
                draw_select_drag(&ctx, &drag, w as f64, h as f64);
            }
        }

        // Draw the heal stroke overlay on the Heal tab.
        if tab.get() == Tab::Heal {
            let ctx = web::ctx2d(&canvas);
            draw_heal_overlay(&ctx, heal_drag.get().as_deref(), state.heal_radius.get(), w as f64, h as f64);
        }

        // Clone tab: stroke discs plus a crosshair at the active sample point.
        if tab.get() == Tab::Clone {
            let ctx = web::ctx2d(&canvas);
            let r = state.clone_radius.get();
            draw_heal_overlay(&ctx, clone_drag.get().as_deref(), r, w as f64, h as f64);
            // Where the brush is currently sampling from: the source until an
            // aligned stroke fixes the offset, then source-follows-brush.
            let sample = match (clone_drag.get(), state.clone_source.get()) {
                (Some(pts), Some(src)) => {
                    let start = pts[0];
                    let off = state.clone_offset.get().unwrap_or((src.0 - start.0, src.1 - start.1));
                    pts.last().map(|&p| (p.0 + off.0, p.1 + off.1))
                }
                (None, Some(src)) => Some(src),
                _ => None,
            };
            if let Some((sx, sy)) = sample {
                draw_crosshair(&ctx, sx * w as f32, sy * h as f32);
            }
        }

        if overlay_open {
            ctx.restore();
        }
            render_span.set_detail(format!("{}x{}", canvas.width(), canvas.height()));
        });
        PREVIEW_DIRTY.with(|d| d.set(true));
        if !PREVIEW_PENDING.with(|p| p.get()) {
            PREVIEW_PENDING.with(|p| p.set(true));
            PREVIEW_DIRTY.with(|d| d.set(false));
            render();
            let trailing = render.clone();
            let cb = Closure::once(move || {
                PREVIEW_PENDING.with(|p| p.set(false));
                if PREVIEW_DIRTY.with(|d| d.replace(false)) {
                    trailing();
                }
            });
            web::request_animation_frame(cb.as_ref().unchecked_ref());
            cb.forget();
        }
    });

    view! {
        <div class="preview">
            {move || match state.current() {
                Some(m) if m.kind == MediaKind::Video => {
                    let e = m.edit.clone();
                    let style = format!(
                        "filter: brightness({}) contrast({}) saturate({}); transform: rotate({}deg);",
                        1.0 + e.brightness,
                        1.0 + e.contrast,
                        1.0 + e.saturation,
                        e.fine_angle
                    );
                    view! {
                        <div class="canvas-wrap video-wrap">
                            <video src=m.object_url.clone() controls style=style></video>
                            <VideoOverlays state=state/>
                            <SelectedTextOverlay state=state tab=tab layer_drag=layer_drag/>
                            <p class="dim">"Preview is approximate — exact render happens at export."</p>
                        </div>
                    }.into_view()
                }
                _ => view! {
                    <div
                        class="canvas-wrap"
                        style=move || {
                            state.current().map(|m| {
                                let (w, h) = working_dims(m.width, m.height, &m.edit);
                                let (dw, dh) = if tab.get() != Tab::Crop && !crop_is_full(&m.edit.crop) {
                                    let (_, _, cw, ch) = crop_px(&m.edit, w, h);
                                    (cw, ch)
                                } else {
                                    (w, h)
                                };
                                format!("--ar:{}", dw as f64 / dh.max(1) as f64)
                            }).unwrap_or_default()
                        }
                        class:cropped=move || {
                            tab.get() != Tab::Crop
                                && state.current().map(|m| m.kind == MediaKind::Photo && !crop_is_full(&m.edit.crop)).unwrap_or(false)
                        }
                        on:pointerdown=move |ev: web_sys::PointerEvent| {
                            canvas_loupe_clear(state);
                            if tab.get() == Tab::Clone {
                                if let Some(item) = state.current().filter(|m| m.kind == MediaKind::Photo) {
                                    if let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) {
                                        if state.clone_pick.get() || ev.alt_key() {
                                            state.clone_source.set(Some((nx, ny)));
                                            state.clone_offset.set(None);
                                            state.clone_pick.set(false);
                                        } else if state.clone_source.get().is_some() {
                                            clone_drag.set(Some(vec![(nx, ny)]));
                                        }
                                    }
                                }
                                return;
                            }
                            if tab.get() == Tab::Heal {
                                if let Some(item) = state.current().filter(|m| m.kind == MediaKind::Photo) {
                                    if let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) {
                                        heal_drag.set(Some(vec![(nx, ny)]));
                                    }
                                }
                                return;
                            }
                            if tab.get() == Tab::Select {
                                let Some(item) = state.current() else { return };
                                let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return; };
                                match state.selected_select_tool.get() {
                                    SelectTool::Rect => {
                                        select_drag.set(Some(SelectDrag::Rect((nx, ny), (nx, ny))));
                                    }
                                    SelectTool::Lasso => {
                                        select_drag.set(Some(SelectDrag::Lasso(vec![(nx, ny)])));
                                    }
                                    SelectTool::Wand => {
                                        wand_select(state, nx, ny);
                                    }
                                }
                                return;
                            }
                            if tab.get() != Tab::Layers { return; }
                            let Some(id) = state.selected_layer.get() else { return; };
                            let Some(item) = state.current() else { return; };
                            let Some(layer) = item.layers.iter().find(|l| l.id == id) else { return };
                            let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return };

                            match state.selected_tool.get() {
                                Tool::Select => {
                                    match &layer.kind {
                                        LayerKind::Text(t) => {
                                            layer_drag.set(Some((id, nx, ny, t.x, t.y)));
                                        }
                                        LayerKind::Raster(r) => {
                                            layer_drag.set(Some((id, nx, ny, r.x, r.y)));
                                        }
                                        _ => {}
                                    }
                                }
                                Tool::Pen => {
                                    let LayerKind::Path(ref p) = layer.kind else { return };
                                    if let Some((idx, handle)) = hit_test_path(&p.points, nx, ny) {
                                        if handle == PenHandle::Point && idx == 0 && p.points.len() >= 3 && !p.closed {
                                            state.update_current_item(|m| {
                                                if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                                                    if let LayerKind::Path(p) = &mut l.kind {
                                                        push_path_history(p);
                                                        p.closed = true;
                                                    }
                                                }
                                            });
                                        } else {
                                            let drag = match handle {
                                                PenHandle::Point => PenDrag::MovePoint(idx),
                                                PenHandle::Out => PenDrag::MoveOut(idx),
                                                PenHandle::In => PenDrag::MoveIn(idx),
                                            };
                                            pen_drag.set(Some((id, drag, nx, ny)));
                                        }
                                    } else {
                                        state.update_current_item(|m| {
                                            if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                                                if let LayerKind::Path(p) = &mut l.kind {
                                                    push_path_history(p);
                                                    p.points.push(PathPoint::new(nx, ny));
                                                }
                                            }
                                        });
                                    }
                                }
                                Tool::Brush => {
                                    let LayerKind::Brush(_) = &layer.kind else { return };
                                    state.update_current_item(|m| {
                                        if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                                            if let LayerKind::Brush(b) = &mut l.kind {
                                                push_brush_history(b);
                                                b.strokes.push(BrushStroke { points: vec![(nx, ny)] });
                                            }
                                        }
                                    });
                                    brush_draw.set(Some(id));
                                }
                            }
                        }
                        on:dblclick=move |ev: web_sys::MouseEvent| {
                            if tab.get() != Tab::Layers || state.selected_tool.get() != Tool::Pen { return; }
                            let Some(id) = state.selected_layer.get() else { return; };
                            let Some(item) = state.current() else { return; };
                            let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return; };
                            state.update_current_item(|m| {
                                if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                                    if let LayerKind::Path(p) = &mut l.kind {
                                        if let Some((idx, PenHandle::Point)) = hit_test_path(&p.points, nx, ny) {
                                            push_path_history(p);
                                            p.points[idx].smooth = !p.points[idx].smooth;
                                            if p.points[idx].smooth {
                                                p.points[idx].in_x = -p.points[idx].out_x;
                                                p.points[idx].in_y = -p.points[idx].out_y;
                                            }
                                        }
                                    }
                                }
                            });
                        }
                        on:pointermove=move |ev: web_sys::PointerEvent| {
                            canvas_loupe_move(state, tab, &ev);
                        }
                        on:pointerleave=move |_| canvas_loupe_clear(state)
                    >
                        <canvas node_ref=canvas_ref></canvas>
                        <Show when=move || raw_develop_pending(state) fallback=|| ()>
                            <div class="developing-badge" role="status" aria-live="polite">"Developing RAW…"</div>
                        </Show>
                        <Show when=move || tab.get() == Tab::Crop fallback=|| ()>
                            <CropOverlay state=state working=working drag=crop_drag pinch=crop_pinch/>
                        </Show>
                        <SelectedTextOverlay state=state tab=tab layer_drag=layer_drag/>
                    </div>
                }.into_view(),
            }}
            <CanvasLoupe state=state tab=tab main_canvas=canvas_ref/>
        </div>
    }
}

#[component]
pub(crate) fn VideoOverlays(state: AppState) -> impl IntoView {
    view! {
        <div class="video-overlays">
            {move || state.current_layers().into_iter().filter_map(|l| {
                if !l.visible { return None; }
                let LayerKind::Text(t) = &l.kind else { return None; };
                // Video preview is not cropped; identity mapping.
                let style = text_overlay_style(t, l.opacity, &state::CropRect::default());
                Some(view! { <div class="video-text-overlay" style=style>{t.text.clone()}</div> })
            }).collect_view()}
        </div>
    }
}

#[component]
pub(crate) fn SelectedTextOverlay(
    state: AppState,
    tab: RwSignal<Tab>,
    layer_drag: RwSignal<Option<(usize, f32, f32, f32, f32)>>,
) -> impl IntoView {
    let maybe_layer = move || {
        if state.selected_tool.get() != Tool::Select || tab.get() != Tab::Layers {
            return None;
        }
        let m = state.current()?;
        let id = state.selected_layer.get()?;
        let layer = m.layers.iter().find(|l| l.id == id)?;
        if !layer.visible {
            return None;
        }
        let LayerKind::Text(t) = &layer.kind else { return None; };
        Some((layer.id, t.clone(), layer.opacity, m.edit.crop))
    };

    view! {
        {move || {
            let (id, t, opacity, crop) = maybe_layer()?;
            let style = selected_text_overlay_style(&t, opacity, &crop);
            Some(view! {
                <div
                    class="text-overlay selected"
                    style=style
                    on:pointerdown=move |ev: web_sys::PointerEvent| {
                        if tab.get() != Tab::Layers || state.selected_tool.get() != Tool::Select { return; }
                        ev.stop_propagation();
                        ev.prevent_default();
                        let Some(item) = state.current() else { return };
                        let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return };
                        state.selected_layer.set(Some(id));
                        layer_drag.set(Some((id, nx, ny, t.x, t.y)));
                    }
                >{t.text.clone()}</div>
            })
        }}
    }
}

#[component]
pub(crate) fn CropOverlay(
    state: AppState,
    working: RwSignal<(usize, usize)>,
    drag: RwSignal<Option<(u8, f32, f32, state::CropRect)>>,
    pinch: RwSignal<
        Option<(
            Vec<(i32, f32, f32)>,
            Option<(state::CropRect, (f32, f32), f32)>,
        )>,
    >,
) -> impl IntoView {

    let norm_pos = |ev: &web_sys::PointerEvent| -> Option<(f32, f32)> {
        let el = web::document().query_selector(".canvas-wrap").ok().flatten()?;
        let rect = el.get_bounding_client_rect();
        let nx = ((ev.client_x() as f32 - rect.left() as f32) / rect.width() as f32).clamp(0.0, 1.0);
        let ny = ((ev.client_y() as f32 - rect.top() as f32) / rect.height() as f32).clamp(0.0, 1.0);
        Some((nx, ny))
    };

    // Register every finger that lands anywhere on the overlay; when the
    // second one lands, snapshot the crop and switch from drag to pinch.
    let register = move |ev: &web_sys::PointerEvent| {
        let Some((nx, ny)) = norm_pos(ev) else { return };
        pinch.update(|p| {
            let (pts, snap) = p.get_or_insert_with(|| (Vec::new(), None));
            let id = ev.pointer_id();
            if !pts.iter().any(|(pid, _, _)| *pid == id) {
                pts.push((id, nx, ny));
            }
            if pts.len() == 2 {
                if let Some(item) = state.current() {
                    let (a, b) = (pts[0], pts[1]);
                    let mid = ((a.1 + b.1) / 2.0, (a.2 + b.2) / 2.0);
                    let spread = ((a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt();
                    *snap = Some((item.edit.crop, mid, spread));
                    drag.set(None);
                }
            }
        });
    };

    let start = move |ev: web_sys::PointerEvent, handle: u8| {
        ev.prevent_default();
        // Corner handles sit inside .crop-rect, which has its own pointerdown
        // (move). Without this the press bubbles up and overwrites the drag.
        ev.stop_propagation();
        register(&ev);
        let Some(item) = state.current() else { return };
        let Some((nx, ny)) = norm_pos(&ev) else { return };
        drag.set(Some((handle, nx, ny, item.edit.crop)));
    };

    let pm = window_event_listener(leptos::ev::pointermove, move |ev| {
        // Pinch takes priority over the single-finger drag handlers.
        let pinch_active = pinch
            .try_get()
            .flatten()
            .map(|(_, s)| s.is_some())
            .unwrap_or(false);
        if pinch_active {
            pinch.update(|p| {
                let Some((pts, snap)) = p else { return };
                for pt in pts.iter_mut() {
                    if pt.0 == ev.pointer_id() {
                        if let Some((nx, ny)) = norm_pos(&ev) {
                            pt.1 = nx;
                            pt.2 = ny;
                        }
                    }
                }
                let Some(((orig, mid0, d0), (a, b))) =
                    snap.zip(pts.first().copied().zip(pts.get(1).copied()))
                else {
                    return;
                };
                let mid = ((a.1 + b.1) / 2.0, (a.2 + b.2) / 2.0);
                let spread = ((a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt();
                if d0 <= f32::EPSILON {
                    return;
                }
                let Some(ratio) = state.with_current(|m| m.edit.aspect.ratio().map(|(rw, rh)| rw / rh)) else { return };
                let Some((ww, wh)) = working.try_get() else { return };
                let c = zoom_crop(
                    orig,
                    mid0,
                    (mid.0 - mid0.0, mid.1 - mid0.1),
                    spread / d0,
                    ratio,
                    ww as f32 / wh as f32,
                );
                state.update_current(|e| e.crop = c);
            });
            return;
        }
        // Signals are disposed when the Crop tab unmounts, but this window
        // listener outlives them — bail out instead of touching dead signals.
        let Some(Some((handle, sx, sy, orig))) = drag.try_get() else { return };
        let Some(ratio) = state.with_current(|m| m.edit.aspect.ratio().map(|(rw, rh)| rw / rh)) else { return };
        let Some((ww, wh)) = working.try_get() else { return };
        let Some((nx, ny)) = norm_pos(&ev) else { return };
        let dx = nx - sx;
        let dy = ny - sy;

        let mut c = orig;
        let img_aspect = ww as f32 / wh as f32;

        match handle {
            0 => {
                c.x = (orig.x + dx).clamp(0.0, 1.0 - orig.w);
                c.y = (orig.y + dy).clamp(0.0, 1.0 - orig.h);
            }
            hdl => {
                // West handles anchor the right edge, east the left; north
                // anchor the bottom, south the top.
                let west = matches!(hdl, 1 | 3);
                let north = matches!(hdl, 1 | 2);
                let right = orig.x + orig.w;
                let bottom = orig.y + orig.h;
                let max_w = if west { right } else { 1.0 - orig.x };
                let max_h = if north { bottom } else { 1.0 - orig.y };
                let mut nw = match hdl {
                    1 | 3 => orig.w - dx,
                    _ => orig.w + dx,
                };
                let mut nh = match hdl {
                    1 | 2 => orig.h - dy,
                    _ => orig.h + dy,
                };
                if let Some(r) = ratio {
                    nw = nw.clamp(0.02, max_w).min(max_h * r / img_aspect);
                    nh = nw * img_aspect / r;
                } else {
                    nw = nw.clamp(0.02, max_w);
                    nh = nh.clamp(0.02, max_h);
                }
                c.w = nw;
                c.h = nh;
                c.x = if west { right - nw } else { orig.x };
                c.y = if north { bottom - nh } else { orig.y };
            }
        }
        state.update_current(|e| e.crop = c);
    });

    let pu = window_event_listener(leptos::ev::pointerup, move |ev| {
        let _ = drag.try_set(None);
        pinch.update(|p| {
            if let Some((pts, snap)) = p {
                pts.retain(|(pid, _, _)| *pid != ev.pointer_id());
                if pts.len() < 2 {
                    *snap = None;
                }
                if pts.is_empty() {
                    *p = None;
                }
            }
        });
    });

    let pc = window_event_listener(leptos::ev::pointercancel, move |ev| {
        let _ = drag.try_set(None);
        pinch.update(|p| {
            if let Some((pts, snap)) = p {
                pts.retain(|(pid, _, _)| *pid != ev.pointer_id());
                if pts.len() < 2 {
                    *snap = None;
                }
                if pts.is_empty() {
                    *p = None;
                }
            }
        });
    });

    // window_event_listener has no Drop — without this, every CropOverlay
    // remount (any crop edit re-renders the preview) adds another listener
    // pair, and each one applies the drag again: an update storm.
    on_cleanup(move || {
        pm.remove();
        pu.remove();
        pc.remove();
    });

    // Leptos' delegated on:wheel is registered passive, so prevent_default
    // (to keep the page from scrolling while zooming the crop) is rejected.
    // Attach a native non-passive listener instead; filter to events whose
    // target is inside the crop overlay.
    let wheel_zoom = Closure::wrap(Box::new(move |ev: web_sys::WheelEvent| {
        let inside = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest(".crop-overlay").ok().flatten())
            .is_some();
        if !inside {
            return;
        }
        // Plain wheel is a scroll gesture — with the canvas filling the
        // viewport it landed on the overlay constantly and hijacked the crop
        // rect. Trackpad/touchscreen pinch arrives as ctrl+wheel; only zoom on
        // that, and let plain wheel scroll the page.
        if !ev.ctrl_key() {
            return;
        }
        ev.prevent_default();
        let Some(item) = state.current() else { return };
        let Some((ww, wh)) = working.try_get() else { return };
        let el = web::document().query_selector(".canvas-wrap").ok().flatten();
        let Some(el) = el else { return };
        let rect = el.get_bounding_client_rect();
        let nx = ((ev.client_x() as f32 - rect.left() as f32) / rect.width() as f32).clamp(0.0, 1.0);
        let ny = ((ev.client_y() as f32 - rect.top() as f32) / rect.height() as f32).clamp(0.0, 1.0);
        let scale = if ev.delta_y() < 0.0 { 1.12 } else { 1.0 / 1.12 };
        let c = zoom_crop(
            item.edit.crop,
            (nx, ny),
            (0.0, 0.0),
            scale,
            item.edit.aspect.ratio().map(|(rw, rh)| rw / rh),
            ww as f32 / wh as f32,
        );
        state.update_current(|e| e.crop = c);
    }) as Box<dyn FnMut(_)>);
    let wheel_opts = web_sys::AddEventListenerOptions::new();
    wheel_opts.set_passive(false);
    web::window()
        .add_event_listener_with_callback_and_add_event_listener_options(
            "wheel",
            wheel_zoom.as_ref().unchecked_ref(),
            &wheel_opts,
        )
        .ok();
    on_cleanup(move || {
        web::window()
            .remove_event_listener_with_callback("wheel", wheel_zoom.as_ref().unchecked_ref())
            .ok();
    });

    view! {
        {move || {
            let Some(item) = state.current() else {
                return view! { <div></div> }.into_view();
            };
            let c = item.edit.crop;
            let pct = |v: f32| format!("{}%", v * 100.0);
            view! {
                <div
                    class="crop-overlay"
                    on:pointerdown=move |ev| {
                        ev.prevent_default();
                        register(&ev);
                    }
                >
                    <div class="crop-dim" style:left="0" style:top="0"
                        style:width="100%" style:height=pct(c.y)></div>
                    <div class="crop-dim" style:left="0" style:top=pct(c.y + c.h)
                        style:width="100%" style:height=pct(1.0 - c.y - c.h)></div>
                    <div class="crop-dim" style:left="0" style:top=pct(c.y)
                        style:width=pct(c.x) style:height=pct(c.h)></div>
                    <div class="crop-dim" style:left=pct(c.x + c.w) style:top=pct(c.y)
                        style:width=pct(1.0 - c.x - c.w) style:height=pct(c.h)></div>
                    <div
                        class="crop-rect"
                        style:left=pct(c.x)
                        style:top=pct(c.y)
                        style:width=pct(c.w)
                        style:height=pct(c.h)
                        on:pointerdown=move |ev| start(ev, 0)
                    >
                        <div class="handle nw" on:pointerdown=move |ev| start(ev, 1)></div>
                        <div class="handle ne" on:pointerdown=move |ev| start(ev, 2)></div>
                        <div class="handle sw" on:pointerdown=move |ev| start(ev, 3)></div>
                        <div class="handle se" on:pointerdown=move |ev| start(ev, 4)></div>
                    </div>
                </div>
            }.into_view()
        }}
    }
}

/// Whether the worker holds the neutral (un-toned 16-bit) frame for the source
/// this item renders from, which a developed GPU base is built from.
fn source_has_neutral(id: usize, photo: &PhotoData) -> bool {
    if photo.full {
        photo.neutral
    } else {
        proxy_data(id).is_some_and(|p| p.neutral)
    }
}

/// True while the selected RAW is not yet shown from its own sensor develop:
/// the camera-JPEG proxy is still up, the 16-bit develop that RAW edits need
/// has not landed, or it has landed but the last sharp frame drawn (tracked
/// in `shown_src` by the render pass) is still from an older source. Edits are drawn live meanwhile but approximately, so the badge says
/// the look will still change (owner decision, 2026-11-05). Only the source
/// is checked, not the edit, so ordinary slider drags never flash the badge.
fn raw_develop_pending(state: AppState) -> bool {
    // Re-evaluate when a proxy lands or a frame is built.
    state.hi_built.track();
    let Some(item) = state.current() else { return false };
    if item.kind != MediaKind::Photo {
        return false;
    }
    let Some(photo) = get_photo(item.id) else { return false };
    if !photo.raw || photo.full {
        return false;
    }
    if !proxy_developed(item.id) || proxy_wants_neutral(state, item.id) {
        return true;
    }
    let src_fp = source_fingerprint(photo.gen, proxy_gen(item.id));
    state.shown_src.get() != Some((item.id, src_fp))
}

