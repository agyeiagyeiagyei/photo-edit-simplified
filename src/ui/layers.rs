//! Layers tab and per-layer editors for text, vector paths, brush strokes and raster layers.

use crate::prelude::*;
use crate::state::{AdjustLayerState, PointAdjust};
use crate::*;

#[component]
pub(crate) fn LayersTab(state: AppState, tab: RwSignal<Tab>) -> impl IntoView {
    let add_layer = move |kind: &str| {
        if kind == "text" {
            ensure_fonts(state);
        }
        let mut new_id = None;
        let mut took_selection = false;
        state.update_current_item(|m| {
            let id = m.next_layer_id;
            m.next_layer_id += 1;
            let mut layer = match kind {
                "text" => {
                    let mut l = Layer::new_text(id);
                    // Place new text at the center of the current crop, not
                    // the full image, so it's visible in the cropped preview.
                    if let LayerKind::Text(t) = &mut l.kind {
                        let c = m.edit.crop;
                        t.x = c.x + c.w / 2.0;
                        t.y = c.y + c.h / 2.0;
                    }
                    l
                }
                "path" => Layer::new_path(id),
                "brush" => Layer::new_brush(id),
                "adjust" | "adjust-sel" => Layer::new_adjust(id),
                _ => Layer::new_text(id),
            };
            if kind == "adjust-sel" {
                if let Some(sel) = m.edit.selection.take() {
                    layer.set_mask(Some(sel));
                    took_selection = true;
                }
            }
            // New layers land on top: an adjustment above them still applies,
            // and one below doesn't — the same rule as dragging a row.
            m.layers.push(layer);
            new_id = Some(id);
        });
        if took_selection {
            state.subject_photo.set(None);
            clear_subject_brush_ops(state);
        }
        if let Some(id) = new_id {
            state.selected_layer.set(Some(id));
            state.selected_tool.set(match kind {
                "path" => Tool::Pen,
                "brush" => Tool::Brush,
                _ => Tool::Select,
            });
        }
    };

    let delete_layer = move |id: usize| {
        let mut orphaned = Vec::new();
        state.update_current_item(|m| {
            // An adjustment clipped to this layer has no target once it is
            // gone; leaving it would silently widen its edits to the stack.
            m.layers.retain(|l| {
                let clipped = matches!(&l.kind, LayerKind::Adjust(a) if a.clip_to == Some(id));
                if clipped {
                    orphaned.push(l.id);
                }
                l.id != id && !clipped
            });
        });
        if state.selected_layer.get().is_some_and(|sel| sel == id || orphaned.contains(&sel)) {
            state.selected_layer.set(None);
        }
        if state
            .mask_target
            .get()
            .is_some_and(|(_, layer)| layer == id || orphaned.contains(&layer))
        {
            state.mask_target.set(None);
        }
    };

    let move_layer = move |id: usize, delta: isize| {
        state.update_current_item(|m| {
            let Some(i) = m.layers.iter().position(|l| l.id == id) else { return };
            // One stack, so a row may land anywhere: an adjustment renders over
            // whatever sits below it after the move.
            let new_i = (i as isize + delta).clamp(0, m.layers.len() as isize - 1) as usize;
            if new_i != i {
                m.layers.swap(i, new_i);
            }
        });
    };

    let set_visible = move |id: usize, visible: bool| {
        state.update_current_item(|m| {
            if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                l.visible = visible;
            }
        });
    };

    let set_opacity = move |id: usize, opacity: f32| {
        state.update_current_item(|m| {
            if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                l.opacity = opacity;
            }
        });
    };

    let select_layer = move |id: usize| {
        state.selected_layer.set(Some(id));
        // The mask brush target is explicit: only the row thumbnail arms it.
        state.mask_target.set(None);
    };

    let click_mask = move |id: usize| {
        let targeted = state
            .mask_target
            .get()
            .is_some_and(|(_, layer)| layer == id);
        if targeted {
            state.mask_target.set(None);
            return;
        }
        let has_mask = state
            .current_layers()
            .into_iter()
            .find(|l| l.id == id)
            .is_some_and(|l| l.mask.is_some());
        if has_mask {
            let item_id = state.current().map(|m| m.id);
            state.selected_layer.set(Some(id));
            if let Some(item_id) = item_id {
                state.mask_target.set(Some((item_id, id)));
            }
        } else {
            mask_from_selection(state, id);
        }
    };

    let layers = create_memo(move |_| state.current_layers());
    // The stack is bottom-up, but a layers panel reads top-down: show the
    // topmost layer first.
    let display = create_memo(move |_| display_layers(layers.get()));
    // The adjustment editor mounts once per selected adjustment layer. Reading
    // the whole layer list in the mount closure rebuilt the editor on every
    // slider input, which swapped the <input> under the pointer and ended a
    // real drag after its first step.
    let selected_adjust_id = selected_adjust_memo(state);
    let thumb_url = create_memo(move |_| state.current().map(|m| m.thumb_url).unwrap_or_default());
    let is_photo = move || state.current().map(|m| m.kind == MediaKind::Photo).unwrap_or(false);
    let has_selection = move || state.current().map(|m| m.edit.selection.is_some()).unwrap_or(false);

    let image_input_ref = create_node_ref::<html::Input>();
    let import_image = move |file: File| {
        spawn_local(async move {
            let blob: &Blob = file.as_ref();
            let Ok((img, url)) = web::load_image(blob).await else { return };
            let (pixels, w, h) = downscale(&img, 2048);
            let _ = Url::revoke_object_url(&url);
            let mut new_id = None;
            state.update_current_item(|m| {
                let id = m.next_layer_id;
                m.next_layer_id += 1;
                let mut layer = Layer::new_raster(id, pixels, w, h);
                if let LayerKind::Raster(r) = &mut layer.kind {
                    r.scale = 0.5;
                }
                m.layers.push(layer);
                new_id = Some(id);
            });
            if let Some(id) = new_id {
                state.selected_layer.set(Some(id));
                state.selected_tool.set(Tool::Select);
            }
        });
    };

    view! {
        <div class="layers-tab">
            <div class="row layer-add-row">
                <button class="btn" on:click=move |_| add_layer("text")>"＋ Text"</button>
                <button class="btn" on:click=move |_| add_layer("path")>"＋ Path"</button>
                <button class="btn" on:click=move |_| add_layer("brush")>"＋ Brush"</button>
                <button
                    class="btn"
                    disabled=move || !is_photo()
                    on:click=move |_| add_layer("adjust")
                >"＋ Adjust"</button>
                <button
                    class="btn"
                    disabled=move || !is_photo() || !has_selection()
                    on:click=move |_| add_layer("adjust-sel")
                >"＋ Adjust from sel."</button>
                <label class="btn">
                    "＋ Image"
                    <input
                        node_ref=image_input_ref
                        type="file"
                        accept="image/*"
                        style="display:none"
                        on:change=move |_| {
                            if let Some(input) = image_input_ref.get() {
                                if let Some(files) = input.files() {
                                    if let Some(file) = files.item(0) {
                                        import_image(file);
                                    }
                                }
                                input.set_value("");
                            }
                        }
                    />
                </label>
            </div>
            <div class="layer-list">
                <For
                    each=move || display.get()
                    key=|l| l.id
                    children=move |l: Layer| {
                        let id = l.id;
                        let layer = create_memo(move |_| {
                            layers.get().into_iter().find(|l| l.id == id)
                        });
                        let is_selected = move || state.selected_layer.get() == Some(id);
                        let has_mask = move || layer.get().map(|l| l.mask.is_some()).unwrap_or(false);
                        let mask_targeted =
                            move || state.mask_target.get().is_some_and(|(_, lid)| lid == id);
                        let mask_canvas_ref = create_node_ref::<html::Canvas>();
                        create_effect(move |_| {
                            if let (Some(canvas), Some(l)) = (mask_canvas_ref.get(), layer.get()) {
                                draw_mask_thumb(&canvas, &l);
                            }
                        });
                        view! {
                            <div
                                class="layer-row"
                                class:selected=is_selected
                                on:click=move |_| select_layer(id)
                            >
                                <button
                                    class="icon-btn"
                                    on:click=move |ev| {
                                        ev.stop_propagation();
                                        let visible = layer.get().map(|l| l.visible).unwrap_or(true);
                                        set_visible(id, !visible);
                                    }
                                >
                                    {move || {
                                        if layer.get().map(|l| l.visible).unwrap_or(true) {
                                            "●"
                                        } else {
                                            "○"
                                        }
                                    }}
                                </button>
                                <span class="layer-label">
                                    {move || layers.with(|ls| row_label(ls, id, false))}
                                </span>
                                <button
                                    class="icon-btn mask-thumb"
                                    class:has-mask=has_mask
                                    class:targeted=mask_targeted
                                    title="Add or edit this layer's mask"
                                    on:click=move |ev| {
                                        ev.stop_propagation();
                                        click_mask(id);
                                    }
                                >
                                    <canvas node_ref=mask_canvas_ref width="24" height="18"></canvas>
                                </button>
                                <input
                                    type="range"
                                    min="0"
                                    max="1"
                                    step="0.01"
                                    prop:value=move || {
                                        layer.get().map(|l| l.opacity.to_string()).unwrap_or_else(|| "1".into())
                                    }
                                    on:input=move |ev| {
                                        let v: f32 = event_target_value(&ev).parse().unwrap_or(1.0);
                                        set_opacity(id, v.clamp(0.0, 1.0));
                                    }
                                />
                                <button
                                    class="icon-btn"
                                    disabled=move || !can_move_layer(&layers.get(), id, 1)
                                    on:click=move |ev| {
                                        ev.stop_propagation();
                                        move_layer(id, 1);
                                    }
                                >
                                    "↑"
                                </button>
                                <button
                                    class="icon-btn"
                                    disabled=move || !can_move_layer(&layers.get(), id, -1)
                                    on:click=move |ev| {
                                        ev.stop_propagation();
                                        move_layer(id, -1);
                                    }
                                >
                                    "↓"
                                </button>
                                <button
                                    class="icon-btn delete"
                                    on:click=move |ev| {
                                        ev.stop_propagation();
                                        delete_layer(id);
                                    }
                                >
                                    "✕"
                                </button>
                            </div>
                        }
                    }
                />
            </div>
            // The photo is the bottom of the stack, pinned under the scrolling
            // list so it is always in view. It can't be moved or deleted;
            // selecting it means no layer is selected, so the editing tabs
            // edit the photo.
            <div
                class="photo-row"
                class:selected=move || state.selected_layer.get().is_none()
                on:click=move |_| state.selected_layer.set(None)
            >
                <img class="layer-thumb" src=move || thumb_url.get() alt=""/>
                <span class="layer-label">"Photo"</span>
            </div>
            <ToolBar state=state tab=tab/>
            {move || selected_adjust_id
                .get()
                .map(|id| view! { <AdjustLayerEditor state=state layer_id=id/> })}
            {move || state.selected_layer.get().map(|id| view! {
                <MaskSection state=state layer_id=id/>
            })}
            <TextLayerEditor state=state/>
            <PathLayerEditor state=state/>
            <BrushLayerEditor state=state/>
            <RasterLayerEditor state=state/>
        </div>
    }
}

/// Panel order: topmost first. The stack itself is bottom-up (index 0
/// composites first), so the whole list is reversed for display.
fn display_layers(ls: Vec<Layer>) -> Vec<Layer> {
    ls.into_iter().rev().collect()
}

/// The selected layer's id when it is an adjustment layer. Editors mount from
/// this memo, which only changes when the selection does: reading the whole
/// layer list in a mount closure rebuilt the editor on every slider input,
/// which swapped the <input> under the pointer and ended a real drag after
/// its first step.
pub(crate) fn selected_adjust_memo(state: AppState) -> Memo<Option<usize>> {
    create_memo(move |_| {
        let id = state.selected_layer.get()?;
        state
            .current()?
            .layers
            .iter()
            .any(|l| l.id == id && matches!(l.kind, LayerKind::Adjust(_)))
            .then_some(id)
    })
}

/// Coarse layer class: enough to decide which editing tabs can act on a layer
/// without copying its payload into the memo.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayerClass {
    Text,
    Path,
    Brush,
    Raster,
    Adjust,
}

impl LayerClass {
    pub(crate) fn of(kind: &LayerKind) -> Self {
        match kind {
            LayerKind::Text(_) => LayerClass::Text,
            LayerKind::Path(_) => LayerClass::Path,
            LayerKind::Brush(_) => LayerClass::Brush,
            LayerKind::Raster(_) => LayerClass::Raster,
            LayerKind::Adjust(_) => LayerClass::Adjust,
        }
    }
}

/// The selected layer's id when the Rotate tab can transform it. Same
/// selection-only memo rule as `selected_adjust_memo`: the controls read the
/// layer's values reactively, and only a selection change remounts them.
pub(crate) fn selected_transform_memo(state: AppState) -> Memo<Option<usize>> {
    create_memo(move |_| {
        let id = state.selected_layer.get()?;
        state
            .current()?
            .layers
            .iter()
            .any(|l| l.id == id && l.kind.has_transform())
            .then_some(id)
    })
}

/// The selected layer's id when it is an image (raster) layer.
pub(crate) fn selected_raster_memo(state: AppState) -> Memo<Option<usize>> {
    create_memo(move |_| {
        let id = state.selected_layer.get()?;
        state
            .current()?
            .layers
            .iter()
            .any(|l| l.id == id && matches!(l.kind, LayerKind::Raster(_)))
            .then_some(id)
    })
}

/// The adjustment layer clipped to the selected image layer, if one exists.
/// Stable while that layer's values change, so the Color sliders stay mounted
/// through a drag.
pub(crate) fn clip_adjust_memo(state: AppState) -> Memo<Option<usize>> {
    create_memo(move |_| {
        let rid = selected_raster_memo(state).get()?;
        state.current_layers().into_iter().find_map(|l| match l.kind {
            LayerKind::Adjust(a) if a.clip_to == Some(rid) => Some(l.id),
            _ => None,
        })
    })
}

/// Colour on an image layer: make the adjustment clipped to it if it doesn't
/// exist yet, so the Color tab has somewhere to write. Inserted directly above
/// the image so it applies to that layer and nothing above it.
pub(crate) fn ensure_clip_adjust(state: AppState, raster_id: usize) -> Option<usize> {
    let mut made = None;
    state.update_current_item(|m| {
        if m.layers
            .iter()
            .any(|l| matches!(&l.kind, LayerKind::Adjust(a) if a.clip_to == Some(raster_id)))
        {
            return;
        }
        let Some(ri) = m.layers.iter().position(|l| l.id == raster_id) else { return };
        let id = m.next_layer_id;
        m.next_layer_id += 1;
        let mut layer = Layer::new_adjust(id);
        if let LayerKind::Adjust(a) = &mut layer.kind {
            a.clip_to = Some(raster_id);
        }
        m.layers.insert(ri + 1, layer);
        made = Some(id);
    });
    made
}

/// What the editing tabs act on: the photo, or the selected layer.
#[derive(Clone, PartialEq)]
pub(crate) enum EditTarget {
    Photo,
    Layer { id: usize, label: String, class: LayerClass },
}

pub(crate) fn edit_target_memo(state: AppState) -> Memo<EditTarget> {
    create_memo(move |_| {
        let Some(id) = state.selected_layer.get() else { return EditTarget::Photo };
        let Some(m) = state.current() else { return EditTarget::Photo };
        let Some(l) = m.layers.iter().find(|l| l.id == id) else { return EditTarget::Photo };
        let label = row_label(&m.layers, id, true);
        EditTarget::Layer { id, label, class: LayerClass::of(&l.kind) }
    })
}

/// "Editing: …" line at the top of the editing tabs, so a tab never edits the
/// photo while a layer is selected without saying so. `supports` says which
/// layer classes the tab can act on; `photo_note` completes "<layer> is
/// selected, but …" for the classes it can't.
#[component]
pub(crate) fn EditingLine(
    state: AppState,
    photo_note: &'static str,
    supports: fn(LayerClass) -> bool,
) -> impl IntoView {
    let target = edit_target_memo(state);
    view! {
        <div class="editing-line">
            {move || match target.get() {
                EditTarget::Layer { label, class, .. } if supports(class) => view! {
                    <span>"Editing: " <b>{label}</b></span>
                    <button class="link-btn" on:click=move |_| state.selected_layer.set(None)>
                        "Edit the photo instead"
                    </button>
                }
                .into_view(),
                EditTarget::Photo => view! { <span>"Editing: " <b>"Photo"</b></span> }.into_view(),
                EditTarget::Layer { label, .. } => view! {
                    <span>"Editing: " <b>"Photo"</b></span>
                    <span class="dim">{format!("{label} is selected, but {photo_note}.")}</span>
                }
                .into_view(),
            }}
        </div>
    }
}

/// Row label. Layers of the same kind (other than text, which shows its own
/// words) get a number in creation order, so two adjustments read "Adjust 1"
/// and "Adjust 2" and a reorder is visible. The row's mask thumbnail shows a
/// mask; with `mask_note` (the Editing line) the label says so in words.
/// The row label without the mask note, for other tabs that name a layer.
pub(crate) fn layer_row_label(ls: &[Layer], id: usize) -> String {
    row_label(ls, id, false)
}

fn row_label(ls: &[Layer], id: usize, mask_note: bool) -> String {
    let Some(layer) = ls.iter().find(|l| l.id == id) else {
        return String::new();
    };
    let mut label = layer.label();
    if !matches!(layer.kind, LayerKind::Text(_)) {
        let same = |l: &&Layer| std::mem::discriminant(&l.kind) == std::mem::discriminant(&layer.kind);
        if ls.iter().filter(same).count() > 1 {
            let n = ls.iter().filter(same).filter(|l| l.id <= id).count();
            label = format!("{label} {n}");
        }
    }
    if mask_note && layer.mask.is_some() {
        label.push_str(" · masked");
    }
    label
}

/// Whether a layer can move `delta` slots: one stack, so only the ends stop it.
fn can_move_layer(ls: &[Layer], id: usize, delta: isize) -> bool {
    let Some(i) = ls.iter().position(|l| l.id == id) else {
        return false;
    };
    let j = i as isize + delta;
    j >= 0 && j < ls.len() as isize
}

/// Give Pen/Brush a layer to draw on: keep a compatible selection, else select
/// the topmost compatible layer, else create one on top of the stack.
pub(crate) fn ensure_tool_layer(state: AppState, tool: Tool) {
    if !matches!(tool, Tool::Pen | Tool::Brush) {
        return;
    }
    let matching = |l: &Layer| {
        matches!(
            (tool, &l.kind),
            (Tool::Pen, LayerKind::Path(_)) | (Tool::Brush, LayerKind::Brush(_))
        )
    };
    let Some(item) = state.current() else { return };
    let selected_matches = state
        .selected_layer
        .get_untracked()
        .and_then(|id| item.layers.iter().find(|l| l.id == id))
        .map(matching)
        .unwrap_or(false);
    if selected_matches {
        return;
    }
    if let Some(id) = item.layers.iter().rev().find(|l| matching(l)).map(|l| l.id) {
        state.selected_layer.set(Some(id));
        return;
    }
    let mut new_id = None;
    state.update_current_item(|m| {
        let id = m.next_layer_id;
        m.next_layer_id += 1;
        let layer = match tool {
            Tool::Pen => Layer::new_path(id),
            _ => Layer::new_brush(id),
        };
        m.layers.push(layer);
        new_id = Some(id);
    });
    if let Some(id) = new_id {
        state.selected_layer.set(Some(id));
    }
}

fn with_adjust(state: AppState, id: usize, f: impl FnOnce(&mut AdjustLayerState)) {
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
            if let LayerKind::Adjust(a) = &mut l.kind {
                f(a);
            }
        }
    });
}

fn adjust_of(state: AppState, id: usize) -> Option<AdjustLayerState> {
    state.current_layers().into_iter().find_map(|l| match l.kind {
        LayerKind::Adjust(a) if l.id == id => Some(a),
        _ => None,
    })
}

/// An adjustment layer's colour settings. The Layers tab's editor and the
/// Color tab (with that layer selected) both show these.
#[component]
pub(crate) fn AdjustColorControls(state: AppState, layer_id: usize) -> impl IntoView {
    let adjust = move || adjust_of(state, layer_id);
    let channel = create_rw_signal(0usize);
    let slider = move |label: &'static str,
                       get: fn(&PointAdjust) -> f32,
                       set: fn(&mut PointAdjust, f32)| {
        view! {
            <label class="slider">
                <span>{label} ": " {move || format!(
                    "{:+.0}%",
                    adjust().map(|a| get(&a.adjust) * 100.0).unwrap_or(0.0)
                )}</span>
                <input
                    type="range" min="-1" max="1" step="0.01"
                    prop:value=move || adjust().map(|a| get(&a.adjust)).unwrap_or(0.0).to_string()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                        with_adjust(state, layer_id, |a| set(&mut a.adjust, v));
                    }
                />
            </label>
        }
    };
    view! {
        {slider("Brightness", |a| a.brightness, |a, v| a.brightness = v)}
        {slider("Contrast", |a| a.contrast, |a, v| a.contrast = v)}
        {slider("Saturation", |a| a.saturation, |a, v| a.saturation = v)}
        {slider("Warmth", |a| a.warmth, |a, v| a.warmth = v)}
        <LevelsPanel state=state channel=channel target=AdjustTarget::Layer(layer_id)/>
        <CurvesPanel state=state channel=channel target=AdjustTarget::Layer(layer_id)/>
    }
}

#[component]
pub(crate) fn AdjustLayerEditor(state: AppState, layer_id: usize) -> impl IntoView {
    view! {
        <div class="adjust-editor">
            <div class="panel-title">"Adjustment layer"</div>
            <AdjustColorControls state=state layer_id=layer_id/>
        </div>
    }
}

/// Give a layer a mask from the global selection, or a reveal-all mask when
/// there is none. Taking the selection clears it globally, exactly like the
/// old adjustment-layer flow.
fn mask_from_selection(state: AppState, layer_id: usize) -> bool {
    let mut took = false;
    let mut item_id = None;
    state.update_current_item(|m| {
        item_id = Some(m.id);
        let Some(layer) = m.layers.iter_mut().find(|l| l.id == layer_id) else { return };
        let mask = match m.edit.selection.take() {
            Some(sel) => {
                took = true;
                sel
            }
            None => Selection {
                kind: SelectionKind::Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 },
                feather: 0.0,
            },
        };
        layer.set_mask(Some(mask));
    });
    if took {
        state.subject_photo.set(None);
        clear_subject_brush_ops(state);
    }
    if let Some(item_id) = item_id {
        state.selected_layer.set(Some(layer_id));
        state.mask_target.set(Some((item_id, layer_id)));
    }
    item_id.is_some()
}

pub(crate) fn with_layer(state: AppState, layer_id: usize, f: impl FnOnce(&mut Layer)) {
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == layer_id) {
            f(l);
        }
    });
}

const MASK_THUMB_W: usize = 24;
const MASK_THUMB_H: usize = 18;

fn draw_mask_thumb(canvas: &web_sys::HtmlCanvasElement, layer: &Layer) {
    let (w, h) = (MASK_THUMB_W, MASK_THUMB_H);
    let mut mask = match &layer.mask {
        Some(sel) => ops::selection_mask(sel, w, h),
        None => {
            web::put_pixels(canvas, &vec![0u8; w * h * 4], w as u32, h as u32);
            return;
        }
    };
    if layer.invert {
        for v in &mut mask {
            *v = 255 - *v;
        }
    }
    let mut rgba = vec![0u8; w * h * 4];
    for (m, px) in mask.iter().zip(rgba.as_chunks_mut::<4>().0) {
        px[0] = *m;
        px[1] = *m;
        px[2] = *m;
        px[3] = 255;
    }
    web::put_pixels(canvas, &rgba, w as u32, h as u32);
}

/// Mask controls shared by every layer kind: the mask itself, invert, the
/// editor-only overlay and feather.
#[component]
pub(crate) fn MaskSection(state: AppState, layer_id: usize) -> impl IntoView {
    let layer = create_memo(move |_| {
        state.current_layers().into_iter().find(|l| l.id == layer_id)
    });
    let is_adjust = move || {
        layer.get().map(|l| matches!(l.kind, LayerKind::Adjust(_))).unwrap_or(false)
    };
    let has_selection = move || state.current().map(|m| m.edit.selection.is_some()).unwrap_or(false);
    let has_mask = move || layer.get().map(|l| l.mask.is_some()).unwrap_or(false);
    let feather = move || layer.get().and_then(|l| l.mask).map(|s| s.feather).unwrap_or(0.0);
    let targeted = move || state.mask_target.get().is_some_and(|(_, id)| id == layer_id);

    view! {
        <div class="mask-section">
            <div class="panel-title">"Mask"</div>
            <div class="row">
                <button
                    class="btn"
                    on:click=move |_| {
                        mask_from_selection(state, layer_id);
                    }
                >{move || if is_adjust() {
                    "New adjustment from selection"
                } else {
                    "Add mask"
                }}</button>
                <button
                    class="btn"
                    disabled=move || !has_mask()
                    on:click=move |_| with_layer(state, layer_id, |l| l.set_mask(None))
                >"Clear mask"</button>
            </div>
            <label class="row" style="justify-content:flex-start;gap:0.5rem">
                <input
                    type="checkbox"
                    prop:checked=move || layer.get().map(|l| l.invert).unwrap_or(false)
                    on:change=move |ev| {
                        let v = event_target_checked(&ev);
                        with_layer(state, layer_id, |l| l.invert = v);
                    }
                />
                "Invert"
            </label>
            <label class="row" style="justify-content:flex-start;gap:0.5rem">
                <input
                    type="checkbox"
                    prop:checked=move || layer.get().map(|l| l.show_mask).unwrap_or(false)
                    on:change=move |ev| {
                        let v = event_target_checked(&ev);
                        with_layer(state, layer_id, |l| l.show_mask = v);
                    }
                />
                "Show mask"
            </label>
            <label class="slider">
                <span>"Feather: " {move || format!("{:.0}%", feather() * 100.0)}</span>
                <input
                    type="range" min="0" max="0.25" step="0.01"
                    prop:value=move || feather().to_string()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                        with_layer(state, layer_id, |l| {
                            if let Some(ref mut s) = l.mask {
                                s.feather = v.clamp(0.0, 0.25);
                                l.mask_version += 1;
                            }
                        });
                    }
                />
            </label>
            <p class="dim">
                {move || if has_mask() {
                    if targeted() {
                        "Brush (Select tab) paints this mask: Add reveals, Remove hides."
                    } else {
                        "Click the row's mask thumbnail to paint it with the Select-tab Brush."
                    }
                } else if has_selection() {
                    "The current selection becomes this layer's mask."
                } else {
                    "Add mask reveals the whole layer; the Select-tab Brush paints it."
                }}
            </p>
        </div>
    }
}

#[component]
pub(crate) fn TextLayerEditor(state: AppState) -> impl IntoView {
    let fonts = move || state.fonts.get();
    let text_layer = move || selected_text_layer(state);
    let font_input = create_node_ref::<html::Input>();

    let upload_font = move |_| {
        let Some(input) = font_input.get() else { return };
        let Some(files) = input.files() else { return };
        let Some(file) = files.item(0) else { return };
        let name = file.name();
        let base = name.rsplitn(2, '.').last().unwrap_or("custom").to_string();
        spawn_local(async move {
            let Ok(buf) = web::read_file_array_buffer(&file).await else { return };
            let family = format!("custom-{}", base.replace(' ', "-"));
            if web::load_font_from_buffer(&family, &buf, "400").await.is_ok() {
                state.fonts.update(|v| {
                    if !v.contains(&family) {
                        v.push(family.clone());
                    }
                });
                with_text_layer(state, |t| t.font_family = family);
            }
        });
        input.set_value("");
    };

    view! {
        <Show when=move || text_layer().is_some() fallback=|| ()>
            <div class="text-editor">
                <input
                    type="text"
                    prop:value=move || text_layer().map(|t| t.text).unwrap_or_default()
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        with_text_layer(state, |t| t.text = v);
                    }
                />
                <div class="row">
                    <label>
                        "Font"
                        <select
                            prop:value=move || text_layer().map(|t| t.font_family).unwrap_or_default()
                            on:change=move |ev| {
                                let v = event_target_value(&ev);
                                with_text_layer(state, |t| t.font_family = v);
                            }
                        >
                            {move || fonts().into_iter().map(|f| {
                                view! { <option value=f.clone()>{f}</option> }
                            }).collect_view()}
                        </select>
                    </label>
                    <label class="btn font-upload">
                        "Upload font"
                        <input
                            node_ref=font_input
                            type="file"
                            accept=".ttf,.otf,.woff2"
                            style="display:none"
                            on:change=upload_font
                        />
                    </label>
                    <label class="slider compact">
                        "Size"
                        <input
                            type="range"
                            min="0.01"
                            max="0.3"
                            step="0.005"
                            prop:value=move || text_layer().map(|t| t.font_size).unwrap_or_default().to_string()
                            on:input=move |ev| {
                                let v: f32 = event_target_value(&ev).parse().unwrap_or(0.08);
                                with_text_layer(state, |t| t.font_size = v);
                            }
                        />
                    </label>
                    <label class="slider compact">
                        "Angle"
                        <input
                            type="range"
                            min="-180"
                            max="180"
                            step="1"
                            prop:value=move || text_layer().map(|t| t.angle).unwrap_or_default().to_string()
                            on:input=move |ev| {
                                let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                                with_text_layer(state, |t| t.angle = v);
                            }
                        />
                    </label>
                </div>
                <div class="row">
                    <label>
                        "Color"
                        <input
                            type="color"
                            prop:value=move || text_layer().map(|t| t.color).unwrap_or_default()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                with_text_layer(state, |t| t.color = v);
                            }
                        />
                    </label>
                    <label>
                        "Stroke"
                        <input
                            type="color"
                            prop:value=move || text_layer().map(|t| t.stroke_color).unwrap_or_default()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                with_text_layer(state, |t| t.stroke_color = v);
                            }
                        />
                    </label>
                    <label class="slider compact">
                        "W"
                        <input
                            type="range"
                            min="0"
                            max="0.5"
                            step="0.01"
                            prop:value=move || text_layer().map(|t| t.stroke_width).unwrap_or_default().to_string()
                            on:input=move |ev| {
                                let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                                with_text_layer(state, |t| t.stroke_width = v);
                            }
                        />
                    </label>
                </div>
                <div class="row">
                    {TextAlign::ALL.map(|a| {
                        let active = move || text_layer().map(|t| t.alignment == a).unwrap_or(false);
                        view! {
                            <button
                                class="chip"
                                class:active=active
                                on:click=move |_| with_text_layer(state, |t| t.alignment = a)
                            >
                                {a.label()}
                            </button>
                        }
                    })}
                </div>
                <div class="row">
                    <label class="slider compact">
                        "Shadow"
                        <input
                            type="range"
                            min="0"
                            max="20"
                            step="0.5"
                            prop:value=move || text_layer().map(|t| t.shadow_blur).unwrap_or_default().to_string()
                            on:input=move |ev| {
                                let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                                with_text_layer(state, |t| t.shadow_blur = v);
                            }
                        />
                    </label>
                    <label>
                        "Shadow color"
                        <input
                            type="color"
                            prop:value=move || text_layer().map(|t| t.shadow_color).unwrap_or_default()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                with_text_layer(state, |t| t.shadow_color = v);
                            }
                        />
                    </label>
                </div>
            </div>
        </Show>
    }
}

#[component]
pub(crate) fn PathLayerEditor(state: AppState) -> impl IntoView {
    let path_layer = move || selected_path_layer(state);

    let undo = move |_| {
        with_path_layer(state, |p| {
            if let Some(prev) = p.history.pop() {
                p.points = prev;
            }
        });
    };

    let clear = move |_| {
        with_path_layer(state, |p| {
            push_path_history(p);
            p.points.clear();
            p.closed = false;
        });
    };

    view! {
        <Show when=move || path_layer().is_some() fallback=|| ()>
            <div class="path-editor">
                <div class="row">
                    <label>
                        "Fill"
                        <input
                            type="color"
                            prop:value=move || path_layer().map(|p| p.fill_color).unwrap_or_default()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                with_path_layer(state, |p| p.fill_color = v);
                            }
                        />
                    </label>
                    <label>
                        "Stroke"
                        <input
                            type="color"
                            prop:value=move || path_layer().map(|p| p.stroke_color).unwrap_or_default()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                with_path_layer(state, |p| p.stroke_color = v);
                            }
                        />
                    </label>
                    <label class="slider compact">
                        "W"
                        <input
                            type="range"
                            min="0"
                            max="0.1"
                            step="0.001"
                            prop:value=move || path_layer().map(|p| p.stroke_width).unwrap_or_default().to_string()
                            on:input=move |ev| {
                                let v: f32 = event_target_value(&ev).parse().unwrap_or(0.01);
                                with_path_layer(state, |p| p.stroke_width = v);
                            }
                        />
                    </label>
                </div>
                <div class="row">
                    <button class="btn" on:click=undo>"Undo"
                        {move || path_layer().map(|p| format!(" ({})", p.history.len())).unwrap_or_default()}
                    </button>
                    <button class="btn" on:click=clear>"Clear"
                        {move || path_layer().map(|p| format!(" ({} pts)", p.points.len())).unwrap_or_default()}
                    </button>
                </div>
                <p class="dim">
                    "Pen: click to add anchors, drag handles, double-tap point for smooth↔corner, tap first point to close."
                </p>
            </div>
        </Show>
    }
}

#[component]
pub(crate) fn BrushLayerEditor(state: AppState) -> impl IntoView {
    let brush_layer = move || selected_brush_layer(state);

    let undo = move |_| {
        with_brush_layer(state, |b| {
            if let Some(prev) = b.history.pop() {
                b.strokes = prev;
            }
        });
    };

    let clear = move |_| {
        with_brush_layer(state, |b| {
            push_brush_history(b);
            b.strokes.clear();
        });
    };

    view! {
        <Show when=move || brush_layer().is_some() fallback=|| ()>
            <div class="brush-editor">
                <div class="row">
                    <label>
                        "Color"
                        <input
                            type="color"
                            prop:value=move || brush_layer().map(|b| b.color).unwrap_or_default()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                with_brush_layer(state, |b| b.color = v);
                            }
                        />
                    </label>
                    <label class="slider compact">
                        "Size"
                        <input
                            type="range"
                            min="0.005"
                            max="0.1"
                            step="0.001"
                            prop:value=move || brush_layer().map(|b| b.width).unwrap_or_default().to_string()
                            on:input=move |ev| {
                                let v: f32 = event_target_value(&ev).parse().unwrap_or(0.015);
                                with_brush_layer(state, |b| b.width = v);
                            }
                        />
                    </label>
                </div>
                <div class="chips">
                    {state::BrushShape::ALL.map(|shape| {
                        let active = move || brush_layer().map(|b| b.shape) == Some(shape);
                        view! {
                            <button
                                class="chip"
                                class:active=active
                                on:click=move |_| with_brush_layer(state, |b| b.shape = shape)
                            >
                                {shape.label()}
                            </button>
                        }
                    })}
                </div>
                <Show when=move || brush_layer().map(|b| b.shape != state::BrushShape::Smooth).unwrap_or(false)>
                    <label class="slider compact">
                        "Spacing"
                        <input
                            type="range"
                            min="0.1"
                            max="4.0"
                            step="0.05"
                            prop:value=move || brush_layer().map(|b| b.spacing.to_string()).unwrap_or_default()
                            on:input=move |ev| {
                                let v: f32 = event_target_value(&ev).parse().unwrap_or(1.0);
                                with_brush_layer(state, |b| b.spacing = v.clamp(0.1, 4.0));
                            }
                        />
                        {move || brush_layer().map(|b| format!(" {:.2}×", b.spacing)).unwrap_or_default()}
                    </label>
                </Show>
                <Show when=move || brush_layer().map(|b| b.shape == state::BrushShape::Custom).unwrap_or(false)>
                    <label class="custom-path">
                        "SVG path (100×100 box, centered on 50,50, pointing right)"
                        <textarea
                            rows="3"
                            prop:value=move || brush_layer().map(|b| b.custom_path).unwrap_or_default()
                            on:input=move |ev| {
                                let v = event_target_value(&ev);
                                with_brush_layer(state, |b| b.custom_path = v);
                            }
                        />
                    </label>
                </Show>
                <div class="row">
                    <button class="btn" on:click=undo>"Undo"
                        {move || brush_layer().map(|b| format!(" ({})", b.history.len())).unwrap_or_default()}
                    </button>
                    <button class="btn" on:click=clear>"Clear"
                        {move || brush_layer().map(|b| format!(" ({} strokes)", b.strokes.len())).unwrap_or_default()}
                    </button>
                </div>
                <p class="dim">"Brush: drag on the photo to draw freehand strokes."</p>
            </div>
        </Show>
    }
}

#[component]
pub(crate) fn RasterLayerEditor(state: AppState) -> impl IntoView {
    let raster_layer = move || {
        let sel = state.selected_layer.get()?;
        state
            .current_layers()
            .into_iter()
            .find(|l| l.id == sel)
            .and_then(|l| match l.kind {
                LayerKind::Raster(r) => Some(r),
                _ => None,
            })
    };

    view! {
        <Show when=move || raster_layer().is_some() fallback=|| ()>
            <div class="raster-editor">
                <label class="slider compact">
                    "Size"
                    <input
                        type="range"
                        min="0.05"
                        max="2.0"
                        step="0.01"
                        prop:value=move || raster_layer().map(|r| r.scale.to_string()).unwrap_or_default()
                        on:input=move |ev| {
                            let v: f32 = event_target_value(&ev).parse().unwrap_or(1.0);
                            let sel = state.selected_layer.get_untracked();
                            state.update_current_item(|m| {
                                if let Some(l) = m.layers.iter_mut().find(|l| Some(l.id) == sel) {
                                    if let LayerKind::Raster(r) = &mut l.kind {
                                        r.scale = v.clamp(0.05, 2.0);
                                    }
                                }
                            });
                        }
                    />
                    {move || raster_layer().map(|r| format!(" {:.0}%", r.scale * 100.0)).unwrap_or_default()}
                </label>
                <p class="dim">"Image layer: drag on the photo to move it, use the slider to resize."</p>
            </div>
        </Show>
    }
}
