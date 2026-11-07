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

#[component]
pub(crate) fn SelectTab(state: AppState) -> impl IntoView {
    let has_selection =
        move || state.current().map(|m| m.edit.selection.is_some()).unwrap_or(false);
    let feather = move || {
        state
            .current()
            .and_then(|m| m.edit.selection)
            .map(|s| s.feather)
            .unwrap_or(0.0)
    };
    view! {
        <div class="select-tab">
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
                <label class="slider">
                    <span>"Size: " {move || format!("{:.1}%", state.brush_size.get() * 100.0)}</span>
                    <input
                        type="range" min="0.005" max="0.3" step="0.005"
                        prop:value=move || state.brush_size.get().to_string()
                        on:input=move |ev| {
                            let v: f32 = event_target_value(&ev).parse().unwrap_or(0.04);
                            state.brush_size.set(v.clamp(0.005, 0.3));
                        }
                    />
                </label>
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
                 Cut/Copy lift the selection to a raster layer; Content fill synthesizes it from \
                 the surroundings. Isolate person runs MediaPipe Selfie Segmentation."
            </p>
        </div>
    }
}

#[component]
pub(crate) fn RotateTab(state: AppState) -> impl IntoView {
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

#[component]
pub(crate) fn ColorTab(state: AppState) -> impl IntoView {
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
    let radius = move || state.heal_radius.get();
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
            <label class="slider">
                <span>"Brush size: " {move || format!("{:.0}%", radius() * 100.0)}</span>
                <input
                    type="range" min="0.005" max="0.1" step="0.005"
                    prop:value=move || radius().to_string()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.02);
                        state.heal_radius.set(v.clamp(0.005, 0.1));
                    }
                />
            </label>
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
    let radius = move || state.clone_radius.get();
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
            <label class="slider">
                <span>"Brush size: " {move || format!("{:.0}%", radius() * 100.0)}</span>
                <input
                    type="range" min="0.005" max="0.15" step="0.005"
                    prop:value=move || radius().to_string()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.03);
                        state.clone_radius.set(v.clamp(0.005, 0.15));
                    }
                />
            </label>
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
