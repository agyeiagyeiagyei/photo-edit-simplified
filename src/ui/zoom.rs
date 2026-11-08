//! View-only zoom for the editing viewport: the floating control cluster,
//! cursor-anchored wheel/pinch gestures, drag pan and the keyboard shortcuts.
//! Nothing here writes the edit — the view transform is display state only.

use crate::prelude::*;
use crate::*;

/// Tabs whose preview can be zoomed. Crop keeps its own gesture, Rotate has
/// geometry handles, and Trim has no photo canvas. Export keeps the view
/// because checking sharpness next to the export settings is the point.
pub(crate) fn zoomable_tab(tab: Tab) -> bool {
    matches!(
        tab,
        Tab::Raw | Tab::Color | Tab::Select | Tab::Heal | Tab::Clone | Tab::Layers | Tab::Export
    )
}

/// True when the view shows at least one image pixel per CSS pixel; the 1:1
/// hover loupe would only repeat what the canvas already shows there.
pub(crate) fn view_at_native(state: AppState, tab: Tab) -> bool {
    zoomable_tab(tab) && state.view_zoom.get_untracked().zoom >= view_fit_zoom(state)
}

/// Zoom at which one image pixel lands on one CSS pixel, relative to Fit.
pub(crate) fn view_fit_zoom(state: AppState) -> f32 {
    let Some(m) = state.current() else {
        return 1.0;
    };
    let (w, h) = working_dims(m.width, m.height, &m.edit);
    let (_, _, cw, _) = crop_px(&m.edit, w, h);
    let Some((css_w, _)) = canvas_css_size() else {
        return 1.0;
    };
    if css_w < 1.0 || cw == 0 {
        return 1.0;
    }
    (cw as f32 / css_w).clamp(1.0, state::MAX_VIEW_ZOOM)
}

/// Crop-normalized point under a box-local (0..1) position.
pub(crate) fn view_anchor(view: state::ViewZoom, bx: f32, by: f32) -> (f32, f32) {
    let win = view.window();
    (win.x + bx * win.w, win.y + by * win.h)
}

/// Past native resolution the window backing holds one image pixel per source
/// pixel and the browser stretches it; smoothing there would blur precisely
/// the detail a close-up is inspected for. Below two device pixels per image
/// pixel the frame is downscaled, where interpolation is the better answer.
pub(crate) fn sync_canvas_rendering(canvas: &web_sys::HtmlCanvasElement) {
    let css_w = canvas.client_width() as f64;
    if css_w < 1.0 || canvas.width() == 0 {
        return;
    }
    let ratio = web::window().device_pixel_ratio() * css_w / canvas.width() as f64;
    let want = if ratio >= 2.0 { "pixelated" } else { "auto" };
    let style = canvas.style();
    if style.get_property_value("image-rendering").unwrap_or_default() != want {
        let _ = style.set_property("image-rendering", want);
    }
}

fn percent_label(p: f32) -> String {
    if p >= 10.0 || p <= 0.0 {
        format!("{:.0}%", p)
    } else {
        format!("{:.1}%", p)
    }
}

fn wheel_px(ev: &web_sys::WheelEvent, v: f64) -> f32 {
    // deltaMode: 1 = lines, 2 = pages; pixels otherwise.
    let scale = match ev.delta_mode() {
        1 => 16.0,
        2 => 400.0,
        _ => 1.0,
    };
    (v * scale) as f32
}

/// A focused control owns its keys — sliders, text fields and buttons all use
/// arrows, +/− and Space themselves. Zoom shortcuts only win these back when
/// the pointer sits on the canvas, where the user's intent is unambiguous.
fn control_has_focus(ev: &web_sys::KeyboardEvent) -> bool {
    let Some(el) = ev
        .target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
    else {
        return false;
    };
    matches!(
        el.tag_name().as_str(),
        "INPUT" | "TEXTAREA" | "SELECT" | "OPTION" | "BUTTON"
    ) || el
        .dyn_ref::<web_sys::HtmlElement>()
        .is_some_and(|h| h.is_content_editable())
}

fn pointer_over_canvas() -> bool {
    web::document()
        .query_selector(".canvas-wrap")
        .ok()
        .flatten()
        .is_some_and(|el| el.matches(":hover").unwrap_or(false))
}

#[component]
pub(crate) fn ZoomControls(state: AppState, tab: RwSignal<Tab>) -> impl IntoView {
    let zoomable = move || {
        state
            .current()
            .is_some_and(|m| m.kind == MediaKind::Photo)
            && zoomable_tab(tab.get())
    };

    let step = move |factor: f32| {
        let mut view = state.view_zoom.get_untracked();
        view.set_zoom(view.zoom * factor);
        state.view_zoom.set(view);
    };
    let native = move || {
        let mut view = state.view_zoom.get_untracked();
        view.set_zoom(view_fit_zoom(state));
        state.view_zoom.set(view);
    };

    // Ctrl/⌘+wheel and trackpad pinch zoom about the pointer; a plain wheel
    // pans once the view is zoomed and otherwise keeps scrolling the page, so
    // the canvas never traps scroll at Fit.
    let wheel = Closure::wrap(Box::new(move |ev: web_sys::WheelEvent| {
        if !zoomable() {
            return;
        }
        let inside = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest(".canvas-wrap").ok().flatten())
            .is_some();
        if !inside {
            return;
        }
        let Some(el) = web::document().query_selector(".canvas-wrap").ok().flatten() else {
            return;
        };
        let rect = el.get_bounding_client_rect();
        if rect.width() < 1.0 || rect.height() < 1.0 {
            return;
        }
        let bx = ((ev.client_x() as f32 - rect.left() as f32) / rect.width() as f32).clamp(0.0, 1.0);
        let by = ((ev.client_y() as f32 - rect.top() as f32) / rect.height() as f32).clamp(0.0, 1.0);
        let mut view = state.view_zoom.get_untracked();
        if ev.ctrl_key() || ev.meta_key() {
            ev.prevent_default();
            let dy = wheel_px(&ev, ev.delta_y());
            let scale = 1.12_f32.powf((-dy / 100.0).clamp(-3.0, 3.0));
            let anchor = view_anchor(view, bx, by);
            view.set_zoom_at(view.zoom * scale, anchor);
            state.view_zoom.set(view);
        } else if view.zoomed() {
            ev.prevent_default();
            let dx = wheel_px(&ev, ev.delta_x());
            let dy = wheel_px(&ev, ev.delta_y());
            view.pan_by(
                dx / (view.zoom * rect.width() as f32),
                dy / (view.zoom * rect.height() as f32),
            );
            state.view_zoom.set(view);
        }
    }) as Box<dyn FnMut(_)>);
    let wheel_opts = web_sys::AddEventListenerOptions::new();
    wheel_opts.set_passive(false);
    web::window()
        .add_event_listener_with_callback_and_add_event_listener_options(
            "wheel",
            wheel.as_ref().unchecked_ref(),
            &wheel_opts,
        )
        .ok();
    on_cleanup(move || {
        web::window()
            .remove_event_listener_with_callback("wheel", wheel.as_ref().unchecked_ref())
            .ok();
    });

    // Space / middle-button drag pans while zoomed. Capture phase so the pan
    // wins over the tool handlers on the canvas.
    let pan = Rc::new(Cell::new(None::<(i32, f64, f64)>));
    let space = Rc::new(Cell::new(false));
    let pan_ptr = pan.clone();
    let pan_space = space.clone();
    let pan_down = Closure::wrap(Box::new(move |ev: web_sys::PointerEvent| {
        if !zoomable() {
            return;
        }
        let inside = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest(".canvas-wrap").ok().flatten())
            .is_some();
        if !inside || !state.view_zoom.get_untracked().zoomed() {
            return;
        }
        if ev.button() != 1 && !pan_space.get() {
            return;
        }
        ev.prevent_default();
        ev.stop_propagation();
        pan_ptr.set(Some((ev.pointer_id(), ev.client_x() as f64, ev.client_y() as f64)));
    }) as Box<dyn FnMut(_)>);
    let pan_ptr = pan.clone();
    let pan_move = Closure::wrap(Box::new(move |ev: web_sys::PointerEvent| {
        let Some((id, px, py)) = pan_ptr.get() else { return };
        if ev.pointer_id() != id {
            return;
        }
        if ev.buttons() == 0 {
            pan_ptr.set(None);
            return;
        }
        ev.prevent_default();
        ev.stop_propagation();
        let Some(el) = web::document().query_selector(".canvas-wrap").ok().flatten() else {
            return;
        };
        let rect = el.get_bounding_client_rect();
        if rect.width() < 1.0 || rect.height() < 1.0 {
            return;
        }
        let mut view = state.view_zoom.get_untracked();
        view.pan_by(
            -(ev.client_x() as f32 - px as f32) / (view.zoom * rect.width() as f32),
            -(ev.client_y() as f32 - py as f32) / (view.zoom * rect.height() as f32),
        );
        state.view_zoom.set(view);
        pan_ptr.set(Some((id, ev.client_x() as f64, ev.client_y() as f64)));
    }) as Box<dyn FnMut(_)>);
    let pan_ptr = pan.clone();
    let pan_up = Closure::wrap(Box::new(move |ev: web_sys::PointerEvent| {
        if pan_ptr.get().is_some_and(|(id, _, _)| id == ev.pointer_id()) {
            pan_ptr.set(None);
        }
    }) as Box<dyn FnMut(_)>);
    let cap = web_sys::AddEventListenerOptions::new();
    cap.set_capture(true);
    for (name, cb) in [
        ("pointerdown", pan_down.as_ref().unchecked_ref()),
        ("pointermove", pan_move.as_ref().unchecked_ref()),
        ("pointerup", pan_up.as_ref().unchecked_ref()),
        ("pointercancel", pan_up.as_ref().unchecked_ref()),
    ] {
        web::window()
            .add_event_listener_with_callback_and_add_event_listener_options(name, cb, &cap)
            .ok();
    }
    on_cleanup(move || {
        for (name, cb) in [
            ("pointerdown", pan_down.as_ref().unchecked_ref()),
            ("pointermove", pan_move.as_ref().unchecked_ref()),
            ("pointerup", pan_up.as_ref().unchecked_ref()),
            ("pointercancel", pan_up.as_ref().unchecked_ref()),
        ] {
            web::window()
                .remove_event_listener_with_callback_and_bool(name, cb, true)
                .ok();
        }
    });

    // Keyboard: +/− step, 0 = Fit, 1 = 100%, arrows pan while zoomed.
    let key_space = space.clone();
    let kd = window_event_listener(leptos::ev::keydown, move |ev: web_sys::KeyboardEvent| {
        if !zoomable() {
            return;
        }
        if control_has_focus(&ev) && !pointer_over_canvas() {
            return;
        }
        if ev.ctrl_key() || ev.meta_key() || ev.alt_key() {
            return;
        }
        let mut view = state.view_zoom.get_untracked();
        if ev.key() == " " || ev.code() == "Space" {
            // The pointer may be over the canvas while a toolbar button still
            // holds focus; without this, Space would pan *and* press it.
            ev.prevent_default();
            key_space.set(true);
            return;
        }
        let handled = match ev.key().as_str() {
            "+" | "=" => {
                view.set_zoom(view.zoom * 1.4);
                true
            }
            "-" | "_" => {
                view.set_zoom(view.zoom / 1.4);
                true
            }
            "0" => {
                view.reset();
                true
            }
            "1" => {
                view.set_zoom(view_fit_zoom(state));
                true
            }
            "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown" if view.zoomed() => {
                let step = 0.1 / view.zoom;
                let (dx, dy) = match ev.key().as_str() {
                    "ArrowLeft" => (-step, 0.0),
                    "ArrowRight" => (step, 0.0),
                    "ArrowUp" => (0.0, -step),
                    _ => (0.0, step),
                };
                view.pan_by(dx, dy);
                true
            }
            _ => false,
        };
        if handled {
            ev.prevent_default();
            state.view_zoom.set(view);
        }
    });
    let key_space = space.clone();
    let ku = window_event_listener(leptos::ev::keyup, move |ev: web_sys::KeyboardEvent| {
        if (ev.key() == " " || ev.code() == "Space") && key_space.get() {
            // Paired with the keydown half: a focused button must not fire on
            // the release of a pan-hold.
            ev.prevent_default();
            key_space.set(false);
        }
    });
    on_cleanup(move || {
        kd.remove();
        ku.remove();
    });

    let pct = move || {
        state.viewport_gen.get();
        let view = state.view_zoom.get();
        percent_label(view.zoom / view_fit_zoom(state) * 100.0)
    };
    let at_fit = move || !state.view_zoom.get().zoomed();

    view! {
        <Show when=zoomable fallback=|| ()>
            <div
                class="zoom-cluster"
                role="group"
                aria-label="Zoom"
                on:pointerdown=move |ev| ev.stop_propagation()
                on:dblclick=move |ev| ev.stop_propagation()
            >
                <button title="Zoom out (-)" on:click=move |_| step(1.0 / 1.4)>"−"</button>
                <button
                    class="zoom-pct"
                    title="Fit to canvas"
                    on:click=move |_| state.view_zoom.set(state::ViewZoom::default())
                >
                    {pct}
                </button>
                <button title="Zoom in (+)" on:click=move |_| step(1.4)>"+"</button>
                <button class="zoom-fit" class:active=at_fit title="Fit (0)" on:click=move |_| {
                    state.view_zoom.set(state::ViewZoom::default())
                }>"Fit"</button>
                <button class="zoom-native" title="100% (1)" on:click=move |_| native()>"100%"</button>
            </div>
        </Show>
    }
}
