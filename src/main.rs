mod ops;
mod state;
mod web;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::leptos_dom::helpers::window_event_listener;
use leptos::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Blob, File, Url};

use state::{AppState, Aspect, EditParams, MediaItem, MediaKind};

// --- media cache (non-reactive) ---------------------------------------------

struct PhotoData {
    full: (Vec<u8>, usize, usize),
    preview: (Vec<u8>, usize, usize),
}

struct Cache {
    photos: HashMap<usize, Rc<PhotoData>>,
    video_blobs: HashMap<usize, Blob>,
    video_meta: HashMap<usize, (f64, u32, u32)>, // duration, w, h
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache {
        photos: HashMap::new(),
        video_blobs: HashMap::new(),
        video_meta: HashMap::new(),
    });
}

fn get_photo(id: usize) -> Option<Rc<PhotoData>> {
    CACHE.with(|c| c.borrow().photos.get(&id).cloned())
}

fn get_video(id: usize) -> Option<(Blob, f64, u32, u32)> {
    CACHE.with(|c| {
        let c = c.borrow();
        match (c.video_blobs.get(&id), c.video_meta.get(&id)) {
            (Some(b), Some(m)) => Some((b.clone(), m.0, m.1, m.2)),
            _ => None,
        }
    })
}

fn is_heic(file: &File) -> bool {
    let t = file.type_().to_lowercase();
    let n = file.name().to_lowercase();
    t.contains("heic") || t.contains("heif") || n.ends_with(".heic") || n.ends_with(".heif")
}

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(|| view! { <App/> });
}

// --- ingestion ---------------------------------------------------------------

fn ingest_files(state: AppState, files: Vec<File>) {
    spawn_local(async move {
        for file in files {
            let name = file.name();
            let id = state.next_id.get();
            state.next_id.set(id + 1);

            if file.type_().starts_with("video/") {
                state.busy.set(Some(format!("Loading {name}…")));
                let blob: Blob = file.into();
                let url = Url::create_object_url_with_blob(&blob).unwrap();
                if let Some((dur, w, h)) = probe_video(&url).await {
                    CACHE.with(|c| {
                        let mut c = c.borrow_mut();
                        c.video_blobs.insert(id, blob);
                        c.video_meta.insert(id, (dur, w, h));
                    });
                    push_item(state, MediaItem {
                        id,
                        kind: MediaKind::Video,
                        name,
                        object_url: url,
                        width: w as usize,
                        height: h as usize,
                        edit: EditParams::default(),
                    });
                } else {
                    web::log("video probe failed");
                }
            } else {
                state.busy.set(Some(format!("Loading {name}…")));
                let mut blob: Blob = file.clone().into();
                if is_heic(&file) {
                    state.busy.set(Some(format!("Converting HEIC {name}…")));
                    match web::heic_to_jpeg(blob).await {
                        Ok(b) => blob = b,
                        Err(_) => {
                            state.busy.set(None);
                            continue;
                        }
                    }
                }
                let Ok((img, url)) = web::load_image(&blob).await else {
                    state.busy.set(None);
                    continue;
                };
                let full = web::rgba_from_image(&img);
                let (fw, fh) = (full.1, full.2);
                let preview = downscale(&img, 1024);
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(id, Rc::new(PhotoData { full, preview }))
                });
                push_item(state, MediaItem {
                    id,
                    kind: MediaKind::Photo,
                    name,
                    object_url: url,
                    width: fw,
                    height: fh,
                    edit: EditParams::default(),
                });
            }
            state.busy.set(None);
        }
    });
}

fn push_item(state: AppState, item: MediaItem) {
    let id = item.id;
    state.items.update(|v| v.push(item));
    if state.selected.get().is_none() {
        state.selected.set(Some(id));
    }
}

async fn probe_video(url: &str) -> Option<(f64, u32, u32)> {
    let v: web_sys::HtmlVideoElement = web::document()
        .create_element("video")
        .ok()?
        .unchecked_into();
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        v.set_onloadedmetadata(Some(&resolve));
        v.set_onerror(Some(&reject));
    });
    v.set_src(url);
    wasm_bindgen_futures::JsFuture::from(promise).await.ok()?;
    let dur = v.duration();
    let (w, h) = (v.video_width(), v.video_height());
    if dur.is_nan() || w == 0 {
        None
    } else {
        Some((dur, w, h))
    }
}

fn downscale(img: &web_sys::HtmlImageElement, long_edge: u32) -> (Vec<u8>, usize, usize) {
    let (w, h) = (img.natural_width(), img.natural_height());
    let scale = (long_edge as f32 / w.max(h) as f32).min(1.0);
    let (pw, ph) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
    let canvas = web::create_canvas(pw, ph);
    let ctx = web::ctx2d(&canvas);
    ctx.draw_image_with_html_image_element_and_dw_and_dh(img, 0.0, 0.0, pw as f64, ph as f64)
        .unwrap();
    web::image_data_from_ctx(&ctx, pw, ph)
}

// --- processing pipeline ------------------------------------------------------

fn geometry(pixels: &[u8], w: usize, h: usize, edit: &EditParams) -> (Vec<u8>, usize, usize) {
    let (p, w, h) = if edit.rot90 % 4 != 0 {
        ops::rotate_90(pixels, w, h, edit.rot90)
    } else {
        (pixels.to_vec(), w, h)
    };
    if edit.fine_angle.abs() > 0.01 {
        ops::rotate_auto_crop(&p, w, h, edit.fine_angle)
    } else {
        (p, w, h)
    }
}

fn working_dims(w: usize, h: usize, edit: &EditParams) -> (usize, usize) {
    let (w, h) = if edit.rot90 % 2 == 1 { (h, w) } else { (w, h) };
    if edit.fine_angle.abs() > 0.01 {
        let a = edit.fine_angle.to_radians().abs();
        let (sin, cos) = (a.sin(), a.cos());
        let bw = w as f32 * cos + h as f32 * sin;
        let bh = w as f32 * sin + h as f32 * cos;
        let x1 = (w * w) as f32 / (2.0 * bw);
        let x2 = (w * h) as f32 / (2.0 * bh);
        let half = x1.min(x2);
        (
            (half * 2.0).floor().max(1.0) as usize,
            (half * 2.0 * h as f32 / w as f32).floor().max(1.0) as usize,
        )
    } else {
        (w, h)
    }
}

fn default_crop(w: usize, h: usize, aspect: Aspect) -> state::CropRect {
    match aspect.ratio() {
        None => state::CropRect::default(),
        Some((rw, rh)) => {
            let target = rw / rh;
            let src = w as f32 / h as f32;
            if src > target {
                let cw = target * h as f32 / w as f32;
                state::CropRect { x: (1.0 - cw) / 2.0, y: 0.0, w: cw, h: 1.0 }
            } else {
                let ch = w as f32 / (target * h as f32);
                state::CropRect { x: 0.0, y: (1.0 - ch) / 2.0, w: 1.0, h: ch }
            }
        }
    }
}

// --- export --------------------------------------------------------------------

fn crop_px(edit: &EditParams, w: usize, h: usize) -> (usize, usize, usize, usize) {
    let c = edit.crop;
    let x = (c.x * w as f32).round() as usize;
    let y = (c.y * h as f32).round() as usize;
    let cw = ((c.w * w as f32).round() as usize).max(1).min(w - x);
    let ch = ((c.h * h as f32).round() as usize).max(1).min(h - y);
    (x, y, cw, ch)
}

fn export_photo(state: AppState, item: MediaItem) {
    spawn_local(async move {
        state.busy.set(Some(format!("Exporting {}…", item.name)));
        let Some(photo) = get_photo(item.id) else {
            state.busy.set(None);
            return;
        };
        let (pix, w, h) = &photo.full;
        let (p, w, h) = geometry(pix, *w, *h, &item.edit);
        let (cx, cy, cw, ch) = crop_px(&item.edit, w, h);
        let mut p = ops::crop(&p, w, cx, cy, cw, ch);
        if item.edit.is_color_touched() {
            ops::adjust(
                &mut p,
                item.edit.brightness,
                item.edit.contrast,
                item.edit.saturation,
                item.edit.warmth,
            );
        }
        let work = web::create_canvas(cw as u32, ch as u32);
        web::put_pixels(&work, &p, cw as u32, ch as u32);
        let (ew, eh) = item.edit.aspect.export_dims(cw, ch);
        let out = web::create_canvas(ew as u32, eh as u32);
        web::ctx2d(&out)
            .draw_image_with_html_canvas_element_and_dw_and_dh(&work, 0.0, 0.0, ew as f64, eh as f64)
            .unwrap();
        if let Ok(blob) = web::canvas_to_jpeg_blob(&out, 0.92).await {
            let base = item.name.rsplitn(2, '.').last().unwrap_or("photo");
            web::download_blob(&blob, &format!("edited-{}-{}.jpg", item.edit.aspect.label(), base));
        }
        state.busy.set(None);
    });
}

fn export_video(state: AppState, item: MediaItem) {
    spawn_local(async move {
        state.busy.set(Some(format!("Exporting {}…", item.name)));
        state.progress.set(0.0);
        let Some((blob, _dur, w, h)) = get_video(item.id) else {
            state.busy.set(None);
            return;
        };
        let args = build_ffmpeg_args(&item, w as usize, h as usize);
        let on_progress = Closure::new(move |r: f64| state.progress.set(r as f32));
        let result = web::video_transcode(&item.name, blob, args, on_progress).await;
        match result {
            Ok(out) => {
                let base = item.name.rsplitn(2, '.').last().unwrap_or("video");
                web::download_blob(&out, &format!("edited-{}.mp4", base));
            }
            Err(e) => web::log_err(&format!("transcode failed: {e:?}")),
        }
        state.busy.set(None);
        state.progress.set(0.0);
    });
}

fn build_ffmpeg_args(item: &MediaItem, w: usize, h: usize) -> Vec<String> {
    let e = &item.edit;
    let mut filters: Vec<String> = Vec::new();

    match e.rot90 % 4 {
        1 => filters.push("transpose=1".into()),
        2 => {
            filters.push("hflip".into());
            filters.push("vflip".into());
        }
        3 => filters.push("transpose=2".into()),
        _ => {}
    }

    let (mut cw, mut ch) = (w, h);
    if e.rot90 % 2 == 1 {
        std::mem::swap(&mut cw, &mut ch);
    }

    if e.fine_angle.abs() > 0.01 {
        let rad = e.fine_angle as f64 * std::f64::consts::PI / 180.0;
        filters.push(format!("rotate={rad}:c=none:ow=rotw(iw):oh=roth(ih)"));
        let (iw, ih) = working_dims(w, h, e);
        filters.push(format!("crop={iw}:{ih}"));
        cw = iw;
        ch = ih;
    }

    let (x, y, px_w, px_h) = crop_px(e, cw, ch);
    if px_w != cw || px_h != ch {
        filters.push(format!("crop={px_w}:{px_h}:{x}:{y}"));
    }

    let (ew, eh) = e.aspect.export_dims(px_w, px_h);
    filters.push(format!(
        "scale={ew}:{eh}:force_original_aspect_ratio=decrease,pad={ew}:{eh}:(ow-iw)/2:(oh-ih)/2"
    ));

    if e.is_color_touched() {
        filters.push(format!(
            "eq=brightness={}:contrast={}:saturation={}",
            e.brightness,
            1.0 + e.contrast,
            1.0 + e.saturation
        ));
        if e.warmth.abs() > 0.01 {
            let wv = e.warmth * 0.3;
            filters.push(format!("colorbalance=rs={wv}:rm={wv}:bs={}:bm={}", -wv, -wv));
        }
    }

    let mut args: Vec<String> = vec!["-i".into(), item.name.clone()];
    if let Some((start, end)) = e.trim {
        args.push("-ss".into());
        args.push(format!("{start}"));
        args.push("-to".into());
        args.push(format!("{end}"));
    }
    args.push("-vf".into());
    args.push(filters.join(","));
    args.extend([
        "-c:v".into(), "libx264".into(),
        "-preset".into(), "veryfast".into(),
        "-crf".into(), "22".into(),
        "-pix_fmt".into(), "yuv420p".into(),
        "-c:a".into(), "aac".into(),
        "-movflags".into(), "+faststart".into(),
        "pes-out.mp4".into(),
    ]);
    args
}

// --- UI ------------------------------------------------------------------------

#[component]
fn App() -> impl IntoView {
    let state = AppState::new();
    provide_context(state);

    view! {
        <div class="app">
            <header>
                <h1>"photo-edit-simplified"</h1>
                <AddButton state=state/>
            </header>
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

fn files_from_list(list: &web_sys::FileList) -> Vec<File> {
    (0..list.length()).filter_map(|i| list.item(i)).collect()
}

#[component]
fn AddButton(state: AppState) -> impl IntoView {
    let input_ref = create_node_ref::<html::Input>();
    view! {
        <label class="btn">
            "＋ Add media"
            <input
                node_ref=input_ref
                type="file"
                accept="image/*,.heic,.heif,video/*"
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
fn DropZone(state: AppState) -> impl IntoView {
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

#[component]
fn FilmStrip(state: AppState) -> impl IntoView {
    view! {
        <div class="filmstrip">
            <For
                each=move || state.items.get()
                key=|m| m.id
                children=move |m: MediaItem| {
                    let badge = if m.kind == MediaKind::Video { "▶" } else { "" };
                    view! {
                        <div
                            class="thumb"
                            class:selected=move || state.selected.get() == Some(m.id)
                            on:click=move |_| state.selected.set(Some(m.id))
                        >
                            <img src=m.object_url.clone()/>
                            <span class="badge">{badge}</span>
                        </div>
                    }
                }
            />
        </div>
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Crop,
    Rotate,
    Color,
    Trim,
    Export,
}

#[component]
fn TabBtn(tab: RwSignal<Tab>, t: Tab, label: &'static str) -> impl IntoView {
    view! {
        <button class="tab" class:active=move || tab.get() == t on:click=move |_| tab.set(t)>
            {label}
        </button>
    }
}

#[component]
fn Editor(state: AppState) -> impl IntoView {
    let tab = create_rw_signal(Tab::Crop);
    view! {
        <Show when=move || state.current().is_some() fallback=|| ()>
            <div class="editor">
                <Preview state=state tab=tab/>
                <div class="tabs">
                    <TabBtn tab=tab t=Tab::Crop label="Crop"/>
                    <TabBtn tab=tab t=Tab::Rotate label="Rotate"/>
                    <TabBtn tab=tab t=Tab::Color label="Color"/>
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
                        Tab::Crop => view! { <CropTab state=state/> }.into_view(),
                        Tab::Rotate => view! { <RotateTab state=state/> }.into_view(),
                        Tab::Color => view! { <ColorTab state=state/> }.into_view(),
                        Tab::Trim => view! { <TrimTab state=state/> }.into_view(),
                        Tab::Export => view! { <ExportTab state=state/> }.into_view(),
                    }}
                </div>
            </div>
        </Show>
    }
}

#[component]
fn Preview(state: AppState, tab: RwSignal<Tab>) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let working = create_rw_signal((1usize, 1usize));

    create_effect(move |_| {
        state.items.track();
        state.selected.track();
        let Some(item) = state.current() else { return };
        if item.kind != MediaKind::Photo {
            return;
        }
        let Some(photo) = get_photo(item.id) else { return };
        let Some(canvas) = canvas_ref.get() else { return };

        let (pix, w, h) = &photo.preview;
        let (mut p, w, h) = geometry(pix, *w, *h, &item.edit);
        working.set((w, h));
        if item.edit.is_color_touched() {
            ops::adjust(
                &mut p,
                item.edit.brightness,
                item.edit.contrast,
                item.edit.saturation,
                item.edit.warmth,
            );
        }
        web::put_pixels(&canvas, &p, w as u32, h as u32);
    });

    view! {
        <div class="preview">
            {move || match state.current() {
                Some(m) if m.kind == MediaKind::Video => {
                    let e = m.edit;
                    let style = format!(
                        "filter: brightness({}) contrast({}) saturate({}); transform: rotate({}deg);",
                        1.0 + e.brightness,
                        1.0 + e.contrast,
                        1.0 + e.saturation,
                        e.fine_angle
                    );
                    view! {
                        <div class="canvas-wrap">
                            <video src=m.object_url.clone() controls style=style></video>
                            <p class="dim">"Preview is approximate — exact render happens at export."</p>
                        </div>
                    }.into_view()
                }
                _ => view! {
                    <div class="canvas-wrap">
                        <canvas node_ref=canvas_ref></canvas>
                        <Show when=move || tab.get() == Tab::Crop fallback=|| ()>
                            <CropOverlay state=state working=working/>
                        </Show>
                    </div>
                }.into_view(),
            }}
        </div>
    }
}

#[component]
fn CropOverlay(state: AppState, working: RwSignal<(usize, usize)>) -> impl IntoView {
    let drag = create_rw_signal(None::<(u8, f32, f32, state::CropRect)>);

    let norm_pos = |ev: &web_sys::PointerEvent| -> Option<(f32, f32)> {
        let el = web::document().query_selector(".canvas-wrap").ok().flatten()?;
        let rect = el.get_bounding_client_rect();
        let nx = ((ev.client_x() as f32 - rect.left() as f32) / rect.width() as f32).clamp(0.0, 1.0);
        let ny = ((ev.client_y() as f32 - rect.top() as f32) / rect.height() as f32).clamp(0.0, 1.0);
        Some((nx, ny))
    };

    let start = move |ev: web_sys::PointerEvent, handle: u8| {
        ev.prevent_default();
        let Some(item) = state.current() else { return };
        let Some((nx, ny)) = norm_pos(&ev) else { return };
        drag.set(Some((handle, nx, ny, item.edit.crop)));
    };

    window_event_listener(leptos::ev::pointermove, move |ev| {
        // Signals are disposed when the Crop tab unmounts, but this window
        // listener outlives them — bail out instead of touching dead signals.
        let Some(Some((handle, sx, sy, orig))) = drag.try_get() else { return };
        let Some(item) = state.current() else { return };
        let Some((ww, wh)) = working.try_get() else { return };
        let Some((nx, ny)) = norm_pos(&ev) else { return };
        let dx = nx - sx;
        let dy = ny - sy;

        let mut c = orig;
        let ratio = item.edit.aspect.ratio().map(|(rw, rh)| rw / rh);
        let img_aspect = ww as f32 / wh as f32;

        match handle {
            0 => {
                c.x = (orig.x + dx).clamp(0.0, 1.0 - orig.w);
                c.y = (orig.y + dy).clamp(0.0, 1.0 - orig.h);
            }
            hdl => {
                let (fx, fy) = match hdl {
                    1 => (orig.x, orig.y),
                    2 => (orig.x + orig.w, orig.y),
                    3 => (orig.x, orig.y + orig.h),
                    _ => (orig.x + orig.w, orig.y + orig.h),
                };
                let mut nw = match hdl {
                    1 | 3 => (orig.w - dx).max(0.02),
                    _ => (orig.w + dx).max(0.02),
                };
                let mut nh = match hdl {
                    1 | 2 => (orig.h - dy).max(0.02),
                    _ => (orig.h + dy).max(0.02),
                };
                if let Some(r) = ratio {
                    nh = nw * img_aspect / r;
                    if fx + nw > 1.0 || fy + nh > 1.0 {
                        nw = (1.0 - fx).min((1.0 - fy) * r / img_aspect);
                        nh = nw * img_aspect / r;
                    }
                }
                c.w = nw.clamp(0.02, 1.0);
                c.h = nh.clamp(0.02, 1.0);
                c.x = match hdl {
                    1 | 3 => (fx + orig.w - c.w).clamp(0.0, 1.0 - c.w),
                    _ => fx.clamp(0.0, 1.0 - c.w),
                };
                c.y = match hdl {
                    1 | 2 => (fy + orig.h - c.h).clamp(0.0, 1.0 - c.h),
                    _ => fy.clamp(0.0, 1.0 - c.h),
                };
            }
        }
        state.update_current(|e| e.crop = c);
    });

    window_event_listener(leptos::ev::pointerup, move |_| {
        let _ = drag.try_set(None);
    });

    view! {
        {move || {
            let Some(item) = state.current() else {
                return view! { <div></div> }.into_view();
            };
            let c = item.edit.crop;
            let pct = |v: f32| format!("{}%", v * 100.0);
            view! {
                <div class="crop-overlay">
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

#[component]
fn CropTab(state: AppState) -> impl IntoView {
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
        <p class="dim">"Drag the crop box on the image. Corners resize."</p>
    }
}

#[component]
fn RotateTab(state: AppState) -> impl IntoView {
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
fn ColorTab(state: AppState) -> impl IntoView {
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
        <button class="btn dim-btn" on:click=move |_| state.update_current(|e| {
            e.brightness = 0.0; e.contrast = 0.0; e.saturation = 0.0; e.warmth = 0.0;
        })>"Reset color"</button>
    }
}

#[component]
fn TrimTab(state: AppState) -> impl IntoView {
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
fn ExportTab(state: AppState) -> impl IntoView {
    let multi = move || state.items.with(|v| v.len() > 1);
    view! {
        <button class="btn primary" on:click=move |_| {
            if let Some(item) = state.current() {
                match item.kind {
                    MediaKind::Photo => export_photo(state, item),
                    MediaKind::Video => export_video(state, item),
                }
            }
        }>"Download this"</button>
        <Show when=multi fallback=|| ()>
            <button class="btn" on:click=move |_| {
                let Some(cur) = state.current() else { return };
                state.items.update(|v| {
                    for m in v.iter_mut() {
                        if m.kind == cur.kind && m.id != cur.id {
                            m.edit = cur.edit;
                        }
                    }
                });
            }>"Apply edits to all"</button>
            <button class="btn" on:click=move |_| {
                for item in state.items.get() {
                    match item.kind {
                        MediaKind::Photo => export_photo(state, item),
                        MediaKind::Video => export_video(state, item),
                    }
                }
            }>"Download all"</button>
        </Show>
        <p class="dim">"Exports match the selected aspect preset’s social-media dimensions."</p>
    }
}

#[component]
fn BusyOverlay(state: AppState) -> impl IntoView {
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
