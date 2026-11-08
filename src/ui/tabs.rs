//! Editor tab panels: crop, select, rotate, colour, RAW develop, blur/grain, levels/curves, heal/clone, trim and export.

use crate::prelude::*;
use crate::*;

#[component]
pub(crate) fn CropTab(state: AppState, tab: RwSignal<Tab>) -> impl IntoView {
    let set_aspect = move |a: Aspect| {
        let Some(item) = state.current() else { return };
        let (w, h) = working_dims(item.width, item.height, &item.edit);
        let crop = default_crop(w, h, a);
        state.update_current(|e| {
            e.aspect = a;
            e.crop = crop;
        });
    };
    view! {
        <div class="chips">
            {Aspect::ALL.map(|a| {
                view! {
                    <button
                        class="chip"
                        class:active=move || state.current().map(|m| m.edit.aspect == a).unwrap_or(false)
                        on:click=move |_| set_aspect(a)
                    >
                        {a.label()}
                    </button>
                }
            })}
        </div>
        <div class="row">
            <button
                class="btn"
                disabled=move || state.current().map(|m| crop_is_full(&m.edit.crop)).unwrap_or(true)
                on:click=move |_| state.update_current(|e| {
                    e.crop = state::CropRect::default();
                    e.aspect = Aspect::Original;
                })
            >
                "Reset"
            </button>
            <button class="btn primary" on:click=move |_| tab.set(Tab::Color)>
                "Done"
            </button>
        </div>
        <p class="dim">"Drag the crop box on the image. Corners resize. Crop is non-destructive: Done shows the result, and you can come back here anytime to re-adjust."</p>
    }
}

/// Smallest brush the sliders offer: a 2 px diameter (1 px radius). The
/// worker already clamps a stroke radius to 1 px, so anything lower would
/// just be a label that lies.
const BRUSH_MIN_PX: f64 = 2.0;

/// Log position (0..=1) of a brush diameter in image pixels. A log curve is
/// what makes a 2 px dust brush and a 2000 px soft brush share one slider
/// without the small end hiding in the first pixel of travel.
fn brush_log_pos(px: f64, min: f64, max: f64) -> f64 {
    let hi = max.max(min * 1.001);
    (px.max(min) / min).ln() / (hi / min).ln()
}

/// Inverse of [`brush_log_pos`].
fn brush_log_size(t: f64, min: f64, max: f64) -> f64 {
    let hi = max.max(min * 1.001);
    min * (hi / min).powf(t.clamp(0.0, 1.0))
}

fn brush_px_label(px: f64) -> String {
    if px >= 100.0 {
        format!("{:.0} px", px)
    } else if px >= 10.0 {
        format!("{:.1} px", px)
    } else {
        format!("{:.2} px", px)
    }
}

/// Brush size slider, logarithmic and labelled in image pixels for the photo
/// on screen. `value` keeps the stored fraction of the working diagonal —
/// a diameter for the selection brush, a radius for heal and clone — so the
/// same size follows a photo of any resolution.
#[component]
fn BrushSizeSlider(
    state: AppState,
    value: RwSignal<f32>,
    label: &'static str,
    /// True when `value` is a diameter fraction; false when it is a radius.
    diameter: bool,
    /// Largest size the slider reaches, in the same fraction units as `value`.
    max_frac: f32,
) -> impl IntoView {
    let scale = move || if diameter { 1.0 } else { 2.0 };
    let diag = move || {
        let Some(m) = state.current() else {
            return 7211.0;
        };
        let (w, h) = working_dims(m.width, m.height, &m.edit);
        ((w * w + h * h) as f64).sqrt().max(1.0)
    };
    let max_px = move || max_frac as f64 * scale() * diag();
    let size_px = move || value.get() as f64 * scale() * diag();
    view! {
        <label class="slider">
            <span>{label} {move || brush_px_label(size_px())}</span>
            <input
                type="range" min="0" max="1000" step="1"
                attr:data-min-px=BRUSH_MIN_PX.to_string()
                attr:data-max-px=move || max_px().to_string()
                attr:data-diag-px=move || diag().to_string()
                attr:data-scale=move || scale().to_string()
                prop:value=move || {
                    format!("{:.0}", brush_log_pos(size_px(), BRUSH_MIN_PX, max_px()) * 1000.0)
                }
                on:input=move |ev| {
                    let t: f64 = event_target_value(&ev)
                        .parse::<f64>()
                        .unwrap_or(500.0)
                        / 1000.0;
                    let px = brush_log_size(t, BRUSH_MIN_PX, max_px());
                    value.set(((px / (scale() * diag())) as f32).min(max_frac));
                }
            />
        </label>
    }
}

#[component]
pub(crate) fn SelectTab(state: AppState) -> impl IntoView {
    let has_selection =
        move || state.current().map(|m| m.edit.selection.is_some()).unwrap_or(false);
    // The layer whose mask the Brush and the mask panel edit: the selected
    // adjustment, or a layer whose mask thumbnail is armed.
    let mask_layer = create_memo(move |_| {
        let m = state.current()?;
        mask_layer_for(state.mask_target.get(), state.selected_layer.get(), &m)
    });
    let mask_label = create_memo(move |_| {
        let id = mask_layer.get()?;
        let m = state.current()?;
        Some(layer_row_label(&m.layers, id))
    });
    let layer_has_mask = move || {
        let Some(id) = mask_layer.get() else { return false };
        state.current().is_some_and(|m| m.layers.iter().any(|l| l.id == id && l.mask.is_some()))
    };
    let is_raw = move || {
        state
            .current()
            .map(|m| get_photo(m.id).map(|p| p.raw).unwrap_or(false))
            .unwrap_or(false)
    };
    let feather = move || {
        state
            .current()
            .and_then(|m| m.edit.selection)
            .map(|s| s.feather)
            .unwrap_or(0.0)
    };
    view! {
        <div class="select-tab">
            <Show
                when=move || mask_label.get().is_some()
                fallback=|| view! {
                    <div class="editing-line"><span>"Editing: " <b>"the photo's selection"</b></span></div>
                }
            >
                <div class="mask-panel">
                    <div class="editing-line">
                        <span>"Editing: " <b>{move || format!("{} mask", mask_label.get().unwrap_or_default())}</b></span>
                        <button
                            class="link-btn"
                            on:click=move |_| {
                                state.mask_target.set(None);
                                state.selected_layer.set(None);
                            }
                        >
                            "Edit the photo's selection instead"
                        </button>
                    </div>
                    <div class="row select-row">
                        <button
                            class="btn primary"
                            disabled=move || !has_selection()
                            on:click=move |_| commit_selection_to_mask(state, false)
                        >
                            "Add selection to mask"
                        </button>
                        <button
                            class="btn"
                            disabled=move || !has_selection()
                            on:click=move |_| commit_selection_to_mask(state, true)
                        >
                            "Remove selection from mask"
                        </button>
                        <button class="btn" disabled=move || !layer_has_mask() on:click=move |_| invert_layer_mask(state)>
                            "Invert mask"
                        </button>
                        <button class="btn" disabled=move || !layer_has_mask() on:click=move |_| clear_layer_mask(state)>
                            "Clear mask"
                        </button>
                        <button
                            class="btn"
                            disabled=move || !state.brush_history.with(|h| h.can_undo())
                            on:click=move |_| {
                                brush_undo(state);
                            }
                        >
                            "Undo"
                        </button>
                    </div>
                    <p class="dim">
                        "Red shows where the adjustment applies. Brush paints it directly; Rect, \
                         Lasso, Wand and Subject make a selection to add or remove with the \
                         buttons above."
                    </p>
                </div>
            </Show>
            <div class="chips">
                {SelectTool::ALL.map(|t| {
                    let active = move || state.selected_select_tool.get() == t;
                    view! {
                        <button
                            class="chip"
                            class:active=active
                            on:click=move |_| state.selected_select_tool.set(t)
                        >
                            {t.label()}
                        </button>
                    }
                })}
            </div>
            <Show
                when=move || state.selected_select_tool.get() == SelectTool::Wand
                fallback=|| ()
            >
                <label class="slider">
                    <span>"Tolerance: " {move || state.wand_tolerance.get().to_string()}</span>
                    <input
                        type="range" min="0" max="100" step="1"
                        prop:value=move || state.wand_tolerance.get().to_string()
                        on:input=move |ev| {
                            let v: i32 = event_target_value(&ev).parse().unwrap_or(32);
                            state.wand_tolerance.set(v.clamp(0, 255));
                        }
                    />
                </label>
                <label class="row" style="justify-content:flex-start;gap:0.5rem">
                    <input
                        type="checkbox"
                        prop:checked=move || state.wand_contiguous.get()
                        on:change=move |ev| {
                            state.wand_contiguous.set(event_target_checked(&ev));
                        }
                    />
                    "Contiguous"
                </label>
            </Show>
            <Show
                when=move || state.selected_select_tool.get() == SelectTool::Subject
                fallback=|| ()
            >
                <div class="chips">
                    <button
                        class="chip"
                        class:active=move || !state.subject_remove.get()
                        on:click=move |_| state.subject_remove.set(false)
                    >
                        "Add"
                    </button>
                    <button
                        class="chip"
                        class:active=move || state.subject_remove.get()
                        on:click=move |_| state.subject_remove.set(true)
                    >
                        "Remove"
                    </button>
                    <button
                        class="chip"
                        on:click=move |_| {
                            state.subject_photo.set(None);
                            clear_subject_brush_ops(state);
                            state.update_current(|e| {
                                e.selection = None;
                                e.selection_rev += 1;
                            });
                        }
                    >
                        "Start over"
                    </button>
                </div>
            </Show>
            <Show
                when=move || state.selected_select_tool.get() == SelectTool::Brush
                fallback=|| ()
            >
                <div class="chips">
                    <button
                        class="chip"
                        class:active=move || !state.brush_remove.get()
                        on:click=move |_| state.brush_remove.set(false)
                    >
                        "Add"
                    </button>
                    <button
                        class="chip"
                        class:active=move || state.brush_remove.get()
                        on:click=move |_| state.brush_remove.set(true)
                    >
                        "Remove"
                    </button>
                    <button
                        class="chip"
                        disabled=move || !state.brush_history.with(|h| h.can_undo())
                        on:click=move |_| {
                            brush_undo(state);
                        }
                    >
                        "Undo"
                    </button>
                    <button
                        class="chip"
                        disabled=move || !state.brush_history.with(|h| h.can_redo())
                        on:click=move |_| {
                            brush_redo(state);
                        }
                    >
                        "Redo"
                    </button>
                </div>
                <BrushSizeSlider state=state value=state.brush_size label="Size: " diameter=true max_frac=0.3/>
                <label class="slider">
                    <span>"Softness: " {move || format!("{:.0}%", state.brush_softness.get() * 100.0)}</span>
                    <input
                        type="range" min="0" max="1" step="0.01"
                        prop:value=move || state.brush_softness.get().to_string()
                        on:input=move |ev| {
                            let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                            state.brush_softness.set(v.clamp(0.0, 1.0));
                        }
                    />
                </label>
                <p class="dim">
                    {move || {
                        state.selected_layer.track();
                        state.mask_target.track();
                        let Some(item) = state.current() else {
                            return "Brush edits the global selection (blue tint).";
                        };
                        match brush_target(state, &item) {
                            BrushTarget::Layer(id) => {
                                let adjust = item
                                    .layers
                                    .iter()
                                    .find(|l| l.id == id)
                                    .is_some_and(|l| matches!(l.kind, LayerKind::Adjust(_)));
                                if adjust {
                                    "Brush edits the selected adjustment layer's mask (red tint)."
                                } else {
                                    "Brush edits the selected layer's mask (red tint)."
                                }
                            }
                            BrushTarget::Global => "Brush edits the global selection (blue tint).",
                        }
                    }}
                </p>
            </Show>
            <div class="row select-row">
                <button
                    class="btn"
                    disabled=move || !has_selection()
                    on:click=move |_| selection_to_layer(state, false)
                >
                    "Copy to layer"
                </button>
                <button
                    class="btn"
                    disabled=move || !has_selection()
                    on:click=move |_| selection_to_layer(state, true)
                >
                    "Cut to layer"
                </button>
                <button
                    class="btn"
                    disabled=move || !has_selection()
                    on:click=move |_| delete_selection(state)
                >
                    "Delete"
                </button>
                <button
                    class="btn"
                    disabled=move || !has_selection()
                    on:click=move |_| content_fill_selection(state)
                >
                    "Content fill"
                </button>
                <button
                    class="btn"
                    disabled=move || !has_selection()
                    title="Select everything outside the current selection"
                    on:click=move |_| invert_selection(state)
                >
                    "Invert"
                </button>
                <button
                    class="btn"
                    on:click=move |_| {
                        state.subject_photo.set(None);
                        clear_subject_brush_ops(state);
                        state.update_current(|e| {
                            e.selection = None;
                            e.selection_rev += 1;
                        });
                    }
                >
                    "Clear"
                </button>
            </div>
            <label class="slider">
                <span>"Feather: " {move || format!("{:.0}%", feather() * 100.0)}</span>
                <input
                    type="range" min="0" max="0.25" step="0.01"
                    prop:value=move || feather().to_string()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                        state.update_current(|e| {
                            if let Some(ref mut s) = e.selection {
                                s.feather = v.clamp(0.0, 0.25);
                                // Feather changes the mask, so the tint cached
                                // for this revision no longer matches it.
                                e.selection_rev += 1;
                            }
                        });
                    }
                />
            </label>
            <button class="btn primary" on:click=move |_| isolate_subject(state)>
                "Isolate person"
            </button>
            <p class="dim">
                "Rect/Lasso select a region; Wand selects pixels similar to the one you click. \
                 Subject taps a bird or animal and refines with Add/Remove taps or a drag-box; \
                 the model downloads ~15 MB on first use (photo-only). Brush paints into or out \
                 of the selection (or the selected adjustment layer's mask) with Size and \
                 Softness; each stroke is one undo step. \
                 With an adjustment selected, its mask shows in red: Brush paints it, and any \
                 selection can be added to it or removed from it. \
                 Cut/Copy lift the selection to a raster layer; Content fill synthesizes it from \
                 the surroundings; Invert selects everything outside the selection. \
                 Isolate person runs MediaPipe Selfie Segmentation."
            </p>
            <Show when=is_raw fallback=|| ()>
                <p class="dim">
                    "This is a RAW photo. Cut, Delete and Content fill bake the pixels into an \
                     8-bit image: a reload restores the baked result and the RAW develop \
                     settings no longer apply. Heal and Clone stay non-destructive."
                </p>
            </Show>
        </div>
    }
}

#[component]
pub(crate) fn RotateTab(state: AppState) -> impl IntoView {
    // With a transformable layer selected the tab turns and scales that layer;
    // otherwise it turns the photo. The memo changes only with the selection,
    // so a slider drag never remounts the control under the pointer.
    let layer_id = selected_transform_memo(state);
    move || match layer_id.get() {
        Some(id) => view! { <LayerRotateControls state=state layer_id=id/> }.into_view(),
        None => view! { <PhotoRotateControls state=state/> }.into_view(),
    }
}

#[component]
fn PhotoRotateControls(state: AppState) -> impl IntoView {
    let fine = move || state.current().map(|m| m.edit.fine_angle).unwrap_or(0.0);
    view! {
        <div class="row">
            <button class="btn" on:click=move |_| state.update_current(|e| {
                e.rot90 = (e.rot90 + 3) % 4;
                e.crop = state::CropRect::default();
            })>"⟲ 90°"</button>
            <button class="btn" on:click=move |_| state.update_current(|e| {
                e.rot90 = (e.rot90 + 1) % 4;
                e.crop = state::CropRect::default();
            })>"⟳ 90°"</button>
        </div>
        <label class="slider">
            <span>"Straighten: " {move || format!("{:.1}°", fine())}</span>
            <input
                type="range" min="-10" max="10" step="0.1"
                prop:value=move || fine().to_string()
                on:input=move |ev| {
                    let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                    state.update_current(|e| e.fine_angle = v);
                }
            />
        </label>
        <button class="btn dim-btn" on:click=move |_| state.update_current(|e| {
            e.fine_angle = 0.0;
            e.rot90 = 0;
        })>"Reset rotation"</button>
    }
}

/// Values the layer Rotate tab reads and writes, whatever the layer kind.
fn xform_of(kind: &LayerKind) -> Option<(f32, f32, bool)> {
    match kind {
        LayerKind::Text(t) => Some((t.angle, t.font_size, true)),
        LayerKind::Path(p) => Some((p.angle, p.scale, false)),
        LayerKind::Brush(b) => Some((b.angle, b.scale, false)),
        LayerKind::Raster(r) => Some((r.angle, r.scale, false)),
        LayerKind::Adjust(_) => None,
    }
}

fn set_xform(layer: &mut Layer, angle: f32, size: f32) {
    match &mut layer.kind {
        LayerKind::Text(t) => {
            t.angle = angle;
            t.font_size = size;
        }
        LayerKind::Path(p) => {
            p.angle = angle;
            p.scale = size;
        }
        LayerKind::Brush(b) => {
            b.angle = angle;
            b.scale = size;
        }
        LayerKind::Raster(r) => {
            r.angle = angle;
            r.scale = size;
        }
        LayerKind::Adjust(_) => {}
    }
}

#[component]
fn LayerRotateControls(state: AppState, layer_id: usize) -> impl IntoView {
    let kind = move || {
        state
            .current_layers()
            .into_iter()
            .find(|l| l.id == layer_id)
            .and_then(|l| xform_of(&l.kind))
    };
    let is_text = move || kind().map(|(_, _, text)| text).unwrap_or(false);
    let angle = move || kind().map(|(a, _, _)| a).unwrap_or(0.0);
    let size = move || kind().map(|(_, s, _)| s).unwrap_or(1.0);
    let size_range = move || if is_text() { ("0.01", "0.3", "0.005") } else { ("0.05", "5", "0.01") };
    view! {
        <label class="slider">
            <span>"Angle: " {move || format!("{:.0}°", angle())}</span>
            <input
                type="range" min="-180" max="180" step="1"
                prop:value=move || angle().to_string()
                on:input=move |ev| {
                    let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                    with_layer(state, layer_id, move |l| {
                        let s = size_of(l);
                        set_xform(l, v, s);
                    });
                }
            />
        </label>
        <label class="slider">
            <span>{move || if is_text() { "Size: " } else { "Scale: " }}
                {move || if is_text() { format!("{:.1}%", size() * 100.0) } else { format!("{:.0}%", size() * 100.0) }}</span>
            <input
                type="range"
                min=move || size_range().0
                max=move || size_range().1
                step=move || size_range().2
                prop:value=move || size().to_string()
                on:input=move |ev| {
                    let v: f32 = event_target_value(&ev).parse().unwrap_or(1.0);
                    with_layer(state, layer_id, move |l| {
                        let a = angle_of(l);
                        set_xform(l, a, v);
                    });
                }
            />
        </label>
        <button class="btn dim-btn" on:click=move |_| {
            with_layer(state, layer_id, |l| {
                let keep = if matches!(l.kind, LayerKind::Text(_)) { size_of(l) } else { 1.0 };
                set_xform(l, 0.0, keep);
            });
        }>
            {move || if is_text() { "Reset rotation" } else { "Reset transform" }}
        </button>
    }
}

fn angle_of(l: &Layer) -> f32 {
    xform_of(&l.kind).map(|(a, _, _)| a).unwrap_or(0.0)
}

fn size_of(l: &Layer) -> f32 {
    xform_of(&l.kind).map(|(_, s, _)| s).unwrap_or(1.0)
}

#[component]
pub(crate) fn ColorTab(state: AppState) -> impl IntoView {
    // With an adjustment layer selected the tab edits that layer. With an
    // image layer selected it edits an adjustment clipped to it, created on
    // arrival. The memos change only when the selection (or the clip layer
    // existing) does, so a slider drag never remounts the controls under the
    // pointer.
    let adjust_id = selected_adjust_memo(state);
    let raster_id = selected_raster_memo(state);
    let clip_id = clip_adjust_memo(state);
    create_effect(move |_| {
        if clip_id.get().is_none() {
            if let Some(rid) = raster_id.get() {
                ensure_clip_adjust(state, rid);
            }
        }
    });
    move || match adjust_id.get() {
        Some(id) => view! {
            <AdjustColorControls state=state layer_id=id/>
            <p class="dim">"Blur and grain apply to the photo only. This layer's mask is on the Layers tab."</p>
        }
        .into_view(),
        None => match clip_id.get() {
            Some(id) => view! {
                <div class="adjust-editor">
                    <div class="panel-title">"Clipped to this image layer"</div>
                    <AdjustColorControls state=state layer_id=id/>
                </div>
                <p class="dim">"Changes only this image layer, through an adjustment clipped to it. Blur and grain apply to the photo only."</p>
            }
            .into_view(),
            None => view! { <PhotoColorControls state=state/> }.into_view(),
        },
    }
}

#[component]
fn PhotoColorControls(state: AppState) -> impl IntoView {
    let channel = create_rw_signal(0usize); // 0=RGB, 1=R, 2=G, 3=B, shared by Levels + Curves
    let slider = move |label: &'static str,
                       get: fn(&EditParams) -> f32,
                       set: fn(&mut EditParams, f32)| {
        view! {
            <label class="slider">
                <span>{label} ": " {move || format!(
                    "{:+.0}%",
                    state.current().map(|m| get(&m.edit) * 100.0).unwrap_or(0.0)
                )}</span>
                <input
                    type="range" min="-1" max="1" step="0.01"
                    prop:value=move || state.current().map(|m| get(&m.edit)).unwrap_or(0.0).to_string()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                        state.update_current(|e| set(e, v));
                    }
                />
            </label>
        }
    };
    view! {
        {slider("Brightness", |e| e.brightness, |e, v| e.brightness = v)}
        {slider("Contrast", |e| e.contrast, |e, v| e.contrast = v)}
        {slider("Saturation", |e| e.saturation, |e, v| e.saturation = v)}
        {slider("Warmth", |e| e.warmth, |e, v| e.warmth = v)}
        <BlurPanel state=state />
        <LevelsPanel state=state channel=channel target=AdjustTarget::Global />
        <CurvesPanel state=state channel=channel target=AdjustTarget::Global />
        <GrainPanel state=state />
        <button class="btn dim-btn" on:click=move |_| state.update_current(|e| {
            e.brightness = 0.0; e.contrast = 0.0; e.saturation = 0.0; e.warmth = 0.0;
        })>"Reset color"</button>
    }
}

/// Whether an item gets the RAW tab: RAW sources, and any item carrying RAW
/// edits (a healed RAW becomes a raster but keeps its recipe). JPEGs have no
/// develop stage, so the tab stays hidden and their recipes never grow RAW
/// fields.
pub(crate) fn has_raw_controls(m: &MediaItem) -> bool {
    !m.edit.raw.is_identity() || get_photo(m.id).map(|p| p.raw).unwrap_or(false)
}

/// Linear-light RAW develop controls: the RAW tab.
#[component]
pub(crate) fn RawPanel(state: AppState) -> impl IntoView {
    let is_raw = move || state.current().map(|m| has_raw_controls(&m)).unwrap_or(false);
    let rslider = move |label: &'static str,
                        min: f64,
                        max: f64,
                        step: f64,
                        get: fn(&RawAdjust) -> f32,
                        set: fn(&mut RawAdjust, f32),
                        fmt: fn(f32) -> String| {
        view! {
            <label class="slider">
                <span>{label} ": " {move || {
                    let Some(m) = state.current() else { return "—".into() };
                    fmt(get(&m.edit.raw))
                }}</span>
                <input
                    type="range" min=min.to_string() max=max.to_string() step=step.to_string()
                    prop:value=move || state.current()
                        .map(|m| get(&m.edit.raw).to_string())
                        .unwrap_or_default()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                        state.update_current(|e| set(&mut e.raw, v));
                    }
                />
            </label>
        }
    };
    view! {
        <Show when=is_raw fallback=|| ()>
            <div class="raw-panel">
                <div class="panel-title">"RAW develop"</div>
                {rslider(
                    "Temperature", 2500.0, 9500.0, 50.0,
                    |r| r.temp, |r, v| r.temp = v,
                    |v| format!("{v:.0} K"),
                )}
                {rslider(
                    "Tint", -100.0, 100.0, 1.0,
                    |r| r.tint, |r, v| r.tint = v,
                    |v| format!("{v:+.0}"),
                )}
                {rslider(
                    "Exposure", -5.0, 5.0, 0.05,
                    |r| r.exposure, |r, v| r.exposure = v,
                    |v| format!("{v:+.2} EV"),
                )}
                {rslider(
                    "Highlights", 0.0, 100.0, 1.0,
                    |r| r.highlights, |r, v| r.highlights = v,
                    |v| format!("{v:.0}"),
                )}
                {rslider(
                    "Shadows", 0.0, 100.0, 1.0,
                    |r| r.shadows, |r, v| r.shadows = v,
                    |v| format!("{v:.0}"),
                )}
                {rslider(
                    "Shadow NR", 0.0, 100.0, 1.0,
                    |r| r.noise, |r, v| r.noise = v,
                    |v| format!("{v:.0}"),
                )}
                {rslider(
                    "Smart denoise", 0.0, 100.0, 1.0,
                    |r| r.denoise, |r, v| r.denoise = v,
                    |v| format!("{v:.0}"),
                )}
                {rslider(
                    "Demosaic sharpening", 0.0, 100.0, 1.0,
                    |r| r.sharpen, |r, v| r.sharpen = v,
                    |v| format!("{v:.0}"),
                )}
                <button class="btn dim-btn" on:click=move |_| {
                    state.update_current(|e| e.raw = RawAdjust::default());
                }>"Reset RAW"</button>
            </div>
        </Show>
    }
}

#[component]
pub(crate) fn BlurPanel(state: AppState) -> impl IntoView {
    let blur = move || state.current().map(|m| m.edit.blur).unwrap_or(0.0);
    view! {
        <div class="blur-panel">
            <label class="slider">
                <span>"Blur: " {move || format!("{:.0}", blur())}</span>
                <input
                    type="range" min="0" max="100" step="1"
                    prop:value=move || blur().to_string()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                        state.update_current(|e| e.blur = v.clamp(0.0, 100.0));
                    }
                />
            </label>
        </div>
    }
}

/// Histogram fill for a levels channel: 0=RGB, 1=R, 2=G, 3=B.
pub(crate) fn channel_color(ch: usize) -> &'static str {
    match ch {
        1 => "rgba(255,69,58,0.85)",
        2 => "rgba(48,209,88,0.85)",
        3 => "rgba(10,132,255,0.9)",
        _ => "rgba(255,255,255,0.75)",
    }
}

/// Curve stroke for a curves channel (RGB/R/G/B).
pub(crate) fn curve_color(ch: usize, alpha: f64) -> String {
    let (r, g, b) = match ch {
        1 => (255, 69, 58),
        2 => (48, 209, 88),
        3 => (10, 132, 255),
        _ => (242, 242, 247),
    };
    format!("rgba({r},{g},{b},{alpha})")
}

/// Which settings object a Levels/Curves panel edits: the photo's global
/// colour stage or one adjustment layer's effect set.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum AdjustTarget {
    Global,
    Layer(usize),
}

pub(crate) fn levels_of(state: AppState, target: AdjustTarget) -> Option<levels::LevelsSettings> {
    match target {
        AdjustTarget::Global => state.current().map(|m| m.edit.levels),
        AdjustTarget::Layer(id) => state.current_layers().into_iter().find_map(|l| match l.kind {
            LayerKind::Adjust(a) if l.id == id => Some(a.adjust.levels),
            _ => None,
        }),
    }
}

pub(crate) fn with_levels(
    state: AppState,
    target: AdjustTarget,
    f: impl FnOnce(&mut levels::LevelsSettings),
) {
    match target {
        AdjustTarget::Global => state.update_current(|e| f(&mut e.levels)),
        AdjustTarget::Layer(id) => state.update_current_item(|m| {
            if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                if let LayerKind::Adjust(a) = &mut l.kind {
                    f(&mut a.adjust.levels);
                }
            }
        }),
    }
}

pub(crate) fn curves_of(state: AppState, target: AdjustTarget) -> Option<curves::CurvesSettings> {
    match target {
        AdjustTarget::Global => state.current().map(|m| m.edit.curves),
        AdjustTarget::Layer(id) => state.current_layers().into_iter().find_map(|l| match l.kind {
            LayerKind::Adjust(a) if l.id == id => Some(a.adjust.curves),
            _ => None,
        }),
    }
}

pub(crate) fn with_curves(
    state: AppState,
    target: AdjustTarget,
    f: impl FnOnce(&mut curves::CurvesSettings),
) {
    match target {
        AdjustTarget::Global => state.update_current(|e| f(&mut e.curves)),
        AdjustTarget::Layer(id) => state.update_current_item(|m| {
            if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                if let LayerKind::Adjust(a) = &mut l.kind {
                    f(&mut a.adjust.curves);
                }
            }
        }),
    }
}

#[component]
pub(crate) fn LevelsPanel(
    state: AppState,
    channel: RwSignal<usize>,
    target: AdjustTarget,
) -> impl IntoView {
    let hist_ref = create_node_ref::<html::Canvas>();
    // Which Auto button produced the current settings. Cleared as soon as the
    // levels are edited by hand or the selection moves to another item.
    let last_auto = create_rw_signal(None::<levels::AutoMode>);
    create_effect(move |_| {
        state.selected.track();
        last_auto.set(None);
    });

    // Redraw the histogram whenever the photo, its edits, or the channel change.
    create_effect(move |_| {
        state.items.track();
        state.selected.track();
        channel.track();
        let Some(canvas) = hist_ref.get() else { return };
        let Some(item) = state.current() else { return };
        if item.kind != MediaKind::Photo {
            return;
        }
        let Some(photo) = get_photo(item.id) else { return };
        let ch = channel.get_untracked();
        let bins = levels::histogram(&photo.preview.0);
        let scale = levels::histogram_display_scale(&bins[ch]);
        let ctx = web::ctx2d(&canvas);
        ctx.clear_rect(0.0, 0.0, 256.0, 64.0);
        if scale <= 0.0 {
            return;
        }
        ctx.set_fill_style_str(channel_color(ch));
        for (i, &b) in bins[ch].iter().enumerate() {
            let h = (b / scale * 64.0).min(64.0);
            if h >= 0.5 {
                ctx.fill_rect(i as f64, 64.0 - h, 1.0, h);
            }
        }
    });

    let dirty = move |idx: usize| {
        levels_of(state, target)
            .map(|l| {
                let r = &l.ranges;
                if idx == 0 {
                    (1..4).any(|ch| r[ch] != levels::LevelRange::default())
                } else {
                    r[idx] != levels::LevelRange::default()
                }
            })
            .unwrap_or(false)
    };

    let channel_btn = move |idx: usize, label: &'static str| {
        view! {
            <button
                class=move || format!(
                    "btn small chan-{idx}{}",
                    if channel.get() == idx { " active" } else { "" }
                )
                on:click=move |_| channel.set(idx)
            >
                {label}
                <span class=move || if dirty(idx) { "chan-dot on" } else { "chan-dot" }></span>
            </button>
        }
    };

    let lslider = move |label: &'static str,
                        min: f64,
                        max: f64,
                        step: f64,
                        get: fn(&levels::LevelRange) -> f64,
                        set: fn(&mut levels::LevelRange, f64),
                        fmt: fn(f64) -> String| {
        view! {
            <label class="slider">
                <span>{label} ": " {move || {
                    let Some(levels) = levels_of(state, target) else { return "—".into() };
                    let ch = channel.get();
                    let ranges = &levels.ranges;
                    let mut s = fmt(get(&ranges[ch]));
                    // In the RGB view, show the per-channel values once any
                    // channel differs from its default, so Auto color/neutral
                    // visibly move something even when they leave ranges[0] alone.
                    if ch == 0 && (1..4).any(|i| ranges[i] != levels::LevelRange::default()) {
                        s.push_str(&format!(
                            " · R {} G {} B {}",
                            fmt(get(&ranges[1])),
                            fmt(get(&ranges[2])),
                            fmt(get(&ranges[3])),
                        ));
                    }
                    s
                }}</span>
                <input
                    type="range" min=min.to_string() max=max.to_string() step=step.to_string()
                    prop:value=move || levels_of(state, target)
                        .map(|l| get(&l.ranges[channel.get()]).to_string())
                        .unwrap_or_else(|| "0".into())
                    on:input=move |ev| {
                        let v: f64 = event_target_value(&ev).parse().unwrap_or(0.0);
                        let ch = channel.get_untracked();
                        with_levels(state, target, |l| set(&mut l.ranges[ch], v));
                        last_auto.set(None);
                    }
                />
            </label>
        }
    };

    let auto = move |mode: levels::AutoMode| {
        let Some(item) = state.current() else { return };
        let Some(photo) = get_photo(item.id) else { return };
        let hist = levels::histogram(&photo.preview.0);
        let s = levels::auto_settings(&hist, mode);
        with_levels(state, target, |l| *l = s);
        last_auto.set(Some(mode));
    };

    let auto_btn = move |mode: levels::AutoMode, label: &'static str| {
        view! {
            <button
                class=move || format!(
                    "btn small{}",
                    if last_auto.get() == Some(mode) { " active" } else { "" }
                )
                on:click=move |_| auto(mode)
            >{label}</button>
        }
    };

    let applied = move || {
        let Some(mode) = last_auto.get() else { return ().into_view() };
        let Some(levels) = levels_of(state, target) else { return ().into_view() };
        let r = &levels.ranges;
        let span = |i: usize| format!("{:.0}–{:.0}", r[i].black, r[i].white);
        let title = match mode {
            levels::AutoMode::Contrast => "Auto contrast applied",
            levels::AutoMode::Color => "Auto color applied",
            levels::AutoMode::ColorNeutral => "Auto neutral applied",
        };
        let detail = if mode == levels::AutoMode::Contrast {
            format!("black {:.0} · white {:.0}", r[0].black, r[0].white).into_view()
        } else {
            let gamma = if mode == levels::AutoMode::ColorNeutral {
                format!(
                    " · gamma {:.2} / {:.2} / {:.2}",
                    r[1].gamma, r[2].gamma, r[3].gamma
                )
            } else {
                String::new()
            };
            view! {
                <>
                    <span class="las-r">{format!("R {}", span(1))}</span>" · "
                    <span class="las-g">{format!("G {}", span(2))}</span>" · "
                    <span class="las-b">{format!("B {}", span(3))}</span>
                    {gamma}
                </>
            }.into_view()
        };
        view! {
            <div class="levels-auto-status">
                <span class="las-title">{title}</span>" · "{detail}
            </div>
        }.into_view()
    };

    view! {
        <div class="levels-panel">
            <div class="panel-title">"Levels"</div>
            <canvas node_ref=hist_ref width="256" height="64" class="levels-hist"></canvas>
            <div class="levels-channels">
                {channel_btn(0, "RGB")}{channel_btn(1, "R")}{channel_btn(2, "G")}{channel_btn(3, "B")}
            </div>
            {lslider("Black point", 0.0, 254.0, 1.0, |r| r.black, |r, v| r.black = v, |v| format!("{v:.0}"))}
            {lslider("Gamma", 0.1, 9.99, 0.01, |r| r.gamma, |r, v| r.gamma = v, |v| format!("{v:.2}"))}
            {lslider("White point", 1.0, 255.0, 1.0, |r| r.white, |r, v| r.white = v, |v| format!("{v:.0}"))}
            {lslider("Output black", 0.0, 255.0, 1.0, |r| r.output_black, |r, v| r.output_black = v, |v| format!("{v:.0}"))}
            {lslider("Output white", 0.0, 255.0, 1.0, |r| r.output_white, |r, v| r.output_white = v, |v| format!("{v:.0}"))}
            <div class="levels-auto">
                {auto_btn(levels::AutoMode::Contrast, "Auto contrast")}
                {auto_btn(levels::AutoMode::Color, "Auto color")}
                {auto_btn(levels::AutoMode::ColorNeutral, "Auto neutral")}
                <button class="btn small dim-btn" on:click=move |_| {
                    with_levels(state, target, |l| *l = levels::LevelsSettings::default());
                    last_auto.set(None);
                }>"Reset levels"</button>
            </div>
            {applied}
        </div>
    }
}

#[component]
pub(crate) fn CurvesPanel(
    state: AppState,
    channel: RwSignal<usize>,
    target: AdjustTarget,
) -> impl IntoView {
    let selected = create_rw_signal(None::<usize>);
    let dragging = store_value(None::<usize>);
    let canvas_ref = create_node_ref::<html::Canvas>();

    // Redraw the curve whenever the photo's edits, channel, or selection change.
    create_effect(move |_| {
        state.items.track();
        channel.track();
        selected.track();
        let Some(canvas) = canvas_ref.get() else { return };
        let Some(curves_cfg) = curves_of(state, target) else { return };
        let ch = channel.get_untracked();
        let sel = selected.get_untracked();
        let pts = &curves_cfg.channels[ch];
        let ctx = web::ctx2d(&canvas);
        ctx.clear_rect(0.0, 0.0, 256.0, 256.0);
        ctx.set_stroke_style_str("rgba(255,255,255,0.12)");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        for i in 0..=4 {
            let f = i as f64 / 4.0 * 256.0;
            ctx.move_to(f, 0.0);
            ctx.line_to(f, 256.0);
            ctx.move_to(0.0, f);
            ctx.line_to(256.0, f);
        }
        ctx.stroke();
        // Other channels as faint ghosts so an edit on one channel is
        // distinguishable from the untouched ones.
        for other in 0..4 {
            if other == ch {
                continue;
            }
            ctx.set_stroke_style_str(&curve_color(other, 0.22));
            ctx.set_line_width(1.0);
            ctx.begin_path();
            for x in 0..=255 {
                let y = curves_cfg.value(x as f64, other);
                let px = x as f64 / 255.0 * 256.0;
                let py = (1.0 - y / 255.0) * 256.0;
                if x == 0 {
                    ctx.move_to(px, py);
                } else {
                    ctx.line_to(px, py);
                }
            }
            ctx.stroke();
        }
        ctx.set_stroke_style_str(&curve_color(ch, 1.0));
        ctx.set_line_width(2.0);
        ctx.begin_path();
        for x in 0..=255 {
            let y = curves_cfg.value(x as f64, ch);
            let px = x as f64 / 255.0 * 256.0;
            let py = (1.0 - y / 255.0) * 256.0;
            if x == 0 {
                ctx.move_to(px, py);
            } else {
                ctx.line_to(px, py);
            }
        }
        ctx.stroke();
        for (i, pt) in pts.iter().enumerate() {
            let px = pt.x / 255.0 * 256.0;
            let py = (1.0 - pt.y / 255.0) * 256.0;
            ctx.begin_path();
            let _ = ctx.arc(px, py, 4.0, 0.0, std::f64::consts::TAU);
            if sel == Some(i) {
                ctx.set_fill_style_str("#ffffff");
                ctx.fill();
                ctx.set_stroke_style_str(&curve_color(ch, 1.0));
                ctx.set_line_width(2.0);
                ctx.stroke();
            } else {
                ctx.set_fill_style_str(&curve_color(ch, 1.0));
                ctx.fill();
            }
        }
    });

    let to_curve = move |ev: &web_sys::PointerEvent| -> Option<(f64, f64)> {
        let canvas = canvas_ref.get()?;
        let rect = canvas.get_bounding_client_rect();
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return None;
        }
        let x = ((ev.client_x() as f64 - rect.left()) / rect.width() * 255.0).clamp(0.0, 255.0);
        let y = (255.0 - (ev.client_y() as f64 - rect.top()) / rect.height() * 255.0).clamp(0.0, 255.0);
        Some((x, y))
    };

    let on_down = move |ev: web_sys::PointerEvent| {
        ev.prevent_default();
        let Some((x, y)) = to_curve(&ev) else { return };
        if let Some(canvas) = canvas_ref.get() {
            let _ = canvas.set_pointer_capture(ev.pointer_id());
        }
        let ch = channel.get_untracked();
        let Some(curves_cfg) = curves_of(state, target) else { return };
        let pts = curves_cfg.channels[ch].clone();
        let nearest = pts
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                (a.x - x).hypot(a.y - y).partial_cmp(&(b.x - x).hypot(b.y - y)).unwrap()
            })
            .filter(|(_, p)| (p.x - x).hypot(p.y - y) < 14.0)
            .map(|(i, _)| i);
        if let Some(i) = nearest {
            dragging.set_value(Some(i));
            selected.set(Some(i));
        } else if pts.len() < 32 && x > 1.0 && x < 254.0 && pts.iter().all(|p| (p.x - x).abs() > 1.0) {
            let mut new_pts = pts.clone();
            new_pts.push(curves::CurvePoint { x, y });
            new_pts.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap());
            let idx = new_pts.iter().position(|p| p.x == x).unwrap();
            with_curves(state, target, |c| c.channels[ch] = new_pts);
            dragging.set_value(Some(idx));
            selected.set(Some(idx));
        }
    };

    let on_move = move |ev: web_sys::PointerEvent| {
        let Some(i) = dragging.get_value() else { return };
        let Some((x, y)) = to_curve(&ev) else { return };
        let ch = channel.get_untracked();
        with_curves(state, target, |c| {
            let pts = &mut c.channels[ch];
            if i >= pts.len() {
                return;
            }
            pts[i].y = y;
            if i > 0 && i < pts.len() - 1 {
                pts[i].x = x.min(pts[i + 1].x - 1.0).max(pts[i - 1].x + 1.0);
            }
        });
    };

    let on_up = move |_: web_sys::PointerEvent| dragging.set_value(None);

    let dirty = move |idx: usize| {
        curves_of(state, target)
            .map(|c| {
                if idx == 0 {
                    (1..4).any(|ch| !c.channel_is_identity(ch))
                } else {
                    !c.channel_is_identity(idx)
                }
            })
            .unwrap_or(false)
    };

    let channel_btn = move |idx: usize, label: &'static str| {
        view! {
            <button
                class=move || format!(
                    "btn small chan-{idx}{}",
                    if channel.get() == idx { " active" } else { "" }
                )
                on:click=move |_| {
                    channel.set(idx);
                    selected.set(None);
                    dragging.set_value(None);
                }
            >
                {label}
                <span class=move || if dirty(idx) { "chan-dot on" } else { "chan-dot" }></span>
            </button>
        }
    };

    let removable = move || {
        let Some(i) = selected.get() else { return false };
        curves_of(state, target)
            .map(|c| {
                let n = c.channels[channel.get()].len();
                i > 0 && i < n - 1
            })
            .unwrap_or(false)
    };

    view! {
        <div class="curves-panel">
            <div class="panel-title">"Curves"</div>
            <div class="levels-channels">
                {channel_btn(0, "RGB")}{channel_btn(1, "R")}{channel_btn(2, "G")}{channel_btn(3, "B")}
            </div>
            <canvas
                node_ref=canvas_ref width="256" height="256" class="curves-canvas"
                on:pointerdown=on_down
                on:pointermove=on_move
                on:pointerup=on_up
            ></canvas>
            <div class="curves-info">
                <span>{move || {
                    let ch = channel.get();
                    let detail = selected.get().and_then(|i| {
                        curves_of(state, target).and_then(|c| {
                            c.channels[ch]
                                .get(i)
                                .map(|p| format!("Input {} · Output {}", p.x as i32, p.y as i32))
                        })
                    })
                    .unwrap_or_else(|| "Click to add a point. Drag to adjust.".into());
                    format!("{} — {detail}", ["RGB", "R", "G", "B"][ch])
                }}</span>
            </div>
            <div class="levels-auto">
                <button class="btn small" disabled=move || !removable() on:click=move |_| {
                    if let Some(i) = selected.get_untracked() {
                        let ch = channel.get_untracked();
                        with_curves(state, target, |c| {
                            let pts = &mut c.channels[ch];
                            if i > 0 && i < pts.len() - 1 {
                                pts.remove(i);
                            }
                        });
                        selected.set(None);
                    }
                }>"Remove point"</button>
                <button class="btn small dim-btn" on:click=move |_| {
                    let ch = channel.get_untracked();
                    with_curves(state, target, |c| {
                        c.channels[ch] = vec![
                            curves::CurvePoint { x: 0.0, y: 0.0 },
                            curves::CurvePoint { x: 255.0, y: 255.0 },
                        ];
                    });
                    selected.set(None);
                }>"Reset curve"</button>
            </div>
        </div>
    }
}

#[component]
pub(crate) fn GrainPanel(state: AppState) -> impl IntoView {
    let grain = move || state.current().map(|m| m.edit.grain).unwrap_or(0.0);
    let gaussian = move || state.current().map(|m| m.edit.grain_gaussian).unwrap_or(true);
    let mono = move || state.current().map(|m| m.edit.grain_mono).unwrap_or(true);
    view! {
        <div class="grain-panel">
            <label class="slider">
                <span>"Grain: " {move || format!("{:.0}", grain())}</span>
                <input
                    type="range" min="0" max="100" step="1"
                    prop:value=move || grain().to_string()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                        state.update_current(|e| e.grain = v.clamp(0.0, 100.0));
                    }
                />
            </label>
            <label class="row" style="justify-content:flex-start;gap:0.5rem">
                <input
                    type="checkbox"
                    prop:checked=gaussian
                    on:change=move |ev| state.update_current(|e| e.grain_gaussian = event_target_checked(&ev))
                />
                "Gaussian"
            </label>
            <label class="row" style="justify-content:flex-start;gap:0.5rem">
                <input
                    type="checkbox"
                    prop:checked=mono
                    on:change=move |ev| state.update_current(|e| e.grain_mono = event_target_checked(&ev))
                />
                "Monochromatic"
            </label>
        </div>
    }
}

#[component]
pub(crate) fn HealTab(state: AppState) -> impl IntoView {
    let modes = [
        (heal::HealMode::ContentAware, "Content-aware"),
        (heal::HealMode::SmoothFill, "Smooth fill"),
        (heal::HealMode::ProximityMatch, "Proximity match"),
    ];
    view! {
        <div class="heal-panel">
            <div class="chips">
                {modes.map(|(m, label)| {
                    let active = move || state.heal_mode.get() == m;
                    view! {
                        <button
                            class="chip"
                            class:active=active
                            on:click=move |_| state.heal_mode.set(m)
                        >
                            {label}
                        </button>
                    }
                })}
            </div>
            <BrushSizeSlider state=state value=state.heal_radius label="Brush size: " diameter=false max_frac=0.1/>
            <p class="dim">
                "Paint over a blemish to remove it. Content-aware clones a matching patch, \
                 smooth fill blends the surroundings, proximity match prefers nearby texture. \
                 Applies when you release."
            </p>
        </div>
    }
}

#[component]
pub(crate) fn CloneTab(state: AppState) -> impl IntoView {
    let has_source = move || state.clone_source.get().is_some();
    let picking = move || state.clone_pick.get();
    view! {
        <div class="clone-panel">
            <div class="row select-row">
                <button
                    class="chip"
                    class:active=picking
                    on:click=move |_| state.clone_pick.set(!state.clone_pick.get_untracked())
                >
                    "Set source"
                </button>
                <button
                    class="btn"
                    disabled=move || !has_source()
                    on:click=move |_| {
                        state.clone_source.set(None);
                        state.clone_offset.set(None);
                    }
                >
                    "Clear source"
                </button>
            </div>
            <label class="row" style="justify-content:flex-start;gap:0.5rem">
                <input
                    type="checkbox"
                    prop:checked=move || state.clone_aligned.get()
                    on:change=move |ev| {
                        state.clone_aligned.set(event_target_checked(&ev));
                        state.clone_offset.set(None);
                    }
                />
                "Aligned"
            </label>
            <BrushSizeSlider state=state value=state.clone_radius label="Brush size: " diameter=false max_frac=0.15/>
            <p class="dim">
                {move || if picking() {
                    "Click the canvas to set the clone source."
                } else if has_source() {
                    "Drag to paint from the source. Alt-click sets a new source. \
                     Aligned keeps the offset between strokes."
                } else {
                    "Set a source first: Alt-click the canvas, or tap Set source then click."
                }}
            </p>
        </div>
    }
}

#[component]
pub(crate) fn TrimTab(state: AppState) -> impl IntoView {
    let duration = move || {
        state
            .current()
            .and_then(|m| get_video(m.id).map(|(_, d, _, _)| d))
            .unwrap_or(0.0)
    };
    let trim = move || {
        state
            .current()
            .and_then(|m| m.edit.trim)
            .unwrap_or((0.0, duration() as f32))
    };
    view! {
        <label class="slider">
            <span>"Start: " {move || format!("{:.1}s", trim().0)}</span>
            <input type="range" min="0" max=move || duration().to_string() step="0.1"
                prop:value=move || trim().0.to_string()
                on:input=move |ev| {
                    let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                    let end = trim().1;
                    state.update_current(|e| e.trim = Some((v.min(end - 0.1).max(0.0), end)));
                }
            />
        </label>
        <label class="slider">
            <span>"End: " {move || format!("{:.1}s", trim().1)}</span>
            <input type="range" min="0" max=move || duration().to_string() step="0.1"
                prop:value=move || trim().1.to_string()
                on:input=move |ev| {
                    let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                    let start = trim().0;
                    state.update_current(|e| e.trim = Some((start, v.max(start + 0.1))));
                }
            />
        </label>
        <button class="btn dim-btn" on:click=move |_| state.update_current(|e| e.trim = None)>
            "Clear trim"
        </button>
    }
}

#[component]
pub(crate) fn ExportTab(state: AppState) -> impl IntoView {
    let multi = move || state.items.with(|v| v.len() > 1);
    let is_video = move || state.current().map(|m| m.kind == MediaKind::Video).unwrap_or(false);
    let is_photo = move || state.current().map(|m| m.kind == MediaKind::Photo).unwrap_or(false);
    let keep_audio = move || state.current().map(|m| m.edit.keep_audio).unwrap_or(true);
    let format = move || state.photo_format.get();
    let edited_count = move || state.items.with(|v| v.iter().filter(|m| item_edited(m)).count());
    let save_count = move || state.items.with(|v| v.iter().filter(|m| photo_needs_save(m)).count());
    let drive_ready = move || {
        state.drive_token.with(|t| t.is_some()) && state.drive_folder.with(|f| f.is_some())
    };
    view! {
        <button class="btn primary" on:click=move |_| {
            if let Some(item) = state.current() {
                match item.kind {
                    MediaKind::Photo => export_photo(state, item),
                    MediaKind::Video => export_video(state, item),
                }
            }
        }>"Download this"</button>
        <Show when=is_photo fallback=|| ()>
            <Show
                when=move || {
                    state.drive_token.with(|t| t.is_some())
                        && state.drive_folder.with(|f| f.is_some())
                }
                fallback=|| ()
            >
                <button class="btn" on:click=move |_| {
                    if let Some(item) = state.current() {
                        if item.kind == MediaKind::Photo {
                            save_photo_to_drive(state, item);
                        }
                    }
                }>"Save to Drive"</button>
            </Show>
            <label class="row" style="justify-content:flex-start;gap:0.5rem">
                "Format"
                <select
                    on:change=move |ev| {
                        let f = match event_target_value(&ev).as_str() {
                            "png" => PhotoFormat::Png,
                            "jpeg100" => PhotoFormat::JpegMax,
                            _ => PhotoFormat::Jpeg,
                        };
                        state.photo_format.set(f);
                    }
                >
                    <option value="jpeg" selected=move || format() == PhotoFormat::Jpeg>"JPEG"</option>
                    <option value="jpeg100" selected=move || format() == PhotoFormat::JpegMax>"JPEG (max quality)"</option>
                    <option value="png" selected=move || format() == PhotoFormat::Png>"PNG (lossless)"</option>
                </select>
            </label>
        </Show>
        <Show when=is_video fallback=|| ()>
            <label class="row" style="justify-content:flex-start;gap:0.5rem">
                <input
                    type="checkbox"
                    prop:checked=keep_audio
                    on:change=move |ev| {
                        let checked = event_target_checked(&ev);
                        state.update_current_item(|m| m.edit.keep_audio = checked);
                    }
                />
                "Keep original audio"
            </label>
        </Show>
        <Show when=multi fallback=|| ()>
            <button class="btn" on:click=move |_| {
                let Some(cur) = state.current() else { return };
                batch(|| {
                    state.items.update(|v| {
                        for m in v.iter_mut() {
                            if m.kind == cur.kind && m.id != cur.id {
                                m.edit = cur.edit.clone();
                            }
                        }
                    });
                });
            }>"Apply edits to all"</button>
            <button class="btn" disabled=move || edited_count() == 0 on:click=move |_| {
                download_edited_zip(state);
            }>{move || format!("Download {} edited (zip)", edited_count())}</button>
            <Show when=drive_ready fallback=|| ()>
                <button class="btn" disabled=move || save_count() == 0 on:click=move |_| {
                    save_edited_to_drive(state);
                }>{move || format!("Save {} edited to Drive", save_count())}</button>
            </Show>
        </Show>
        <p class="dim">"Exports match the selected aspect preset’s social-media dimensions."</p>
    }
}

#[cfg(test)]
mod tests {
    use super::{brush_log_pos, brush_log_size};

    #[test]
    fn brush_slider_is_logarithmic() {
        let (min, max) = (2.0, 2000.0);
        assert_eq!(brush_log_pos(min, min, max), 0.0);
        assert!((brush_log_pos(max, min, max) - 1.0).abs() < 1e-9);
        // The point of the log curve: 20 px is a third of the travel, where a
        // linear slider would bury it in the first percent.
        let p = brush_log_pos(20.0, min, max);
        assert!((0.2..0.4).contains(&p), "20 px sits at {p}");
    }

    #[test]
    fn brush_slider_round_trips_every_size() {
        let (min, max) = (2.0, 2000.0);
        for px in [2.0, 3.5, 7.5, 20.0, 240.0, 1999.0, 2000.0] {
            let back = brush_log_size(brush_log_pos(px, min, max), min, max);
            assert!((back - px).abs() < 1e-6, "{px} -> {back}");
        }
    }
}
