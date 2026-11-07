//! App shell and shared UI components: root state wiring, import controls, filmstrip, toolbar, busy overlay and layer-selection helpers.

use crate::prelude::*;
use crate::*;

mod canvas;
mod layers;
mod preview;
mod tabs;
pub(crate) use canvas::*;
pub(crate) use layers::*;
pub(crate) use preview::*;
pub(crate) use tabs::*;

thread_local! {
    pub(crate) static FONTS_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

// The bundled fonts only matter once a text layer exists, so don't fetch them
// at startup.
pub(crate) fn ensure_fonts() {
    if FONTS_REQUESTED.with(|c| c.replace(true)) {
        return;
    }
    spawn_local(async {
        let _ = web::load_font("Inter", "fonts/inter-400.woff2", "400").await;
        let _ = web::load_font("Inter", "fonts/inter-700.woff2", "700").await;
        let _ = web::load_font("Oswald", "fonts/oswald-400.woff2", "400").await;
        let _ = web::load_font("Oswald", "fonts/oswald-700.woff2", "700").await;
    });
}

#[component]
pub(crate) fn App() -> impl IntoView {
    let state = AppState::new();
    provide_context(state);

    // Restore the previous session from IndexedDB (no-op on first run), then
    // mirror every later state change back — debounced, diffed per item.
    restore_session(state);
    create_effect(move |_| {
        perf::record("session.effect", 0.0);
        state.items.track();
        state.selected.track();
        schedule_session_save(state);
    });

    // Load the selected photo's display pixels on demand: a RAW gets its
    // camera proxy first, then a background sensor develop; unselected photos
    // stay preview-only and their buffers are LRU-evicted.
    create_effect(move |_| {
        let Some(id) = state.selected.get() else { return };
        let is_photo = state
            .items
            .with(|v| v.iter().any(|m| m.id == id && m.kind == MediaKind::Photo));
        if is_photo {
            ensure_preview(state, id);
            ensure_proxy(state, id, true);
        }
    });

    // Persist parametric edits of Drive-backed items into the manifest's
    // edits map (debounced). Baked items carry no recipe — their Drive copy
    // has the edits flattened into pixels. Reset to identity removes the
    // entry via the same path.
    create_effect(move |_| {
        let Some(item) = state.current() else { return };
        let Some(fid) = item.drive_file_id.clone() else { return };
        if item.drive_baked {
            return;
        }
        let recipe = state::edit_recipe_json(&state::render_edit(&item.edit, &item.layers));
        let stored = state.drive_edits.with(|m| m.get(&fid).cloned());
        if stored != recipe {
            state.drive_edits.update(|m| match &recipe {
                Some(r) => {
                    m.insert(fid.clone(), r.clone());
                }
                None => {
                    m.remove(&fid);
                }
            });
            schedule_manifest_save(state);
        }
    });

    view! {
        <div class="app">
            <header>
                <h1>"photo-edit-simplified"</h1>
                <AddButton state=state/>
            </header>
            <DrivePanel state=state/>
            <Show when=move || state.items.with(|v| v.is_empty()) fallback=|| ()>
                <DropZone state=state/>
            </Show>
            <Show when=move || !state.items.with(|v| v.is_empty()) fallback=|| ()>
                <FilmStrip state=state/>
                <Editor state=state/>
            </Show>
            <BusyOverlay state=state/>
        </div>
    }
}

pub(crate) fn files_from_list(list: &web_sys::FileList) -> Vec<File> {
    (0..list.length()).filter_map(|i| list.item(i)).collect()
}

#[component]
pub(crate) fn AddButton(state: AppState) -> impl IntoView {
    let input_ref = create_node_ref::<html::Input>();
    view! {
        <label class="btn">
            "＋ Add media"
            <input
                node_ref=input_ref
                type="file"
                accept="image/*,.heic,.heif,.dng,.cr2,.cr3,.nef,.nrw,.arw,.orf,.rw2,.raf,.pef,.srw,video/*"
                multiple
                style="display:none"
                on:change=move |_| {
                    if let Some(input) = input_ref.get() {
                        if let Some(files) = input.files() {
                            ingest_files(state, files_from_list(&files));
                        }
                        input.set_value("");
                    }
                }
            />
        </label>
    }
}

#[component]
pub(crate) fn DropZone(state: AppState) -> impl IntoView {
    view! {
        <div
            class="dropzone"
            on:dragover=move |ev| ev.prevent_default()
            on:drop=move |ev| {
                ev.prevent_default();
                if let Some(dt) = ev.data_transfer() {
                    if let Some(files) = dt.files() {
                        ingest_files(state, files_from_list(&files));
                    }
                }
            }
        >
            <p>"Drag photos or videos here, or tap “＋ Add media”."</p>
            <p class="dim">"Everything stays on your device."</p>
        </div>
    }
}

/// True when an item carries any visible edit: adjusted params, crop/rotate,
/// trim, annotation layers, an unsaved destructive op, or a Drive copy that
/// already has edits baked in.
pub(crate) fn item_edited(m: &MediaItem) -> bool {
    let dirty = get_photo(m.id).map(|p| p.dirty).unwrap_or(false);
    m.drive_baked
        || dirty
        || match m.kind {
            MediaKind::Photo => !photo_unedited(m),
            MediaKind::Video => {
                let e = &m.edit;
                e.trim.is_some()
                    || !e.keep_audio
                    || e.crop != state::CropRect::default()
                    || e.rot90 != 0
                    || e.fine_angle != 0.0
                    || e.aspect != Aspect::Original
            }
        }
}

#[component]
pub(crate) fn FilmStrip(state: AppState) -> impl IntoView {
    let strip_ref = create_node_ref::<html::Div>();
    create_effect(move |_| {
        if let Some(el) = strip_ref.get() {
            ensure_thumb_observer(state, &el);
        }
    });
    view! {
        <div class="filmstrip" node_ref=strip_ref>
            <For
                each=move || {
                    perf::record("filmstrip.each", 0.0);
                    // Membership only: edit ticks touch `items` but must not
                    // rebuild this list.
                    state.list_gen.track();
                    state.items.get_untracked()
                }
                key=|m| m.id
                children=move |m: MediaItem| {
                    let id = m.id;
                    let is_photo = m.kind == MediaKind::Photo;
                    let badge = if m.kind == MediaKind::Video { "▶" } else { "" };
                    let tile_ref = create_node_ref::<html::Div>();
                    create_effect(move |_| {
                        if let Some(el) = tile_ref.get() {
                            observe_thumb(&el);
                        }
                    });
                    view! {
                        <div
                            class="thumb"
                            node_ref=tile_ref
                            attr:data-id=id.to_string()
                            title=m.name.clone()
                            class:selected=move || state.selected.get() == Some(id)
                            on:click=move |_| state.selected.set(Some(id))
                            on:mouseenter=move |ev| {
                                if is_photo {
                                    loupe_soon(state, id, &ev);
                                }
                            }
                            on:mousemove=move |ev| loupe_move(state, id, &ev)
                            on:mouseleave=move |_| loupe_clear(state)
                        >
                            <img src=m.thumb_url.clone()/>
                            <span class="badge">{badge}</span>
                            {move || {
                                let edited = state.items.with(|v| {
                                    v.iter().find(|m| m.id == id).map(item_edited).unwrap_or(false)
                                });
                                edited.then(|| {
                                    view! { <span class="badge thumb-edited" title="Has edits">"✎"</span> }
                                })
                            }}
                            {move || {
                                // Read live: an item gains its drive id after
                                // the first successful save, and this tile is
                                // not rebuilt on item-content changes.
                                let did = state.items.with(|v| {
                                    v.iter().find(|m| m.id == id).and_then(|m| m.drive_file_id.clone())
                                })?;
                                if !state.drive_shortlist.with(|s| s.contains(&did)) {
                                    return None;
                                }
                                Some(view! {
                                    <button
                                        class="thumb-star"
                                        title="Starred on Drive — click to unstar (removes it here)"
                                        on:click=move |ev| {
                                            ev.stop_propagation();
                                            toggle_star(state, did.clone());
                                        }
                                    >
                                        "★"
                                    </button>
                                })
                            }}
                        </div>
                    }
                }
            />
        </div>
        <FocusLoupe state=state/>
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Tab {
    Raw,
    Crop,
    Rotate,
    Color,
    Select,
    Heal,
    Clone,
    Layers,
    Trim,
    Export,
}

#[component]
pub(crate) fn TabBtn(tab: RwSignal<Tab>, t: Tab, label: &'static str) -> impl IntoView {
    view! {
        <button class="tab" class:active=move || tab.get() == t on:click=move |_| tab.set(t)>
            {label}
        </button>
    }
}

#[component]
pub(crate) fn Editor(state: AppState) -> impl IntoView {
    let tab = create_rw_signal(Tab::Crop);
    // The subject worker holds the encoder/decoder and wasm pages that never
    // shrink: close it when the Select tool is left so the tab gets them back.
    create_effect(move |_| {
        if tab.get() != Tab::Select {
            web::segment_subject_close();
            state.subject_photo.set(None);
        }
    });
    on_cleanup(move || {
        web::segment_subject_close();
        state.subject_photo.set(None);
    });
    let raw_item = move || state.current().map(|m| has_raw_controls(&m)).unwrap_or(false);
    // The RAW tab only exists for RAW items: switching to a JPEG while it is
    // open lands on Color, where the rest of the colour controls live.
    create_effect(move |_| {
        if tab.get() == Tab::Raw && state.current().is_some() && !raw_item() {
            tab.set(Tab::Color);
        }
    });
    view! {
        <Show when=move || state.current().is_some() fallback=|| ()>
            <div class="editor">
                <Preview state=state tab=tab/>
                <div class="controls">
                    <div class="tabs">
                    <Show when=raw_item fallback=|| ()>
                        <TabBtn tab=tab t=Tab::Raw label="RAW"/>
                    </Show>
                    <TabBtn tab=tab t=Tab::Crop label="Crop"/>
                    <TabBtn tab=tab t=Tab::Rotate label="Rotate"/>
                    <TabBtn tab=tab t=Tab::Color label="Color"/>
                    <TabBtn tab=tab t=Tab::Select label="Select"/>
                    <TabBtn tab=tab t=Tab::Heal label="Heal"/>
                    <TabBtn tab=tab t=Tab::Clone label="Clone"/>
                    <TabBtn tab=tab t=Tab::Layers label="Layers"/>
                    <Show
                        when=move || state.current().map(|m| m.kind == MediaKind::Video).unwrap_or(false)
                        fallback=|| ()
                    >
                        <TabBtn tab=tab t=Tab::Trim label="Trim"/>
                    </Show>
                        <TabBtn tab=tab t=Tab::Export label="Export"/>
                    </div>
                    <div class="panel">
                    {move || match tab.get() {
                        Tab::Raw => view! { <RawPanel state=state/> }.into_view(),
                        Tab::Crop => view! { <CropTab state=state tab=tab/> }.into_view(),
                        Tab::Rotate => view! { <RotateTab state=state/> }.into_view(),
                        Tab::Color => view! { <ColorTab state=state/> }.into_view(),
                        Tab::Select => view! { <SelectTab state=state/> }.into_view(),
                        Tab::Heal => view! { <HealTab state=state/> }.into_view(),
                        Tab::Clone => view! { <CloneTab state=state/> }.into_view(),
                        Tab::Layers => view! { <LayersTab state=state/> }.into_view(),
                        Tab::Trim => view! { <TrimTab state=state/> }.into_view(),
                        Tab::Export => view! { <ExportTab state=state/> }.into_view(),
                    }}
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[component]
pub(crate) fn ToolBar(state: AppState) -> impl IntoView {
    view! {
        <div class="toolbar">
            {Tool::ALL.map(|t| {
                view! {
                    <button
                        class="chip"
                        class:active=move || state.selected_tool.get() == t
                        on:click=move |_| {
                            ensure_tool_layer(state, t);
                            state.selected_tool.set(t);
                        }
                    >
                        {t.label()}
                    </button>
                }
            })}
        </div>
    }
}

pub(crate) fn with_text_layer(state: AppState, f: impl FnOnce(&mut state::TextLayer)) {
    let Some(sel) = state.selected_layer.get_untracked() else { return };
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == sel) {
            let LayerKind::Text(t) = &mut l.kind else { return };
            f(t);
        }
    });
}

pub(crate) fn selected_text_layer(state: AppState) -> Option<state::TextLayer> {
    let sel = state.selected_layer.get()?;
    state.current()?.layers.iter().find(|l| l.id == sel).and_then(|l| {
        match &l.kind {
            LayerKind::Text(t) => Some(t.clone()),
            _ => None,
        }
    })
}

pub(crate) fn with_path_layer(state: AppState, f: impl FnOnce(&mut state::PathLayer)) {
    let Some(sel) = state.selected_layer.get_untracked() else { return };
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == sel) {
            let LayerKind::Path(p) = &mut l.kind else { return };
            f(p);
        }
    });
}

pub(crate) fn selected_path_layer(state: AppState) -> Option<state::PathLayer> {
    let sel = state.selected_layer.get()?;
    state.current()?.layers.iter().find(|l| l.id == sel).and_then(|l| {
        match &l.kind {
            LayerKind::Path(p) => Some(p.clone()),
            _ => None,
        }
    })
}

pub(crate) fn with_brush_layer(state: AppState, f: impl FnOnce(&mut state::BrushLayer)) {
    let Some(sel) = state.selected_layer.get_untracked() else { return };
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == sel) {
            let LayerKind::Brush(b) = &mut l.kind else { return };
            f(b);
        }
    });
}

pub(crate) fn selected_brush_layer(state: AppState) -> Option<state::BrushLayer> {
    let sel = state.selected_layer.get()?;
    state.current()?.layers.iter().find(|l| l.id == sel).and_then(|l| {
        match &l.kind {
            LayerKind::Brush(b) => Some(b.clone()),
            _ => None,
        }
    })
}

#[component]
pub(crate) fn BusyOverlay(state: AppState) -> impl IntoView {
    view! {
        <Show when=move || state.busy.get().is_some() fallback=|| ()>
            <div class="overlay">
                <div class="card">
                    <p>{move || state.busy.get().unwrap_or_default()}</p>
                    <progress max="1" value=move || state.progress.get().to_string()></progress>
                </div>
            </div>
        </Show>
    }
}
