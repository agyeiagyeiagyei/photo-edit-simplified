mod blur;
mod clone;
mod curves;
mod fill;
mod heal;
mod levels;
mod noise;
mod ops;
mod raw;
mod state;
mod wand;
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
            } else if raw::is_raw_name(&name) {
                state.busy.set(Some(format!("Converting RAW {name}…")));
                let Ok(buf) = web::read_file_array_buffer(&file).await else {
                    state.busy.set(None);
                    continue;
                };
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                let Ok((full_rgba, fw, fh)) = raw::decode_raw(&bytes) else {
                    web::log(&format!("RAW decode failed for {name}"));
                    state.busy.set(None);
                    continue;
                };
                let (pp, pw, ph) = downscale_pixels(&full_rgba, fw, fh, 1024);
                let canvas = web::create_canvas(pw as u32, ph as u32);
                web::put_pixels(&canvas, &pp, pw as u32, ph as u32);
                let url = canvas.to_data_url().unwrap_or_default();
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData { full: (full_rgba, fw, fh), preview: (pp, pw, ph) }),
                    )
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
                blur::blur_apply_masked(&mut p, &mask, w, h, item.edit.blur);
                if !item.edit.levels.is_identity() {
                    levels::levels_apply_masked(&mut p, &mask, &item.edit.levels.build_tables());
                }
                item.edit.curves.apply_masked(&mut p, &mask);
                ops::adjust_masked(
                    &mut p,
                    &mask,
                    item.edit.brightness,
                    item.edit.contrast,
                    item.edit.saturation,
                    item.edit.warmth,
                );
                noise::noise_add_masked(&mut p, &mask, w, h, item.edit.grain, item.edit.grain_gaussian, item.edit.grain_mono, item.id as u32);
            } else {
                blur::blur_apply(&mut p, w, h, item.edit.blur);
                if !item.edit.levels.is_identity() {
                    levels::levels_apply(&mut p, &item.edit.levels.build_tables());
                }
                item.edit.curves.apply(&mut p);
                ops::adjust(
                    &mut p,
                    item.edit.brightness,
                    item.edit.contrast,
                    item.edit.saturation,
                    item.edit.warmth,
                );
                noise::noise_add(&mut p, w, h, item.edit.grain, item.edit.grain_gaussian, item.edit.grain_mono, item.id as u32);
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
    Heal,
    Clone,
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
                        Tab::Crop => view! { <CropTab state=state/> }.into_view(),
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
        SelectionKind::Mask { .. } => {
            let (wi, hi) = (w as usize, h as usize);
            let mask = ops::selection_mask(sel, wi, hi);
            let mut tint = vec![0u8; wi * hi * 4];
            for (i, &m) in mask.iter().enumerate() {
                if m == 0 {
                    continue;
                }
                tint[i * 4] = 10;
                tint[i * 4 + 1] = 132;
                tint[i * 4 + 2] = 255;
                tint[i * 4 + 3] = m / 3;
            }
            let tmp = web::create_canvas(wi as u32, hi as u32);
            web::put_pixels(&tmp, &tint, wi as u32, hi as u32);
            let _ = ctx.draw_image_with_html_canvas_element(&tmp, 0.0, 0.0);
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

/// Heal-brush preview: soft translucent discs along the in-progress stroke.
fn draw_heal_overlay(
    ctx: &web_sys::CanvasRenderingContext2d,
    pts: Option<&[(f32, f32)]>,
    radius_frac: f32,
    w: f64,
    h: f64,
) {
    let Some(pts) = pts else { return };
    if pts.is_empty() {
        return;
    }
    let r = radius_frac as f64 * (w * w + h * h).sqrt();
    ctx.save();
    ctx.set_fill_style_str("rgba(255,255,255,0.35)");
    ctx.set_stroke_style_str("rgba(255,255,255,0.9)");
    ctx.set_line_width(1.5);
    for &(nx, ny) in pts {
        ctx.begin_path();
        let _ = ctx.arc(nx as f64 * w, ny as f64 * h, r, 0.0, std::f64::consts::TAU);
        ctx.fill();
        ctx.stroke();
    }
    ctx.restore();
}

fn draw_crosshair(ctx: &web_sys::CanvasRenderingContext2d, x: f32, y: f32) {
    ctx.save();
    ctx.set_stroke_style_str("rgba(10,132,255,0.9)");
    ctx.set_line_width(1.5);
    ctx.begin_path();
    ctx.move_to(x as f64 - 9.0, y as f64);
    ctx.line_to(x as f64 + 9.0, y as f64);
    ctx.move_to(x as f64, y as f64 - 9.0);
    ctx.line_to(x as f64, y as f64 + 9.0);
    ctx.stroke();
    ctx.begin_path();
    let _ = ctx.arc(x as f64, y as f64, 4.0, 0.0, std::f64::consts::TAU);
    ctx.stroke();
    ctx.restore();
}

fn text_overlay_style(t: &state::TextLayer, opacity: f32) -> String {
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
    format!(
        "position:absolute;left:{};top:{};transform:{};font-family:'{}',sans-serif;\
         font-weight:{};font-size:{};color:{};text-align:{};text-shadow:{};opacity:{};white-space:nowrap;{}",
        left, top, translate, t.font_family, t.font_weight, px, t.color, align, shadow, opacity, stroke
    )
}

fn selected_text_overlay_style(t: &state::TextLayer, opacity: f32) -> String {
    let px = format!("{:.2}%", t.font_size * 100.0);
    let left = format!("{:.2}%", t.x * 100.0);
    let top = format!("{:.2}%", t.y * 100.0);
    let align = t.alignment.canvas_value();
    let translate = match t.alignment {
        TextAlign::Left => "translate(0,-50%)",
        TextAlign::Center => "translate(-50%,-50%)",
        TextAlign::Right => "translate(-100%,-50%)",
    };
    format!(
        "position:absolute;left:{};top:{};transform:{};font-family:'{}',sans-serif;\
         font-weight:{};font-size:{};color:transparent;text-align:{};opacity:{};white-space:nowrap;\
         border:2px dashed #0a84ff;border-radius:4px;background:rgba(10,132,255,0.08);",
        left, top, translate, t.font_family, t.font_weight, px, align, opacity
    )
}

#[component]
fn Preview(state: AppState, tab: RwSignal<Tab>) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let working = create_rw_signal((1usize, 1usize));
    let layer_drag = create_rw_signal(None::<(usize, f32, f32, f32, f32)>);
    let pen_drag = create_rw_signal(None::<(usize, PenDrag, f32, f32)>);
    let brush_draw = create_rw_signal(None::<usize>);
    let select_drag = create_rw_signal(None::<SelectDrag>);
    let heal_drag = create_rw_signal(None::<Vec<(f32, f32)>>);
    let clone_drag = create_rw_signal(None::<Vec<(f32, f32)>>);

    // --- clone stamp stroke ----------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some((nx, ny)) = layer_norm_pos(&ev) else { return };
        let Some(Some(mut pts)) = clone_drag.try_get() else { return };
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

    // --- heal brush stroke -----------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some((nx, ny)) = layer_norm_pos(&ev) else { return };
        let Some(Some(mut pts)) = heal_drag.try_get() else { return };
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
        heal_drag.track();
        state.heal_radius.track();
        clone_drag.track();
        state.clone_source.track();
        state.clone_offset.track();
        state.clone_radius.track();
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
                blur::blur_apply_masked(&mut p, &mask, w, h, item.edit.blur);
                if !item.edit.levels.is_identity() {
                    levels::levels_apply_masked(&mut p, &mask, &item.edit.levels.build_tables());
                }
                item.edit.curves.apply_masked(&mut p, &mask);
                ops::adjust_masked(
                    &mut p,
                    &mask,
                    item.edit.brightness,
                    item.edit.contrast,
                    item.edit.saturation,
                    item.edit.warmth,
                );
                noise::noise_add_masked(&mut p, &mask, w, h, item.edit.grain, item.edit.grain_gaussian, item.edit.grain_mono, item.id as u32);
            } else {
                blur::blur_apply(&mut p, w, h, item.edit.blur);
                if !item.edit.levels.is_identity() {
                    levels::levels_apply(&mut p, &item.edit.levels.build_tables());
                }
                item.edit.curves.apply(&mut p);
                ops::adjust(
                    &mut p,
                    item.edit.brightness,
                    item.edit.contrast,
                    item.edit.saturation,
                    item.edit.warmth,
                );
                noise::noise_add(&mut p, w, h, item.edit.grain, item.edit.grain_gaussian, item.edit.grain_mono, item.id as u32);
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
                        on:pointerdown=move |ev: web_sys::PointerEvent| {
                            if tab.get() == Tab::Clone {
                                if state.current().map(|m| m.kind == MediaKind::Photo).unwrap_or(false) {
                                    if let Some((nx, ny)) = layer_norm_pos(&ev) {
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
                                if state.current().map(|m| m.kind == MediaKind::Photo).unwrap_or(false) {
                                    if let Some((nx, ny)) = layer_norm_pos(&ev) {
                                        heal_drag.set(Some(vec![(nx, ny)]));
                                    }
                                }
                                return;
                            }
                            if tab.get() == Tab::Select {
                                let Some((nx, ny)) = layer_norm_pos(&ev) else { return; };
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
                        <SelectedTextOverlay state=state tab=tab layer_drag=layer_drag/>
                    </div>
                }.into_view(),
            }}
        </div>
    }
}

#[component]
fn VideoOverlays(state: AppState) -> impl IntoView {
    view! {
        <div class="video-overlays">
            {move || state.current_layers().into_iter().filter_map(|l| {
                if !l.visible { return None; }
                let LayerKind::Text(t) = &l.kind else { return None; };
                let style = text_overlay_style(t, l.opacity);
                Some(view! { <div class="video-text-overlay" style=style>{t.text.clone()}</div> })
            }).collect_view()}
        </div>
    }
}

#[component]
fn SelectedTextOverlay(
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
        Some((layer.id, t.clone(), layer.opacity))
    };

    view! {
        {move || {
            let (id, t, opacity) = maybe_layer()?;
            let style = selected_text_overlay_style(&t, opacity);
            Some(view! {
                <div
                    class="text-overlay selected"
                    style=style
                    on:pointerdown=move |ev: web_sys::PointerEvent| {
                        if tab.get() != Tab::Layers || state.selected_tool.get() != Tool::Select { return; }
                        ev.stop_propagation();
                        ev.prevent_default();
                        let Some((nx, ny)) = layer_norm_pos(&ev) else { return; };
                        state.selected_layer.set(Some(id));
                        layer_drag.set(Some((id, nx, ny, t.x, t.y)));
                    }
                >{t.text.clone()}</div>
            })
        }}
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

/// Content-aware fill: synthesize the selection from surrounding texture
/// (PatchMatch-style), destructive on the base pixels like delete_selection.
fn content_fill_selection(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(sel) = item.edit.selection.clone() else { return; };
    let Some(photo) = get_photo(item.id) else { return; };
    let (full, fw, fh) = photo.full.clone();
    let (mut base, w, h) = geometry(&full, fw, fh, &item.edit);
    let mask = ops::selection_mask(&sel, w, h);
    if !fill::content_fill(&mut base, &mask, w, h) {
        return;
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

/// Spot heal: stamp soft discs along the stroke (normalized coords) into a
/// coverage mask at full-res, run the patch-synthesis solver, and swap the
/// cached pixels — destructive, mirroring delete_selection.
fn apply_heal(state: AppState, pts: &[(f32, f32)]) {
    if pts.is_empty() {
        return;
    }
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(photo) = get_photo(item.id) else { return };
    let (full, fw, fh) = photo.full.clone();
    let (mut base, w, h) = geometry(&full, fw, fh, &item.edit);
    let diag = ((w * w + h * h) as f32).sqrt();
    let r = (state.heal_radius.get_untracked() * diag).max(1.0);
    let mut coverage = vec![0u8; w * h];
    stamp_stroke(&mut coverage, w, h, pts, r);
    let mode = state.heal_mode.get_untracked();
    if !heal::spot_heal(&mut base, &coverage, w, h, 1.0, mode, item.id as u32) {
        return;
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

/// Rasterize a polyline stroke into a coverage mask as soft discs of radius r
/// (255 at the center, feathering to 0 at the edge), taking the max on overlap.
fn stamp_stroke(coverage: &mut [u8], w: usize, h: usize, pts: &[(f32, f32)], r: f32) {
    let mut stamp = |cx: f32, cy: f32| {
        let x0 = (cx - r).floor().max(0.0) as usize;
        let y0 = (cy - r).floor().max(0.0) as usize;
        let x1 = ((cx + r).ceil() as usize).min(w - 1);
        let y1 = ((cy + r).ceil() as usize).min(h - 1);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                if d < r {
                    // Plateau to 60% of the radius, linear feather beyond —
                    // overlapping stamps keep full strength between centers.
                    let v = if d <= r * 0.6 {
                        255
                    } else {
                        (255.0 * (r - d) / (r * 0.4)) as u8
                    };
                    let i = y * w + x;
                    coverage[i] = coverage[i].max(v);
                }
            }
        }
    };
    let step = (r / 3.0).max(1.0);
    for pair in pts.windows(2) {
        let (ax, ay) = (pair[0].0 * w as f32, pair[0].1 * h as f32);
        let (bx, by) = (pair[1].0 * w as f32, pair[1].1 * h as f32);
        let len = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
        let n = (len / step).ceil().max(1.0) as usize;
        for i in 0..=n {
            let t = i as f32 / n as f32;
            stamp(ax + (bx - ax) * t, ay + (by - ay) * t);
        }
    }
    if pts.len() == 1 {
        stamp(pts[0].0 * w as f32, pts[0].1 * h as f32);
    }
}

/// Clone stamp: copy from the source point through the stroke's coverage mask
/// at full-res. Aligned strokes keep the first stroke's offset; unaligned
/// strokes re-derive it from the source each time. Destructive like apply_heal.
fn apply_clone(state: AppState, pts: &[(f32, f32)]) {
    if pts.is_empty() {
        return;
    }
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(src) = state.clone_source.get_untracked() else { return };
    let Some(photo) = get_photo(item.id) else { return };
    let (full, fw, fh) = photo.full.clone();
    let (mut base, w, h) = geometry(&full, fw, fh, &item.edit);
    let aligned = state.clone_aligned.get_untracked();
    let offset = if aligned {
        match state.clone_offset.get_untracked() {
            Some(off) => off,
            None => {
                let off = (src.0 - pts[0].0, src.1 - pts[0].1);
                state.clone_offset.set(Some(off));
                off
            }
        }
    } else {
        (src.0 - pts[0].0, src.1 - pts[0].1)
    };
    let dx = (offset.0 * w as f32).round() as i32;
    let dy = (offset.1 * h as f32).round() as i32;
    let diag = ((w * w + h * h) as f32).sqrt();
    let r = (state.clone_radius.get_untracked() * diag).max(1.0);
    let mut coverage = vec![0u8; w * h];
    stamp_stroke(&mut coverage, w, h, pts, r);
    clone::clone_stamp(&mut base, w, h, &coverage, dx, dy);
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

/// Magic wand: flood-select pixels similar to the clicked one, sampled from
/// the color-adjusted preview (what the user sees, minus selection masking).
fn wand_select(state: AppState, nx: f32, ny: f32) {
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(photo) = get_photo(item.id) else { return };
    let (pix, pw, ph) = &photo.preview;
    let (mut p, w, h) = geometry(pix, *pw, *ph, &item.edit);
    if item.edit.is_color_touched() {
        blur::blur_apply(&mut p, w, h, item.edit.blur);
        if !item.edit.levels.is_identity() {
            levels::levels_apply(&mut p, &item.edit.levels.build_tables());
        }
        item.edit.curves.apply(&mut p);
        ops::adjust(
            &mut p,
            item.edit.brightness,
            item.edit.contrast,
            item.edit.saturation,
            item.edit.warmth,
        );
    }
    let sx = ((nx * w as f32) as usize).min(w - 1);
    let sy = ((ny * h as f32) as usize).min(h - 1);
    let tol = state.wand_tolerance.get_untracked();
    let contig = state.wand_contiguous.get_untracked();
    let (mask, count) = wand::wand_mask(&p, w, h, sx, sy, 0, tol, contig);
    state.update_current(|e| {
        e.selection = if count > 0 {
            Some(state::Selection {
                kind: state::SelectionKind::Mask { data: mask, width: w, height: h },
                feather: 0.0,
            })
        } else {
            None
        };
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
                "Rect/Lasso select a region; Wand selects pixels similar to the one you click. \
                 Cut/Copy lift it to a raster layer; Content fill synthesizes it from the \
                 surroundings. \
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
        <BlurPanel state=state />
        <LevelsPanel state=state />
        <CurvesPanel state=state />
        <GrainPanel state=state />
        <button class="btn dim-btn" on:click=move |_| state.update_current(|e| {
            e.brightness = 0.0; e.contrast = 0.0; e.saturation = 0.0; e.warmth = 0.0;
        })>"Reset color"</button>
    }
}

#[component]
fn BlurPanel(state: AppState) -> impl IntoView {
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

#[component]
fn LevelsPanel(state: AppState) -> impl IntoView {
    let channel = create_rw_signal(0usize); // 0=RGB, 1=R, 2=G, 3=B
    let hist_ref = create_node_ref::<html::Canvas>();

    // Redraw the histogram whenever the photo or its edits change.
    create_effect(move |_| {
        state.items.track();
        state.selected.track();
        let Some(canvas) = hist_ref.get() else { return };
        let Some(item) = state.current() else { return };
        if item.kind != MediaKind::Photo {
            return;
        }
        let Some(photo) = get_photo(item.id) else { return };
        let bins = levels::histogram(&photo.preview.0);
        let scale = levels::histogram_display_scale(&bins[0]);
        let ctx = web::ctx2d(&canvas);
        ctx.clear_rect(0.0, 0.0, 256.0, 64.0);
        if scale <= 0.0 {
            return;
        }
        ctx.set_fill_style_str("rgba(255,255,255,0.75)");
        for (i, &b) in bins[0].iter().enumerate() {
            let h = (b / scale * 64.0).min(64.0);
            if h >= 0.5 {
                ctx.fill_rect(i as f64, 64.0 - h, 1.0, h);
            }
        }
    });

    let channel_btn = move |idx: usize, label: &'static str| {
        view! {
            <button
                class=move || if channel.get() == idx { "btn small active" } else { "btn small" }
                on:click=move |_| channel.set(idx)
            >{label}</button>
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
                    state.current()
                        .map(|m| fmt(get(&m.edit.levels.ranges[channel.get()])))
                        .unwrap_or_else(|| "—".into())
                }}</span>
                <input
                    type="range" min=min.to_string() max=max.to_string() step=step.to_string()
                    prop:value=move || state.current()
                        .map(|m| get(&m.edit.levels.ranges[channel.get()]).to_string())
                        .unwrap_or_else(|| "0".into())
                    on:input=move |ev| {
                        let v: f64 = event_target_value(&ev).parse().unwrap_or(0.0);
                        let ch = channel.get_untracked();
                        state.update_current(|e| set(&mut e.levels.ranges[ch], v));
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
        state.update_current(|e| e.levels = s);
    };

    view! {
        <div class="levels-panel">
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
                <button class="btn small" on:click=move |_| auto(levels::AutoMode::Contrast)>"Auto contrast"</button>
                <button class="btn small" on:click=move |_| auto(levels::AutoMode::Color)>"Auto color"</button>
                <button class="btn small" on:click=move |_| auto(levels::AutoMode::ColorNeutral)>"Auto neutral"</button>
                <button class="btn small dim-btn" on:click=move |_| {
                    state.update_current(|e| e.levels = levels::LevelsSettings::default());
                }>"Reset levels"</button>
            </div>
        </div>
    }
}

#[component]
fn CurvesPanel(state: AppState) -> impl IntoView {
    let channel = create_rw_signal(0usize); // 0=RGB, 1=R, 2=G, 3=B
    let selected = create_rw_signal(None::<usize>);
    let dragging = store_value(None::<usize>);
    let canvas_ref = create_node_ref::<html::Canvas>();

    // Redraw the curve whenever the photo's edits, channel, or selection change.
    create_effect(move |_| {
        state.items.track();
        channel.track();
        selected.track();
        let Some(canvas) = canvas_ref.get() else { return };
        let Some(item) = state.current() else { return };
        let ch = channel.get_untracked();
        let sel = selected.get_untracked();
        let pts = &item.edit.curves.channels[ch];
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
        ctx.set_stroke_style_str("#ffffff");
        ctx.set_line_width(2.0);
        ctx.begin_path();
        for x in 0..=255 {
            let y = item.edit.curves.value(x as f64, ch);
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
            ctx.set_fill_style_str(if sel == Some(i) { "#0a84ff" } else { "#ffffff" });
            ctx.fill();
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
        let Some(item) = state.current() else { return };
        let pts = item.edit.curves.channels[ch].clone();
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
            state.update_current(|e| e.curves.channels[ch] = new_pts);
            dragging.set_value(Some(idx));
            selected.set(Some(idx));
        }
    };

    let on_move = move |ev: web_sys::PointerEvent| {
        let Some(i) = dragging.get_value() else { return };
        let Some((x, y)) = to_curve(&ev) else { return };
        let ch = channel.get_untracked();
        state.update_current(|e| {
            let pts = &mut e.curves.channels[ch];
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

    let channel_btn = move |idx: usize, label: &'static str| {
        view! {
            <button
                class=move || if channel.get() == idx { "btn small active" } else { "btn small" }
                on:click=move |_| {
                    channel.set(idx);
                    selected.set(None);
                    dragging.set_value(None);
                }
            >{label}</button>
        }
    };

    let removable = move || {
        let Some(i) = selected.get() else { return false };
        state
            .current()
            .map(|m| {
                let n = m.edit.curves.channels[channel.get()].len();
                i > 0 && i < n - 1
            })
            .unwrap_or(false)
    };

    view! {
        <div class="curves-panel">
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
                    let i = selected.get();
                    i.and_then(|i| {
                        state.current().and_then(|m| {
                            m.edit.curves.channels[channel.get()]
                                .get(i)
                                .map(|p| format!("Input {} · Output {}", p.x as i32, p.y as i32))
                        })
                    })
                    .unwrap_or_else(|| "Click to add a point. Drag to adjust.".into())
                }}</span>
            </div>
            <div class="levels-auto">
                <button class="btn small" disabled=move || !removable() on:click=move |_| {
                    if let Some(i) = selected.get_untracked() {
                        let ch = channel.get_untracked();
                        state.update_current(|e| {
                            let pts = &mut e.curves.channels[ch];
                            if i > 0 && i < pts.len() - 1 {
                                pts.remove(i);
                            }
                        });
                        selected.set(None);
                    }
                }>"Remove point"</button>
                <button class="btn small dim-btn" on:click=move |_| {
                    let ch = channel.get_untracked();
                    state.update_current(|e| {
                        e.curves.channels[ch] = vec![
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
fn GrainPanel(state: AppState) -> impl IntoView {
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
fn HealTab(state: AppState) -> impl IntoView {
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
fn CloneTab(state: AppState) -> impl IntoView {
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
