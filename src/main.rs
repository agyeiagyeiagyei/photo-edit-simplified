mod ops;
mod state;
mod web;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::leptos_dom::helpers::window_event_listener;
use leptos::*;
use leptos::batch;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Blob, File, Url};

use state::{AppState, Aspect, BrushStroke, EditParams, Layer, LayerKind, MediaItem, MediaKind, PathPoint, SelectTool, Selection, SelectionKind, TextAlign, Tool};

// --- media cache (non-reactive) ---------------------------------------------

#[derive(Clone)]
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
            let id = state.next_id.get_untracked();
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
                        layers: Vec::new(),
                        next_layer_id: 0,
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
                    layers: Vec::new(),
                    next_layer_id: 0,
                });
            }
            state.busy.set(None);
        }
    });
}

fn push_item(state: AppState, item: MediaItem) {
    let id = item.id;
    batch(|| {
        state.items.update(|v| v.push(item));
        if state.selected.get_untracked().is_none() {
            state.selected.set(Some(id));
        }
    });
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

// --- layer compositing ---------------------------------------------------------

fn draw_text_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Text(t) = &layer.kind else { return; };
    let px = (t.font_size * h as f32) as f64;
    ctx.set_font(&format!(
        "{} {}px \"{}\", sans-serif",
        t.font_weight, px, t.font_family
    ));
    ctx.set_text_align(t.alignment.canvas_value());
    ctx.set_text_baseline("middle");

    let x = (t.x * w as f32) as f64;
    let y = (t.y * h as f32) as f64;

    ctx.set_shadow_color(&t.shadow_color);
    ctx.set_shadow_blur(t.shadow_blur as f64);
    ctx.set_shadow_offset_x(t.shadow_offset_x as f64);
    ctx.set_shadow_offset_y(t.shadow_offset_y as f64);

    if t.stroke_width > 0.0 {
        ctx.set_line_width(t.stroke_width as f64 * px);
        ctx.set_stroke_style_str(&t.stroke_color);
        let _ = ctx.stroke_text(&t.text, x, y);
    }

    ctx.set_fill_style_str(&t.color);
    let _ = ctx.fill_text(&t.text, x, y);

    ctx.set_shadow_color("transparent");
    ctx.set_shadow_blur(0.0);
    ctx.set_shadow_offset_x(0.0);
    ctx.set_shadow_offset_y(0.0);
}

fn draw_path_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Path(p) = &layer.kind else { return; };
    if p.points.len() < 2 {
        return;
    }
    ctx.begin_path();
    let first = &p.points[0];
    ctx.move_to((first.x * w as f32) as f64, (first.y * h as f32) as f64);
    for i in 1..p.points.len() {
        let prev = &p.points[i - 1];
        let curr = &p.points[i];
        let c1x = (prev.x + prev.out_x) * w as f32;
        let c1y = (prev.y + prev.out_y) * h as f32;
        let c2x = (curr.x + curr.in_x) * w as f32;
        let c2y = (curr.y + curr.in_y) * h as f32;
        let x = curr.x * w as f32;
        let y = curr.y * h as f32;
        ctx.bezier_curve_to(c1x as f64, c1y as f64, c2x as f64, c2y as f64, x as f64, y as f64);
    }
    if p.closed && p.points.len() > 2 {
        let last = p.points.last().unwrap();
        let first = &p.points[0];
        let c1x = (last.x + last.out_x) * w as f32;
        let c1y = (last.y + last.out_y) * h as f32;
        let c2x = (first.x + first.in_x) * w as f32;
        let c2y = (first.y + first.in_y) * h as f32;
        let x = first.x * w as f32;
        let y = first.y * h as f32;
        ctx.bezier_curve_to(c1x as f64, c1y as f64, c2x as f64, c2y as f64, x as f64, y as f64);
        ctx.close_path();
    }

    let stroke_px = (p.stroke_width * h as f32) as f64;
    if !p.fill_color.is_empty() && p.fill_color != "none" {
        ctx.set_fill_style_str(&p.fill_color);
        let _ = ctx.fill();
    }
    if stroke_px > 0.0 {
        ctx.set_line_width(stroke_px);
        ctx.set_stroke_style_str(&p.stroke_color);
        ctx.set_line_join("round");
        let _ = ctx.stroke();
    }
}

fn draw_brush_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Brush(b) = &layer.kind else { return; };
    ctx.set_stroke_style_str(&b.color);
    ctx.set_line_width((b.width * h as f32) as f64);
    ctx.set_line_cap("round");
    ctx.set_line_join("round");
    for stroke in &b.strokes {
        if stroke.points.len() < 2 {
            continue;
        }
        ctx.begin_path();
        let (x0, y0) = stroke.points[0];
        ctx.move_to((x0 * w as f32) as f64, (y0 * h as f32) as f64);
        for &(x, y) in &stroke.points[1..] {
            ctx.line_to((x * w as f32) as f64, (y * h as f32) as f64);
        }
        let _ = ctx.stroke();
    }
}

fn draw_raster_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Raster(r) = &layer.kind else { return };
    if r.width == 0 || r.height == 0 {
        return;
    }
    let canvas = web::create_canvas(r.width as u32, r.height as u32);
    web::put_pixels(&canvas, &r.pixels, r.width as u32, r.height as u32);
    let _ = ctx.draw_image_with_html_canvas_element_and_dw_and_dh(&canvas, 0.0, 0.0, w, h,
    );
}

fn composite_layers(canvas: &web_sys::HtmlCanvasElement, layers: &[state::Layer], w: usize, h: usize) {
    let ctx = web::ctx2d(canvas);
    for layer in layers {
        if !layer.visible || layer.opacity <= 0.01 {
            continue;
        }
        ctx.set_global_alpha(layer.opacity as f64);
        match &layer.kind {
            LayerKind::Text(_) => draw_text_layer(&ctx, layer, w as f64, h as f64),
            LayerKind::Path(_) => draw_path_layer(&ctx, layer, w as f64, h as f64),
            LayerKind::Brush(_) => draw_brush_layer(&ctx, layer, w as f64, h as f64),
            LayerKind::Raster(_) => draw_raster_layer(&ctx, layer, w as f64, h as f64),
        }
        ctx.set_global_alpha(1.0);
    }
}

async fn render_text_overlay(t: &state::TextLayer, w: usize, h: usize) -> Result<web_sys::Blob, JsValue> {
    let canvas = web::create_canvas(w as u32, h as u32);
    let layer = state::Layer {
        id: 0,
        visible: true,
        opacity: 1.0,
        kind: state::LayerKind::Text(t.clone()),
    };
    draw_text_layer(&web::ctx2d(&canvas), &layer, w as f64, h as f64);
    web::canvas_to_blob(&canvas, "image/png").await
}

fn video_export_dims(edit: &EditParams, w: usize, h: usize) -> (usize, usize, usize, usize, usize, usize) {
    let (mut cw, mut ch) = (w, h);
    if edit.rot90 % 2 == 1 {
        std::mem::swap(&mut cw, &mut ch);
    }
    if edit.fine_angle.abs() > 0.01 {
        let (iw, ih) = working_dims(w, h, edit);
        cw = iw;
        ch = ih;
    }
    let (_x, _y, px_w, px_h) = crop_px(edit, cw, ch);
    let (ew, eh) = edit.aspect.export_dims(px_w, px_h);
    (cw, ch, px_w, px_h, ew, eh)
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
        let (mut p, w, h) = geometry(pix, *w, *h, &item.edit);
        if item.edit.is_color_touched() {
            if let Some(sel) = &item.edit.selection {
                let mask = ops::selection_mask(sel, w, h);
                ops::adjust_masked(
                    &mut p,
                    &mask,
                    item.edit.brightness,
                    item.edit.contrast,
                    item.edit.saturation,
                    item.edit.warmth,
                );
            } else {
                ops::adjust(
                    &mut p,
                    item.edit.brightness,
                    item.edit.contrast,
                    item.edit.saturation,
                    item.edit.warmth,
                );
            }
        }

        // Composite onto the full geometry-corrected canvas, then crop.
        let base = web::create_canvas(w as u32, h as u32);
        web::put_pixels(&base, &p, w as u32, h as u32);
        composite_layers(&base, &item.layers, w, h);

        let (cx, cy, cw, ch) = crop_px(&item.edit, w, h);
        let work = web::create_canvas(cw as u32, ch as u32);
        web::ctx2d(&work)
            .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                &base, cx as f64, cy as f64, cw as f64, ch as f64,
                0.0, 0.0, cw as f64, ch as f64,
            )
            .unwrap();

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
        let (_, _, _, _, ew, eh) = video_export_dims(&item.edit, w as usize, h as usize);

        let mut overlays: Vec<(String, Blob)> = Vec::new();
        for (i, layer) in item.layers.iter().enumerate() {
            if !layer.visible || layer.opacity <= 0.01 {
                continue;
            }
            let state::LayerKind::Text(t) = &layer.kind else {
                // Path/brush layers are photo-only for now.
                continue;
            };
            match render_text_overlay(t, ew, eh).await {
                Ok(blob) => overlays.push((format!("overlay-{i}.png"), blob)),
                Err(_) => web::log("text overlay render failed"),
            }
        }

        let overlay_names: Vec<String> = overlays.iter().map(|(n, _)| n.clone()).collect();
        let args = build_ffmpeg_args(&item, w as usize, h as usize, &overlay_names);
        let on_progress = Closure::new(move |r: f64| state.progress.set(r as f32));
        let result = web::video_transcode(&item.name, blob, args, overlays, on_progress).await;
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

fn build_ffmpeg_args(item: &MediaItem, w: usize, h: usize, overlays: &[String]) -> Vec<String> {
    let e = &item.edit;
    let (_, _, px_w, px_h, ew, eh) = video_export_dims(e, w, h);

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

    if e.fine_angle.abs() > 0.01 {
        let rad = e.fine_angle as f64 * std::f64::consts::PI / 180.0;
        filters.push(format!("rotate={rad}:c=none:ow=rotw(iw):oh=roth(ih)"));
        let (iw, ih) = working_dims(w, h, e);
        filters.push(format!("crop={iw}:{ih}"));
    }

    let (x, y, cx, cy) = crop_px(e, px_w, px_h);
    if cx != px_w || cy != px_h {
        filters.push(format!("crop={cx}:{cy}:{x}:{y}"));
    }

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
    for ov in overlays {
        args.push("-i".into());
        args.push(ov.clone());
    }
    if let Some((start, end)) = e.trim {
        args.push("-ss".into());
        args.push(format!("{start}"));
        args.push("-to".into());
        args.push(format!("{end}"));
    }

    if overlays.is_empty() {
        args.push("-vf".into());
        args.push(filters.join(","));
    } else {
        let mut graph = vec![format!("[0:v]{}[base]", filters.join(","))];
        let mut last = "base".to_string();
        for (i, _) in overlays.iter().enumerate() {
            let input_idx = i + 1;
            if i == overlays.len() - 1 {
                graph.push(format!("[{}][{}:v]overlay=0:0", last, input_idx));
            } else {
                let next = format!("v{i}");
                graph.push(format!("[{}][{}:v]overlay=0:0[{}]", last, input_idx, next));
                last = next;
            }
        }
        args.push("-filter_complex".into());
        args.push(graph.join(";"));
    }

    args.extend([
        "-c:v".into(), "libx264".into(),
        "-preset".into(), "veryfast".into(),
        "-crf".into(), "22".into(),
        "-pix_fmt".into(), "yuv420p".into(),
        "-movflags".into(), "+faststart".into(),
    ]);
    if item.edit.keep_audio {
        args.push("-c:a".into());
        args.push("aac".into());
    } else {
        args.push("-an".into());
    }
    args.push("pes-out.mp4".into());
    args
}

// --- UI ------------------------------------------------------------------------

#[component]
fn App() -> impl IntoView {
    let state = AppState::new();
    provide_context(state);

    spawn_local(async move {
        let _ = web::load_font("Inter", "fonts/inter-400.woff2", "400").await;
        let _ = web::load_font("Inter", "fonts/inter-700.woff2", "700").await;
        let _ = web::load_font("Oswald", "fonts/oswald-400.woff2", "400").await;
        let _ = web::load_font("Oswald", "fonts/oswald-700.woff2", "700").await;
    });

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
    Select,
    Layers,
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
                    <TabBtn tab=tab t=Tab::Select label="Select"/>
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
                        Tab::Crop => view! { <CropTab state=state/> }.into_view(),
                        Tab::Rotate => view! { <RotateTab state=state/> }.into_view(),
                        Tab::Color => view! { <ColorTab state=state/> }.into_view(),
                        Tab::Select => view! { <SelectTab state=state/> }.into_view(),
                        Tab::Layers => view! { <LayersTab state=state/> }.into_view(),
                        Tab::Trim => view! { <TrimTab state=state/> }.into_view(),
                        Tab::Export => view! { <ExportTab state=state/> }.into_view(),
                    }}
                </div>
            </div>
        </Show>
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PenHandle {
    Point,
    Out,
    In,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PenDrag {
    MovePoint(usize),
    MoveOut(usize),
    MoveIn(usize),
}

#[derive(Clone, PartialEq, Debug)]
enum SelectDrag {
    Rect((f32, f32), (f32, f32)),
    Lasso(Vec<(f32, f32)>),
}

fn layer_norm_pos(ev: &web_sys::MouseEvent) -> Option<(f32, f32)> {
    let el = web::document().query_selector(".canvas-wrap").ok().flatten()?;
    let rect = el.get_bounding_client_rect();
    let nx = ((ev.client_x() as f32 - rect.left() as f32) / rect.width() as f32).clamp(0.0, 1.0);
    let ny = ((ev.client_y() as f32 - rect.top() as f32) / rect.height() as f32).clamp(0.0, 1.0);
    Some((nx, ny))
}

fn dist2(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    dx * dx + dy * dy
}

fn hit_test_path(points: &[PathPoint], nx: f32, ny: f32) -> Option<(usize, PenHandle)> {
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

fn push_path_history(layer: &mut state::PathLayer) {
    if layer.history.len() >= 50 {
        layer.history.remove(0);
    }
    layer.history.push(layer.points.clone());
}

fn push_brush_history(layer: &mut state::BrushLayer) {
    if layer.history.len() >= 50 {
        layer.history.remove(0);
    }
    layer.history.push(layer.strokes.clone());
}

fn draw_path_edit_handles(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
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
        let _ = ctx.stroke();

        ctx.set_fill_style_str(pt.smooth.then_some("#0a84ff").unwrap_or("#ffcc00"));
        ctx.begin_path();
        ctx.arc(px, py, 6.0, 0.0, std::f64::consts::PI * 2.0).unwrap();
        let _ = ctx.fill();

        ctx.set_fill_style_str("#fff");
        for (hx, hy) in [(ox, oy), (ix, iy)] {
            ctx.begin_path();
            ctx.arc(hx, hy, 4.0, 0.0, std::f64::consts::PI * 2.0).unwrap();
            let _ = ctx.fill();
        }
    }
}

fn draw_selection_overlay(ctx: &web_sys::CanvasRenderingContext2d, sel: &Selection, w: f64, h: f64) {
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
    }
    ctx.restore();
}

fn draw_select_drag(ctx: &web_sys::CanvasRenderingContext2d, drag: &SelectDrag, w: f64, h: f64) {
    ctx.save();
    ctx.set_stroke_style_str("rgba(10,132,255,0.8)");
    ctx.set_line_width(2.0);
    match drag {
        SelectDrag::Rect((x0, y0), (x1, y1)) => {
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

#[component]
fn Preview(state: AppState, tab: RwSignal<Tab>) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let working = create_rw_signal((1usize, 1usize));
    let layer_drag = create_rw_signal(None::<(usize, f32, f32, f32, f32)>);
    let pen_drag = create_rw_signal(None::<(usize, PenDrag, f32, f32)>);
    let brush_draw = create_rw_signal(None::<usize>);
    let select_drag = create_rw_signal(None::<SelectDrag>);

    // --- text layer drag -------------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some((id, nx0, ny0, x0, y0))) = layer_drag.try_get() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev) else { return };
        let dx = nx - nx0;
        let dy = ny - ny0;
        state.update_current_item(|m| {
            if let Some(l) = m.layers.iter_mut().find(|l| l.id == id) {
                let LayerKind::Text(t) = &mut l.kind else { return };
                t.x = (x0 + dx).clamp(0.0, 1.0);
                t.y = (y0 + dy).clamp(0.0, 1.0);
            }
        });
    });

    window_event_listener(leptos::ev::pointerup, move |_| {
        let _ = layer_drag.try_set(None);
    });

    // --- pen tool drag ---------------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some((id, drag, nx0, ny0))) = pen_drag.try_get() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev) else { return };
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

    // --- brush tool drawing ----------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(id) = brush_draw.try_get().flatten() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev) else { return };
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

    // --- select tool drag --------------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some((nx, ny)) = layer_norm_pos(&ev) else { return };
        let Some(Some(drag)) = select_drag.try_get() else { return };
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
        state.items.track();
        state.selected.track();
        state.selected_layer.track();
        state.selected_tool.track();
        state.selected_select_tool.track();
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
            if let Some(sel) = &item.edit.selection {
                let mask = ops::selection_mask(sel, w, h);
                ops::adjust_masked(
                    &mut p,
                    &mask,
                    item.edit.brightness,
                    item.edit.contrast,
                    item.edit.saturation,
                    item.edit.warmth,
                );
            } else {
                ops::adjust(
                    &mut p,
                    item.edit.brightness,
                    item.edit.contrast,
                    item.edit.saturation,
                    item.edit.warmth,
                );
            }
        }
        web::put_pixels(&canvas, &p, w as u32, h as u32);
        composite_layers(&canvas, &item.layers, w, h);

        // Draw editing handles on top for the selected path layer.
        if tab.get() == Tab::Layers && state.selected_tool.get() == Tool::Pen {
            if let Some(sel) = state.selected_layer.get() {
                if let Some(layer) = item.layers.iter().find(|l| l.id == sel) {
                    draw_path_edit_handles(&web::ctx2d(&canvas), layer, w as f64, h as f64);
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
                            <VideoOverlays state=state item=m.clone()/>
                            <p class="dim">"Preview is approximate — exact render happens at export."</p>
                        </div>
                    }.into_view()
                }
                _ => view! {
                    <div
                        class="canvas-wrap"
                        on:pointerdown=move |ev: web_sys::PointerEvent| {
                            if tab.get() == Tab::Select {
                                let Some((nx, ny)) = layer_norm_pos(&ev) else { return; };
                                match state.selected_select_tool.get() {
                                    SelectTool::Rect => {
                                        select_drag.set(Some(SelectDrag::Rect((nx, ny), (nx, ny))));
                                    }
                                    SelectTool::Lasso => {
                                        select_drag.set(Some(SelectDrag::Lasso(vec![(nx, ny)])));
                                    }
                                }
                                return;
                            }
                            if tab.get() != Tab::Layers { return; }
                            let Some(id) = state.selected_layer.get() else { return; };
                            let Some(item) = state.current() else { return; };
                            let Some(layer) = item.layers.iter().find(|l| l.id == id) else { return };
                            let Some((nx, ny)) = layer_norm_pos(&ev) else { return };

                            match state.selected_tool.get() {
                                Tool::Select => {
                                    let LayerKind::Text(t) = &layer.kind else { return };
                                    layer_drag.set(Some((id, nx, ny, t.x, t.y)));
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
                            let Some((nx, ny)) = layer_norm_pos(&ev) else { return; };
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
                    >
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
fn VideoOverlays(state: AppState, item: MediaItem) -> impl IntoView {
    let _ = state;
    view! {
        <div class="video-overlays" style="pointer-events:none">
            {item.layers.iter().filter_map(|l| {
                if !l.visible { return None; }
                let LayerKind::Text(t) = &l.kind else { return None; };
                let px = format!("{:.2}%", t.font_size * 100.0);
                let left = format!("{:.2}%", t.x * 100.0);
                let top = format!("{:.2}%", t.y * 100.0);
                let align = t.alignment.canvas_value();
                let translate = match t.alignment {
                    TextAlign::Left => "translate(0,-50%)",
                    TextAlign::Center => "translate(-50%,-50%)",
                    TextAlign::Right => "translate(-100%,-50%)",
                };
                let shadow = format!(
                    "{}px {}px {}px {}",
                    t.shadow_offset_x, t.shadow_offset_y, t.shadow_blur, t.shadow_color
                );
                let stroke = if t.stroke_width > 0.0 {
                    format!("-webkit-text-stroke: {}em {}", t.stroke_width, t.stroke_color)
                } else {
                    String::new()
                };
                let style = format!(
                    "position:absolute;left:{};top:{};transform:{};font-family:'{}',sans-serif;\
                     font-weight:{};font-size:{};color:{};text-align:{};text-shadow:{};opacity:{};white-space:nowrap;{}",
                    left, top, translate, t.font_family, t.font_weight, px, t.color, align, shadow, l.opacity, stroke
                );
                Some(view! { <div style=style>{t.text.clone()}</div> })
            }).collect_view()}
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

fn downscale_pixels(pixels: &[u8], w: usize, h: usize, long_edge: u32) -> (Vec<u8>, usize, usize) {
    let scale = (long_edge as f32 / w.max(h) as f32).min(1.0);
    let pw = (w as f32 * scale).max(1.0) as u32;
    let ph = (h as f32 * scale).max(1.0) as u32;
    let src = web::create_canvas(w as u32, h as u32);
    web::put_pixels(&src, pixels, w as u32, h as u32);
    let dst = web::create_canvas(pw, ph);
    let ctx = web::ctx2d(&dst);
    ctx.draw_image_with_html_canvas_element_and_dw_and_dh(&src, 0.0, 0.0, pw as f64, ph as f64)
        .unwrap();
    web::image_data_from_ctx(&ctx, pw, ph)
}

fn selection_to_layer(state: AppState, cut: bool) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(sel) = item.edit.selection.clone() else { return; };
    let Some(photo) = get_photo(item.id) else { return; };
    let (full, fw, fh) = photo.full.clone();
    let (base, w, h) = geometry(&full, fw, fh, &item.edit);
    let mask = ops::selection_mask(&sel, w, h);
    let extracted = ops::extract_masked(&base, &mask);
    let next_id = item.next_layer_id;
    state.update_current_item(|m| {
        m.next_layer_id += 1;
        m.layers.push(Layer::new_raster(next_id, extracted, w, h));
    });
    if cut {
        let mut cleared = base;
        for (px, m) in cleared.chunks_exact_mut(4).zip(mask.iter()) {
            let a = *m as f32 / 255.0;
            px[3] = (px[3] as f32 * (1.0 - a)).min(255.0) as u8;
        }
        let preview = downscale_pixels(&cleared, w, h, 1024);
        CACHE.with(|c| {
            if let Some(pd) = c.borrow_mut().photos.get_mut(&item.id) {
                let pd = Rc::make_mut(pd);
                pd.full = (cleared, w, h);
                pd.preview = preview;
            }
        });
        state.update_current(|e| {
            e.rot90 = 0;
            e.fine_angle = 0.0;
            e.crop = state::CropRect::default();
            e.selection = None;
        });
    }
}

fn delete_selection(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(sel) = item.edit.selection.clone() else { return; };
    let Some(photo) = get_photo(item.id) else { return; };
    let (full, fw, fh) = photo.full.clone();
    let (mut base, w, h) = geometry(&full, fw, fh, &item.edit);
    let mask = ops::selection_mask(&sel, w, h);
    for (px, m) in base.chunks_exact_mut(4).zip(mask.iter()) {
        let a = *m as f32 / 255.0;
        px[3] = (px[3] as f32 * (1.0 - a)).min(255.0) as u8;
    }
    let preview = downscale_pixels(&base, w, h, 1024);
    CACHE.with(|c| {
        if let Some(pd) = c.borrow_mut().photos.get_mut(&item.id) {
            let pd = Rc::make_mut(pd);
            pd.full = (base, w, h);
            pd.preview = preview;
        }
    });
    state.update_current(|e| {
        e.rot90 = 0;
        e.fine_angle = 0.0;
        e.crop = state::CropRect::default();
        e.selection = None;
    });
}

fn isolate_subject(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(photo) = get_photo(item.id) else { return; };
    let (pix, pw, ph) = photo.preview.clone();
    state.busy.set(Some("Segmenting subject…".into()));
    spawn_local(async move {
        let canvas = web::create_canvas(pw as u32, ph as u32);
        web::put_pixels(&canvas, &pix, pw as u32, ph as u32);
        let Ok(blob) = web::canvas_to_blob(&canvas, "image/jpeg").await else {
            state.busy.set(None);
            return;
        };
        let Ok((img, url)) = web::load_image(&blob).await else {
            state.busy.set(None);
            return;
        };
        match web::segment_selfie(&img).await {
            Ok(mask) => {
                let _ = Url::revoke_object_url(&url);
                if mask.len() != pw * ph {
                    web::log("segmentation mask size mismatch");
                    state.busy.set(None);
                    return;
                }
                let mut bg = pix.clone();
                ops::box_blur_rgba(&mut bg, pw, ph, 8);
                ops::darken(&mut bg, 0.3);
                let inv_mask: Vec<u8> = mask.iter().map(|m| 255 - m).collect();
                let bg = ops::extract_masked(&bg, &inv_mask);
                let subject = ops::extract_masked(&pix, &mask);
                let mut next_id = 0;
                state.update_current_item(|m| {
                    let id0 = m.next_layer_id;
                    m.next_layer_id += 1;
                    let id1 = m.next_layer_id;
                    m.next_layer_id += 1;
                    m.layers.push(Layer::new_raster(id0, bg, pw, ph));
                    m.layers.push(Layer::new_raster(id1, subject, pw, ph));
                    next_id = id1;
                });
                state.selected_layer.set(Some(next_id));
                state.busy.set(None);
            }
            Err(e) => {
                let _ = Url::revoke_object_url(&url);
                web::log_err(&format!("segmentation failed: {e:?}"));
                state.busy.set(None);
            }
        }
    });
}

#[component]
fn SelectTab(state: AppState) -> impl IntoView {
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
                    on:click=move |_| state.update_current(|e| e.selection = None)
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
                "Isolate subject"
            </button>
            <p class="dim">
                "Rect/Lasso selects a region. Cut/Copy lift it to a raster layer. \
                 Isolate subject runs MediaPipe Selfie Segmentation (photo-only)."
            </p>
        </div>
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
fn LayersTab(state: AppState) -> impl IntoView {
    let add_layer = move |kind: &str| {
        let mut new_id = None;
        state.update_current_item(|m| {
            let id = m.next_layer_id;
            m.next_layer_id += 1;
            let layer = match kind {
                "text" => Layer::new_text(id),
                "path" => Layer::new_path(id),
                "brush" => Layer::new_brush(id),
                _ => Layer::new_text(id),
            };
            m.layers.push(layer);
            new_id = Some(id);
        });
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
        state.update_current_item(|m| {
            m.layers.retain(|l| l.id != id);
        });
        if state.selected_layer.get() == Some(id) {
            state.selected_layer.set(None);
        }
    };

    let move_layer = move |id: usize, delta: isize| {
        state.update_current_item(|m| {
            let idx = m.layers.iter().position(|l| l.id == id);
            if let Some(i) = idx {
                let new_i = (i as isize + delta)
                    .clamp(0, m.layers.len() as isize - 1) as usize;
                if new_i != i {
                    m.layers.swap(i, new_i);
                }
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
    };

    let layers = create_memo(move |_| state.current_layers());

    view! {
        <div class="layers-tab">
            <div class="row layer-add-row">
                <button class="btn" on:click=move |_| add_layer("text")>"＋ Text"</button>
                <button class="btn" on:click=move |_| add_layer("path")>"＋ Path"</button>
                <button class="btn" on:click=move |_| add_layer("brush")>"＋ Brush"</button>
            </div>
            <div class="layer-list">
                <For
                    each=move || layers.get()
                    key=|l| l.id
                    children=move |l: Layer| {
                        let id = l.id;
                        let layer = create_memo(move |_| {
                            layers.get().into_iter().find(|l| l.id == id)
                        });
                        let is_selected = move || state.selected_layer.get() == Some(id);
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
                                    {move || layer.get().map(|l| l.label()).unwrap_or_default()}
                                </span>
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
                                    on:click=move |ev| {
                                        ev.stop_propagation();
                                        move_layer(id, -1);
                                    }
                                >
                                    "↑"
                                </button>
                                <button
                                    class="icon-btn"
                                    on:click=move |ev| {
                                        ev.stop_propagation();
                                        move_layer(id, 1);
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
            <ToolBar state=state/>
            <TextLayerEditor state=state/>
            <PathLayerEditor state=state/>
            <BrushLayerEditor state=state/>
        </div>
    }
}

#[component]
fn ToolBar(state: AppState) -> impl IntoView {
    view! {
        <div class="toolbar">
            {Tool::ALL.map(|t| {
                view! {
                    <button
                        class="chip"
                        class:active=move || state.selected_tool.get() == t
                        on:click=move |_| state.selected_tool.set(t)
                    >
                        {t.label()}
                    </button>
                }
            })}
        </div>
    }
}

fn with_text_layer(state: AppState, f: impl FnOnce(&mut state::TextLayer)) {
    let Some(sel) = state.selected_layer.get_untracked() else { return };
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == sel) {
            let LayerKind::Text(t) = &mut l.kind else { return };
            f(t);
        }
    });
}

fn selected_text_layer(state: AppState) -> Option<state::TextLayer> {
    let sel = state.selected_layer.get()?;
    state.current()?.layers.iter().find(|l| l.id == sel).and_then(|l| {
        match &l.kind {
            LayerKind::Text(t) => Some(t.clone()),
            _ => None,
        }
    })
}

fn with_path_layer(state: AppState, f: impl FnOnce(&mut state::PathLayer)) {
    let Some(sel) = state.selected_layer.get_untracked() else { return };
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == sel) {
            let LayerKind::Path(p) = &mut l.kind else { return };
            f(p);
        }
    });
}

fn selected_path_layer(state: AppState) -> Option<state::PathLayer> {
    let sel = state.selected_layer.get()?;
    state.current()?.layers.iter().find(|l| l.id == sel).and_then(|l| {
        match &l.kind {
            LayerKind::Path(p) => Some(p.clone()),
            _ => None,
        }
    })
}

fn with_brush_layer(state: AppState, f: impl FnOnce(&mut state::BrushLayer)) {
    let Some(sel) = state.selected_layer.get_untracked() else { return };
    state.update_current_item(|m| {
        if let Some(l) = m.layers.iter_mut().find(|l| l.id == sel) {
            let LayerKind::Brush(b) = &mut l.kind else { return };
            f(b);
        }
    });
}

fn selected_brush_layer(state: AppState) -> Option<state::BrushLayer> {
    let sel = state.selected_layer.get()?;
    state.current()?.layers.iter().find(|l| l.id == sel).and_then(|l| {
        match &l.kind {
            LayerKind::Brush(b) => Some(b.clone()),
            _ => None,
        }
    })
}

#[component]
fn TextLayerEditor(state: AppState) -> impl IntoView {
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
fn PathLayerEditor(state: AppState) -> impl IntoView {
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
fn BrushLayerEditor(state: AppState) -> impl IntoView {
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
    let is_video = move || state.current().map(|m| m.kind == MediaKind::Video).unwrap_or(false);
    let keep_audio = move || state.current().map(|m| m.edit.keep_audio).unwrap_or(true);
    view! {
        <button class="btn primary" on:click=move |_| {
            if let Some(item) = state.current() {
                match item.kind {
                    MediaKind::Photo => export_photo(state, item),
                    MediaKind::Video => export_video(state, item),
                }
            }
        }>"Download this"</button>
        <Show when=is_video fallback=|| ()>
            <label class="row" style="justify-content:flex-start;gap:0.5rem">
                <input
                    type="checkbox"
                    prop:checked=move || keep_audio()
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
