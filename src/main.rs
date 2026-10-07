mod blur;
mod clone;
mod curves;
mod drive;
mod exif;
mod fill;
mod heal;
mod levels;
mod noise;
mod ops;
mod raw;
mod state;
mod stroke;
mod wand;
mod web;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use leptos::leptos_dom::helpers::window_event_listener;
use leptos::*;
use leptos::batch;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Blob, File, Url};

use state::{AppState, Aspect, BrushStroke, DriveFilter, DriveSort, EditParams, Layer, LayerKind, MediaItem, MediaKind, PathPoint, PhotoFormat, SelectTool, Selection, SelectionKind, TextAlign, Tool};

// --- media cache (non-reactive) ---------------------------------------------

#[derive(Clone)]
struct PhotoData {
    full: (Vec<u8>, usize, usize),
    preview: (Vec<u8>, usize, usize),
    /// Bumped whenever full/preview are rebuilt in place (heal, cut, …) so the
    /// hi-res render cache knows its source pixels changed.
    gen: u64,
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

/// Drop an item from the filmstrip: revoke its object URLs and free its
/// cached pixels/video blob.
fn remove_item(state: AppState, item_id: usize) {
    let mut removed = None;
    state.items.update(|v| {
        if let Some(pos) = v.iter().position(|m| m.id == item_id) {
            let m = v.remove(pos);
            removed = Some((m.object_url, m.thumb_url));
        }
    });
    if let Some((url, thumb)) = removed {
        let _ = Url::revoke_object_url(&url);
        let _ = Url::revoke_object_url(&thumb);
    }
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.photos.remove(&item_id);
        c.video_blobs.remove(&item_id);
        c.video_meta.remove(&item_id);
    });
    if state.selected.get_untracked() == Some(item_id) {
        let next = state.items.with(|v| v.first().map(|m| m.id));
        state.selected.set(next);
    }
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
                        object_url: url.clone(),
                        thumb_url: url,
                        width: w as usize,
                        height: h as usize,
                        edit: EditParams::default(),
                        layers: Vec::new(),
                        next_layer_id: 0,
                        exif: None,
                        drive_file_id: None,
                        drive_parent_id: None,
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
                let exif = raw::extract_exif_tiff(&bytes).map(Rc::new);
                let (pp, pw, ph) = downscale_pixels(&full_rgba, fw, fh, 1024);
                let canvas = web::create_canvas(pw as u32, ph as u32);
                web::put_pixels(&canvas, &pp, pw as u32, ph as u32);
                // Blob URL, not a data URL: a 1024px base64 PNG is multi-MB and
                // was deep-cloned on every state read.
                let url = match web::canvas_to_blob(&canvas, "image/png").await {
                    Ok(b) => Url::create_object_url_with_blob(&b).unwrap_or_default(),
                    Err(_) => String::new(),
                };
                let thumb_url = thumb_object_url(&pp, pw, ph).await;
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData { full: (full_rgba, fw, fh), preview: (pp, pw, ph), gen: 0 }),
                    )
                });
                push_item(state, MediaItem {
                    id,
                    kind: MediaKind::Photo,
                    name,
                    object_url: url,
                    thumb_url,
                    width: fw,
                    height: fh,
                    edit: EditParams::default(),
                    layers: Vec::new(),
                    next_layer_id: 0,
                    exif,
                    drive_file_id: None,
                    drive_parent_id: None,
                });
            } else {
                state.busy.set(Some(format!("Loading {name}…")));
                let mut blob: Blob = file.clone().into();
                let lname = name.to_lowercase();
                let is_jpeg = !is_heic(&file)
                    && (lname.ends_with(".jpg") || lname.ends_with(".jpeg"));
                let is_png = lname.ends_with(".png");
                let exif = if is_jpeg || is_png {
                    web::read_file_array_buffer(&file)
                        .await
                        .ok()
                        .map(|b| js_sys::Uint8Array::new(&b).to_vec())
                        .and_then(|b| {
                            if is_png {
                                exif::tiff_from_png(&b)
                            } else {
                                exif::tiff_from_jpeg(&b)
                            }
                        })
                        .map(Rc::new)
                } else {
                    None
                };
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
                let thumb_url = thumb_object_url(&preview.0, preview.1, preview.2).await;
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(id, Rc::new(PhotoData { full, preview, gen: 0 }))
                });
                push_item(state, MediaItem {
                    id,
                    kind: MediaKind::Photo,
                    name,
                    object_url: url,
                    thumb_url,
                    width: fw,
                    height: fh,
                    edit: EditParams::default(),
                    layers: Vec::new(),
                    next_layer_id: 0,
                    exif,
                    drive_file_id: None,
                    drive_parent_id: None,
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

/// Import a photo/RAW already downloaded from Drive as raw bytes.
fn ingest_drive_bytes(
    state: AppState,
    drive_file_id: String,
    drive_parent_id: String,
    name: String,
    bytes: Vec<u8>,
) {
    spawn_local(async move {
        let id = state.next_id.get_untracked();
        state.next_id.set(id + 1);
        state.busy.set(Some(format!("Importing {name}…")));

        if raw::is_raw_name(&name) {
            let Ok((full_rgba, fw, fh)) = raw::decode_raw(&bytes) else {
                web::log(&format!("RAW decode failed for {name}"));
                state.busy.set(None);
                return;
            };
            let exif = raw::extract_exif_tiff(&bytes).map(Rc::new);
            let (pp, pw, ph) = downscale_pixels(&full_rgba, fw, fh, 1024);
            let canvas = web::create_canvas(pw as u32, ph as u32);
            web::put_pixels(&canvas, &pp, pw as u32, ph as u32);
            let url = match web::canvas_to_blob(&canvas, "image/png").await {
                Ok(b) => Url::create_object_url_with_blob(&b).unwrap_or_default(),
                Err(_) => String::new(),
            };
            let thumb_url = thumb_object_url(&pp, pw, ph).await;
            CACHE.with(|c| {
                c.borrow_mut().photos.insert(
                    id,
                    Rc::new(PhotoData { full: (full_rgba, fw, fh), preview: (pp, pw, ph), gen: 0 }),
                )
            });
            push_item(state, MediaItem {
                id,
                kind: MediaKind::Photo,
                name,
                object_url: url,
                thumb_url,
                width: fw,
                height: fh,
                edit: EditParams::default(),
                layers: Vec::new(),
                next_layer_id: 0,
                exif,
                drive_file_id: Some(drive_file_id),
                drive_parent_id: Some(drive_parent_id),
            });
        } else {
            let lname = name.to_lowercase();
            let mime = if lname.ends_with(".png") { "image/png" } else { "image/jpeg" };
            let exif = if lname.ends_with(".png") {
                exif::tiff_from_png(&bytes)
            } else if lname.ends_with(".jpg") || lname.ends_with(".jpeg") {
                exif::tiff_from_jpeg(&bytes)
            } else {
                None
            }
            .map(Rc::new);
            let Ok(blob) = web::bytes_to_blob(&bytes, mime) else {
                state.busy.set(None);
                return;
            };
            let Ok((img, url)) = web::load_image(&blob).await else {
                state.busy.set(None);
                return;
            };
            let full = web::rgba_from_image(&img);
            let (fw, fh) = (full.1, full.2);
            let preview = downscale(&img, 1024);
            let thumb_url = thumb_object_url(&preview.0, preview.1, preview.2).await;
            CACHE.with(|c| {
                c.borrow_mut().photos.insert(id, Rc::new(PhotoData { full, preview, gen: 0 }))
            });
            push_item(state, MediaItem {
                id,
                kind: MediaKind::Photo,
                name,
                object_url: url,
                thumb_url,
                width: fw,
                height: fh,
                edit: EditParams::default(),
                layers: Vec::new(),
                next_layer_id: 0,
                exif,
                drive_file_id: Some(drive_file_id),
                drive_parent_id: Some(drive_parent_id),
            });
        }
        state.busy.set(None);
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

    ctx.save();
    if t.angle != 0.0 {
        let _ = ctx.translate(x, y);
        let _ = ctx.rotate((t.angle as f64).to_radians());
        let _ = ctx.translate(-x, -y);
    }

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

    ctx.restore();
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
    let size_px = (b.width * h as f32) as f64;
    if b.shape == state::BrushShape::Smooth {
        ctx.set_stroke_style_str(&b.color);
        ctx.set_line_width(size_px);
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
        return;
    }

    ctx.set_fill_style_str(&b.color);
    let spacing_px = (b.spacing.clamp(0.1, 4.0) * b.width * h as f32).max(0.5);
    let custom = if b.shape == state::BrushShape::Custom {
        web_sys::Path2d::new_with_path_string(&b.custom_path).ok()
    } else {
        None
    };
    let half = size_px / 2.0;
    for stroke in &b.strokes {
        let pts: Vec<(f32, f32)> = stroke
            .points
            .iter()
            .map(|&(x, y)| (x * w as f32, y * h as f32))
            .collect();
        for dab in stroke::dab_positions(&pts, spacing_px) {
            ctx.save();
            ctx.translate(dab.x as f64, dab.y as f64).ok();
            ctx.rotate(dab.angle as f64).ok();
            match b.shape {
                state::BrushShape::Square => {
                    ctx.fill_rect(-half, -half, size_px, size_px);
                }
                state::BrushShape::Dot => {
                    ctx.begin_path();
                    ctx.arc(0.0, 0.0, half, 0.0, std::f64::consts::TAU).ok();
                    ctx.fill();
                }
                state::BrushShape::Triangle => {
                    ctx.begin_path();
                    ctx.move_to(half, 0.0);
                    ctx.line_to(half * -0.5, half * 0.866);
                    ctx.line_to(half * -0.5, half * -0.866);
                    ctx.close_path();
                    ctx.fill();
                }
                state::BrushShape::Custom => {
                    if let Some(path) = &custom {
                        ctx.scale(size_px / 100.0, size_px / 100.0).ok();
                        ctx.translate(-50.0, -50.0).ok();
                        ctx.fill_with_path_2d(path);
                    }
                }
                state::BrushShape::Smooth => unreachable!(),
            }
            ctx.restore();
        }
    }
}

fn draw_raster_layer(ctx: &web_sys::CanvasRenderingContext2d, layer: &state::Layer, w: f64, h: f64) {
    let LayerKind::Raster(r) = &layer.kind else { return };
    if r.width == 0 || r.height == 0 {
        return;
    }
    let canvas = web::create_canvas(r.width as u32, r.height as u32);
    web::put_pixels(&canvas, &r.pixels, r.width as u32, r.height as u32);
    let dw = (r.scale * w as f32) as f64;
    let dh = dw * r.height as f64 / r.width as f64;
    let dx = (r.x as f64 * w) - dw / 2.0;
    let dy = (r.y as f64 * h) - dh / 2.0;
    let _ = ctx.draw_image_with_html_canvas_element_and_dw_and_dh(&canvas, dx, dy, dw, dh);
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

// --- hi-res display render ----------------------------------------------------
//
// The editing preview is a 1024px downscale; once the on-screen canvas is
// bigger than that (large windows, retina, tight crops) it visibly softens.
// When the preview underserves the display we render the whole working image
// from the full-res original at display resolution into this cache, and blit
// from it. Rebuilds are debounced so slider drags stay on the cheap path.

/// Ceiling for the hi-res long edge — past this, CPU edit ops cost more than
/// the sharpness is worth.
const HI_MAX_EDGE: usize = 3200;

struct HiCache {
    item_id: usize,
    fingerprint: u64,
    canvas: web_sys::HtmlCanvasElement,
    w: usize,
    h: usize,
}

thread_local! {
    static HI_CACHE: RefCell<Option<HiCache>> = const { RefCell::new(None) };
    /// Generation guard for the debounced hi rebuild; only the latest fires.
    static HI_DEBOUNCE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Last nonzero canvas display need (CSS px × DPR). Reactive remounts
    /// briefly hand us a not-yet-inserted canvas with zero client size.
    static LAST_NEED: std::cell::Cell<(f64, f64)> = const { std::cell::Cell::new((0.0, 0.0)) };
}

/// Content identity for the hi cache: everything that changes the rendered
/// pixels EXCEPT the crop rect (crop only reframes the blit).
fn content_fingerprint(item: &MediaItem, photo_gen: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u64(photo_gen);
    let e = &item.edit;
    h.write_u8(e.rot90);
    for v in [e.fine_angle, e.brightness, e.contrast, e.saturation, e.warmth, e.blur, e.grain] {
        h.write_u32(v.to_bits());
    }
    h.write_u8(e.grain_gaussian as u8);
    h.write_u8(e.grain_mono as u8);
    match e.aspect.ratio() {
        Some((a, b)) => {
            h.write_u32(a.to_bits());
            h.write_u32(b.to_bits());
        }
        None => h.write_u8(0),
    }
    format!("{:?}", e.levels).hash(&mut h);
    format!("{:?}", e.curves).hash(&mut h);
    match &e.selection {
        None => h.write_u8(0),
        Some(sel) => {
            h.write_u32(sel.feather.to_bits());
            match &sel.kind {
                SelectionKind::Rect { x, y, w, h: rh } => {
                    h.write_u8(1);
                    for v in [*x, *y, *w, *rh] {
                        h.write_u32(v.to_bits());
                    }
                }
                SelectionKind::Lasso(pts) => {
                    h.write_u8(2);
                    pts.len().hash(&mut h);
                    for p in pts.first().into_iter().chain(pts.last()) {
                        h.write_u32(p.0.to_bits());
                        h.write_u32(p.1.to_bits());
                    }
                }
                SelectionKind::Mask { data, width, height } => {
                    h.write_u8(3);
                    width.hash(&mut h);
                    height.hash(&mut h);
                    if !data.is_empty() {
                        data.len().hash(&mut h);
                        for i in 0..8 {
                            h.write_u8(data[i * data.len() / 8]);
                        }
                    }
                }
            }
        }
    }
    item.layers.len().hash(&mut h);
    for l in &item.layers {
        l.id.hash(&mut h);
        l.visible.hash(&mut h);
        h.write_u32(l.opacity.to_bits());
        match &l.kind {
            LayerKind::Text(t) => {
                h.write_u8(1);
                t.text.hash(&mut h);
                t.font_family.hash(&mut h);
                t.color.hash(&mut h);
                t.stroke_color.hash(&mut h);
                t.shadow_color.hash(&mut h);
                t.font_weight.hash(&mut h);
                std::mem::discriminant(&t.alignment).hash(&mut h);
                for v in [t.x, t.y, t.font_size, t.angle, t.stroke_width, t.shadow_blur, t.shadow_offset_x, t.shadow_offset_y] {
                    h.write_u32(v.to_bits());
                }
            }
            LayerKind::Path(p) => {
                h.write_u8(2);
                p.points.len().hash(&mut h);
                p.closed.hash(&mut h);
                p.fill_color.hash(&mut h);
                p.stroke_color.hash(&mut h);
                h.write_u32(p.stroke_width.to_bits());
                for pt in p.points.first().into_iter().chain(p.points.last()) {
                    h.write_u32(pt.x.to_bits());
                    h.write_u32(pt.y.to_bits());
                }
            }
            LayerKind::Brush(b) => {
                h.write_u8(3);
                b.strokes.len().hash(&mut h);
                b.color.hash(&mut h);
                b.custom_path.hash(&mut h);
                std::mem::discriminant(&b.shape).hash(&mut h);
                h.write_u32(b.width.to_bits());
                h.write_u32(b.spacing.to_bits());
                if let Some(s) = b.strokes.last() {
                    s.points.len().hash(&mut h);
                    if let Some(p) = s.points.last() {
                        h.write_u32(p.0.to_bits());
                        h.write_u32(p.1.to_bits());
                    }
                }
            }
            LayerKind::Raster(r) => {
                h.write_u8(4);
                r.width.hash(&mut h);
                r.height.hash(&mut h);
                h.write_u32(r.x.to_bits());
                h.write_u32(r.y.to_bits());
                h.write_u32(r.scale.to_bits());
                if !r.pixels.is_empty() {
                    r.pixels.len().hash(&mut h);
                    for i in 0..8 {
                        h.write_u8(r.pixels[i * r.pixels.len() / 8]);
                    }
                }
            }
        }
    }
    h.finish()
}

/// Resize a pixel buffer to exact dims via a canvas round-trip.
fn resample_pixels(pixels: &[u8], w: usize, h: usize, tw: usize, th: usize) -> Vec<u8> {
    let src = web::create_canvas(w as u32, h as u32);
    web::put_pixels(&src, pixels, w as u32, h as u32);
    let dst = web::create_canvas(tw as u32, th as u32);
    web::ctx2d(&dst)
        .draw_image_with_html_canvas_element_and_dw_and_dh(&src, 0.0, 0.0, tw as f64, th as f64)
        .unwrap();
    web::image_data_from_ctx(&web::ctx2d(&dst), tw as u32, th as u32).0
}

/// Render the photo from its full-res original at (roughly) display
/// resolution. Mirrors the preview pipeline and export.
/// Render size for the hi cache: enough that the requested total-image
/// region covers the display, with headroom — capped by HI_MAX_EDGE and by
/// the full-resolution working dims (rendering past the source buys nothing).
fn hi_target(req_w: f64, req_h: f64, full_w: usize, full_h: usize) -> (usize, usize) {
    let edge = (req_w.max(req_h) * 1.25).round();
    let cap = (HI_MAX_EDGE as f64).min(full_w.max(full_h) as f64);
    let t = edge.min(cap).max(1.0);
    let scale = t / full_w.max(full_h) as f64;
    if scale < 0.999 {
        (
            (full_w as f64 * scale).round().max(1.0) as usize,
            (full_h as f64 * scale).round().max(1.0) as usize,
        )
    } else {
        (full_w, full_h)
    }
}

fn render_hi(item: &MediaItem, req_w: f64, req_h: f64) -> Option<HiCache> {
    let photo = get_photo(item.id)?;
    let fingerprint = content_fingerprint(item, photo.gen);
    let (fpix, fw, fh) = &photo.full;
    let (p, ww, wh) = geometry(fpix, *fw, *fh, &item.edit);
    let (tw, th) = hi_target(req_w, req_h, ww, wh);
    let mut buf = if (tw, th) != (ww, wh) {
        resample_pixels(&p, ww, wh, tw, th)
    } else {
        p
    };
    if item.edit.is_color_touched() {
        if let Some(sel) = &item.edit.selection {
            let mask = ops::selection_mask(sel, tw, th);
            blur::blur_apply_masked(&mut buf, &mask, tw, th, item.edit.blur);
            if !item.edit.levels.is_identity() {
                levels::levels_apply_masked(&mut buf, &mask, &item.edit.levels.build_tables());
            }
            item.edit.curves.apply_masked(&mut buf, &mask);
            ops::adjust_masked(
                &mut buf,
                &mask,
                item.edit.brightness,
                item.edit.contrast,
                item.edit.saturation,
                item.edit.warmth,
            );
            noise::noise_add_masked(&mut buf, &mask, tw, th, item.edit.grain, item.edit.grain_gaussian, item.edit.grain_mono, item.id as u32);
        } else {
            blur::blur_apply(&mut buf, tw, th, item.edit.blur);
            if !item.edit.levels.is_identity() {
                levels::levels_apply(&mut buf, &item.edit.levels.build_tables());
            }
            item.edit.curves.apply(&mut buf);
            ops::adjust(
                &mut buf,
                item.edit.brightness,
                item.edit.contrast,
                item.edit.saturation,
                item.edit.warmth,
            );
            noise::noise_add(&mut buf, tw, th, item.edit.grain, item.edit.grain_gaussian, item.edit.grain_mono, item.id as u32);
        }
    }
    let canvas = web::create_canvas(tw as u32, th as u32);
    web::put_pixels(&canvas, &buf, tw as u32, th as u32);
    composite_layers(&canvas, &item.layers, tw, th);
    Some(HiCache { item_id: item.id, fingerprint, canvas, w: tw, h: th })
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

/// iPhone-style crop zoom: scale the crop rect about a fixed screen point
/// (pinch centroid / cursor), then translate by the centroid's movement.
/// Zooming in shrinks the crop rect — the photo appears to enlarge inside
/// the frame. `ratio` locks the rect's on-image aspect when set.
fn zoom_crop(
    orig: state::CropRect,
    zoom_from: (f32, f32),
    pan: (f32, f32),
    scale: f32,
    ratio: Option<f32>,
    img_aspect: f32,
) -> state::CropRect {
    let scale = scale.clamp(0.1, 16.0);
    let (w, h) = match ratio {
        Some(r) => {
            let w = (orig.w / scale).clamp(0.02, 1.0f32.min(r / img_aspect));
            (w, w * img_aspect / r)
        }
        None => (
            (orig.w / scale).clamp(0.02, 1.0),
            (orig.h / scale).clamp(0.02, 1.0),
        ),
    };
    let x = (zoom_from.0 - (zoom_from.0 - orig.x) * (w / orig.w) + pan.0).clamp(0.0, 1.0 - w);
    let y = (zoom_from.1 - (zoom_from.1 - orig.y) * (h / orig.h) + pan.1).clamp(0.0, 1.0 - h);
    state::CropRect { x, y, w, h }
}

/// Render a photo through the full edit pipeline into an encoded blob.
/// Returns (blob, mime, filename).
async fn render_photo_export(
    state: AppState,
    item: &MediaItem,
) -> Option<(Blob, &'static str, String)> {
    let photo = get_photo(item.id)?;
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
    let format = state.photo_format.get_untracked();
    let encoded = match format {
        PhotoFormat::Jpeg => web::canvas_to_jpeg_blob(&out, 0.92).await,
        PhotoFormat::JpegMax => web::canvas_to_jpeg_blob(&out, 1.0).await,
        PhotoFormat::Png => web::canvas_to_blob(&out, "image/png").await,
    };
    let blob = encoded.ok()?;
    let (mime, ext) = match format {
        PhotoFormat::Png => ("image/png", "png"),
        _ => ("image/jpeg", "jpg"),
    };
    // Carry source EXIF into the export when we have it.
    let blob = match &item.exif {
        Some(tiff) => match web::blob_to_bytes(&blob).await {
            Ok(bytes) => {
                let bytes = match format {
                    PhotoFormat::Png => exif::inject_png(&bytes, tiff),
                    _ => exif::inject_jpeg(&bytes, tiff),
                };
                web::bytes_to_blob(&bytes, mime).unwrap_or(blob)
            }
            Err(_) => blob,
        },
        None => blob,
    };
    let base = item.name.rsplitn(2, '.').last().unwrap_or("photo");
    let filename = format!("edited-{}-{}.{}", item.edit.aspect.label(), base, ext);
    Some((blob, mime, filename))
}

fn export_photo(state: AppState, item: MediaItem) {
    spawn_local(async move {
        state.busy.set(Some(format!("Exporting {}…", item.name)));
        if let Some((blob, _mime, filename)) = render_photo_export(state, &item).await {
            web::download_blob(&blob, &filename);
        }
        state.busy.set(None);
    });
}

/// Upload the rendered export into the granted Drive folder; overwrites in
/// place when the item came from Drive or was already saved there.
fn save_photo_to_drive(state: AppState, item: MediaItem) {
    spawn_local(async move {
        let Some(token) = state.drive_token.get_untracked() else {
            return;
        };
        let Some((folder_id, _)) = state.drive_folder.get_untracked() else {
            return;
        };
        state.busy.set(Some(format!("Saving {} to Drive…", item.name)));
        state.drive_error.set(None);

        // Unedited JPEG/PNG: upload the source bytes untouched. If the item
        // came from Drive it is already byte-identical there — nothing to do.
        if photo_unedited(&item) {
            if let Some(mime) = passthru_mime(&item.name) {
                if item.drive_file_id.is_some() {
                    state.busy.set(None);
                    state
                        .drive_error
                        .set(Some(format!("{} is unchanged — already in Drive", item.name)));
                    return;
                }
                match web::object_url_bytes(&item.object_url).await {
                    Ok(bytes) => {
                        let r = drive::upload_file(&token, &folder_id, None, &item.name, mime, &bytes).await;
                        state.busy.set(None);
                        match r {
                            Ok(fid) => {
                                state.update_current_item(|m| m.drive_file_id = Some(fid));
                                drive_refresh(state);
                            }
                            Err(e) => state
                                .drive_error
                                .set(Some(format!("Drive save failed: {}", js_err(&e)))),
                        }
                        return;
                    }
                    // Couldn't read the source back — fall through to re-encode.
                    Err(_) => {}
                }
            }
        }

        let Some((blob, mime, filename)) = render_photo_export(state, &item).await else {
            state.busy.set(None);
            return;
        };
        let result = async {
            let bytes = web::blob_to_bytes(&blob).await?;
            let first = drive::upload_file(
                &token,
                &folder_id,
                item.drive_file_id.as_deref(),
                &filename,
                mime,
                &bytes,
            )
            .await;
            match first {
                Ok(fid) => Ok(fid),
                // Overwrite-in-place needs write access; drive.file grants that
                // only for app-created/picked files, so a readonly-imported
                // original rejects PATCH. Fall back to a new copy beside it.
                Err(e) => match item.drive_parent_id.as_deref() {
                    Some(parent) => {
                        drive::upload_file(&token, parent, None, &filename, mime, &bytes).await
                    }
                    None => Err(e),
                },
            }
        }
        .await;
        match result {
            Ok(fid) => {
                state.update_current_item(|m| {
                    m.drive_file_id = Some(fid);
                    m.drive_parent_id = None;
                });
                drive_refresh(state);
            }
            Err(e) => {
                state
                    .drive_error
                    .set(Some(format!("Drive save failed: {}", js_err(&e))));
            }
        }
        state.busy.set(None);
    });
}

fn js_err(e: &JsValue) -> String {
    e.as_string()
        .unwrap_or_else(|| format!("{e:?}").chars().take(200).collect())
}

/// True when nothing about the photo differs from the source file — saving
/// can pass the original bytes through instead of re-encoding (a JPEG would
/// otherwise lose a generation on every save).
fn photo_unedited(item: &MediaItem) -> bool {
    let e = &item.edit;
    e.aspect == Aspect::Original
        && e.crop == state::CropRect::default()
        && e.rot90 == 0
        && e.fine_angle == 0.0
        && !e.is_color_touched()
        && e.selection.is_none()
        && item.layers.iter().all(|l| !l.visible)
}

/// Mime for byte pass-through, or None when the object URL doesn't hold the
/// original file bytes (RAW/HEIC are decoded to a working format on import).
fn passthru_mime(name: &str) -> Option<&'static str> {
    let n = name.to_lowercase();
    if n.ends_with(".png") {
        Some("image/png")
    } else if n.ends_with(".jpg") || n.ends_with(".jpeg") {
        Some("image/jpeg")
    } else {
        None
    }
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

thread_local! {
    static FONTS_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

// The bundled fonts only matter once a text layer exists, so don't fetch them
// at startup.
fn ensure_fonts() {
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
fn App() -> impl IntoView {
    let state = AppState::new();
    provide_context(state);

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
                            <img src=m.thumb_url.clone()/>
                            <span class="badge">{badge}</span>
                        </div>
                    }
                }
            />
        </div>
    }
}

/// Folder currently being browsed: deepest breadcrumb, or the granted root.
fn drive_current_folder(state: AppState) -> Option<(String, String)> {
    if let Some(last) = state.drive_path.get_untracked().last() {
        return Some(last.clone());
    }
    state.drive_folder.get_untracked()
}

fn drive_refresh(state: AppState) {
    spawn_local(async move {
        let Some(token) = state.drive_token.get_untracked() else {
            return;
        };
        let Some((root_id, _)) = state.drive_folder.get_untracked() else {
            return;
        };
        let Some((fid, _)) = drive_current_folder(state) else {
            return;
        };
        state.drive_error.set(None);
        let order = state.drive_sort.get_untracked().order_by();
        match drive::list_folder(&token, &fid, order).await {
            Ok((files, subs)) => {
                state.drive_files.set(files);
                state.drive_subfolders.set(subs);
            }
            Err(e) => state
                .drive_error
                .set(Some(format!("Drive list failed: {}", js_err(&e)))),
        }
        // The shortlist manifest lives at the granted root regardless of
        // which subfolder is being browsed.
        match drive::find_manifest(&token, &root_id).await {
            Ok(Some(mid)) => {
                state.drive_manifest_id.set(Some(mid.clone()));
                if let Ok(bytes) = drive::download_file(&token, &mid).await {
                    state.drive_shortlist.set(drive::parse_shortlist(&bytes));
                }
            }
            Ok(None) => {
                state.drive_manifest_id.set(None);
                state.drive_shortlist.set(Default::default());
            }
            Err(_) => {}
        }
    });
}

/// Files as shown in the grid: current folder, star filter applied.
/// Sorting happens server-side via orderBy on the listing.
fn visible_drive_files(state: AppState) -> Vec<drive::DriveFile> {
    let filter = state.drive_filter.get();
    let shortlist = state.drive_shortlist.get();
    state.drive_files.with(|v| {
        v.iter()
            .filter(|f| match filter {
                DriveFilter::All => true,
                DriveFilter::Starred => shortlist.contains(&f.id),
                DriveFilter::Unstarred => !shortlist.contains(&f.id),
            })
            .cloned()
            .collect()
    })
}

fn toggle_star(state: AppState, id: String) {
    let mut unstarred = false;
    state.drive_shortlist.update(|s| {
        if s.remove(&id) {
            unstarred = true;
        } else {
            s.insert(id.clone());
        }
    });
    if unstarred {
        // Unstarring a file that's imported (including one under edit) drops
        // it from the filmstrip and frees its pixels.
        let doomed: Vec<usize> = state.items.with(|v| {
            v.iter()
                .filter(|m| m.drive_file_id.as_deref() == Some(id.as_str()))
                .map(|m| m.id)
                .collect()
        });
        for item_id in doomed {
            remove_item(state, item_id);
        }
    }
    // Debounced manifest write: only the latest generation actually saves.
    state.drive_save_gen.update(|g| *g += 1);
    let gen = state.drive_save_gen.get_untracked();
    let cb = wasm_bindgen::closure::Closure::once(move || {
        spawn_local(async move {
            if state.drive_save_gen.get_untracked() != gen {
                return;
            }
            let Some(token) = state.drive_token.get_untracked() else {
                return;
            };
            let Some((root_id, _)) = state.drive_folder.get_untracked() else {
                return;
            };
            let ids = state.drive_shortlist.get_untracked();
            let body = drive::shortlist_json(&ids);
            let existing = state.drive_manifest_id.get_untracked();
            match drive::upload_file(
                &token,
                &root_id,
                existing.as_deref(),
                drive::MANIFEST_NAME,
                "application/json",
                &body,
            )
            .await
            {
                Ok(fid) => state.drive_manifest_id.set(Some(fid)),
                Err(e) => state
                    .drive_error
                    .set(Some(format!("Couldn't save shortlist: {}", js_err(&e)))),
            }
        });
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        900,
    );
    cb.forget();
}

fn drive_navigate(state: AppState, depth: usize) {
    // depth 0 = root; otherwise truncate breadcrumbs to that many levels.
    state.drive_path.update(|p| p.truncate(depth));
    state.drive_selected.set(Default::default());
    state.drive_loupe.set(None);
    drive_hover_clear(state);
    drive_refresh(state);
}

fn drive_enter(state: AppState, folder: drive::SubFolder) {
    state
        .drive_path
        .update(|p| p.push((folder.id, folder.name)));
    state.drive_selected.set(Default::default());
    state.drive_loupe.set(None);
    drive_hover_clear(state);
    drive_refresh(state);
}

fn download_import(state: AppState, f: drive::DriveFile) {
    spawn_local(async move {
        let Some(token) = state.drive_token.get_untracked() else {
            return;
        };
        state.busy.set(Some(format!("Downloading {} from Drive…", f.name)));
        state.drive_error.set(None);
        match drive::download_file(&token, &f.id).await {
            Ok(bytes) => {
                state.busy.set(None);
                ingest_drive_bytes(state, f.id, f.parent, f.name, bytes);
            }
            Err(e) => {
                state.busy.set(None);
                state
                    .drive_error
                    .set(Some(format!("Drive download failed: {}", js_err(&e))));
            }
        }
    });
}

/// Hover-zoom: open the floating preview after a short rest on a tile, so
/// sweeping the cursor across the grid doesn't strobe previews.
fn drive_hover_soon(state: AppState, id: String, x: f64, y: f64) {
    state.drive_hover_gen.update(|g| *g += 1);
    let gen = state.drive_hover_gen.get_untracked();
    let cb = wasm_bindgen::closure::Closure::once(move || {
        if state.drive_hover_gen.get_untracked() == gen {
            state.drive_hover.set(Some((id, x, y)));
        }
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        280,
    );
    cb.forget();
}

fn drive_hover_move(state: AppState, id: &str, x: f64, y: f64) {
    let showing = state.drive_hover.with(|h| {
        h.as_ref().map(|(hid, _, _)| hid == id).unwrap_or(false)
    });
    if showing {
        state.drive_hover.set(Some((id.to_string(), x, y)));
    }
}

fn drive_hover_clear(state: AppState) {
    state.drive_hover_gen.update(|g| *g += 1);
    state.drive_hover.set(None);
}

/// Fixed-position style for the hover preview, kept inside the viewport.
fn drive_hover_style(x: f64, y: f64) -> String {
    let w = 400.0_f64;
    let h = 340.0_f64;
    let vw = web::window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1280.0);
    let vh = web::window().inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
    let left = if x + 24.0 + w > vw { (x - w - 14.0).max(8.0) } else { x + 24.0 };
    let top = (y - h / 2.0).max(8.0).min((vh - h - 8.0).max(8.0));
    format!("left:{left:.0}px;top:{top:.0}px")
}

/// Grid tile width to request from the Drive thumbnail endpoint: CSS tile
/// size × dpr, bucketed to 100px so URL churn stays low when the slider moves.
fn drive_grid_thumb_w(state: AppState) -> u32 {
    let px = state.drive_thumb_px.get() as f64 * web::window().device_pixel_ratio();
    ((px / 100.0).ceil() as u32 * 100).clamp(100, 400)
}

#[component]
fn DriveLoupe(state: AppState) -> impl IntoView {
    let close = move |_| state.drive_loupe.set(None);
    // Remember viewed files so their large thumbnails stay warm, and keep
    // hidden <img> elements mounted for them plus the current neighbors —
    // the thumbnail endpoint is cross-origin, so this is the only caching
    // available to us. Cap the pool so a long session doesn't hoard memory.
    create_effect(move |_| {
        if let Some(idx) = state.drive_loupe.get() {
            let files = visible_drive_files(state);
            if let Some(f) = files.get(idx.min(files.len().saturating_sub(1))) {
                let id = f.id.clone();
                state.drive_loupe_seen.update(|v| {
                    if let Some(pos) = v.iter().position(|x| *x == id) {
                        v.remove(pos);
                    }
                    v.push(id);
                    while v.len() > 12 {
                        v.remove(0);
                    }
                });
            }
        }
    });
    // Loupe paints the w800 thumb first (likely already fetched by hover) and
    // swaps to w1600 once that finishes downloading in the background.
    let hi_ready = create_rw_signal(false);
    create_effect(move |_| {
        let _ = state.drive_loupe.get();
        hi_ready.set(false);
    });
    let step = move |d: i64| {
        let n = visible_drive_files(state).len();
        if n == 0 {
            return;
        }
        let cur = state.drive_loupe.get_untracked().unwrap_or(0) as i64;
        state
            .drive_loupe
            .set(Some(((cur + d).rem_euclid(n as i64)) as usize));
    };
    let kd = window_event_listener(leptos::ev::keydown, move |ev| {
        if state.drive_loupe.get_untracked().is_none() {
            return;
        }
        match ev.key().as_str() {
            "ArrowLeft" => step(-1),
            "ArrowRight" => step(1),
            "Escape" => state.drive_loupe.set(None),
            _ => {}
        }
    });
    on_cleanup(move || kd.remove());

    view! {
        <Show when=move || state.drive_loupe.get().is_some() fallback=|| ()>
            {move || {
                let files = visible_drive_files(state);
                let Some(idx) = state.drive_loupe.get() else {
                    return view! { <div></div> }.into_view();
                };
                let Some(f) = files.get(idx.min(files.len().saturating_sub(1))).cloned() else {
                    return view! { <div></div> }.into_view();
                };
                let starred = state.drive_shortlist.with(|s| s.contains(&f.id));
                let fid = f.id.clone();
                let f_import = f.clone();
                let date = f.modified.get(..10).unwrap_or("").to_string();
                // Warm pool: seen ids + immediate neighbors, minus current.
                let n = files.len();
                let mut warm: Vec<String> = state.drive_loupe_seen.get_untracked();
                for d in [-1i64, 1] {
                    let nb = ((idx as i64 + d).rem_euclid(n as i64)) as usize;
                    if let Some(g) = files.get(nb) {
                        if !warm.contains(&g.id) {
                            warm.push(g.id.clone());
                        }
                    }
                }
                warm.retain(|id| id != &f.id);
                view! {
                    <div class="loupe" on:click=close>
                        <div class="loupe-body" on:click=move |ev| ev.stop_propagation()>
                            <img
                                src={
                                    let id = f.id.clone();
                                    move || {
                                        if hi_ready.get() {
                                            drive::thumb_url_sz(&id, 1600)
                                        } else {
                                            drive::thumb_url_sz(&id, 800)
                                        }
                                    }
                                }
                                class="loupe-img"
                                decoding="async"
                            />
                            <img
                                src=drive::thumb_url_sz(&f.id, 1600)
                                style="display:none"
                                aria-hidden="true"
                                decoding="async"
                                on:load=move |_| hi_ready.set(true)
                            />
                            <div class="loupe-preload" aria-hidden="true">
                                {warm
                                    .into_iter()
                                    .map(|id| {
                                        view! { <img src=drive::thumb_url_sz(&id, 1600) loading="eager" decoding="async"/> }
                                    })
                                    .collect_view()}
                            </div>
                            <div class="loupe-bar">
                                <button class="btn" on:click=move |_| step(-1)>"‹ Prev"</button>
                                <button class="btn" on:click=move |_| step(1)>"Next ›"</button>
                                <div class="loupe-info">
                                    <span>{f.name.clone()}</span>
                                    <span class="dim">
                                        {format!("{} · {} KB · {}/{}", date, f.size_kb, idx + 1, files.len())}
                                    </span>
                                </div>
                                <button
                                    class="btn loupe-star"
                                    class:on=starred
                                    on:click=move |_| toggle_star(state, fid.clone())
                                >
                                    {if starred { "★ Starred" } else { "☆ Star" }}
                                </button>
                                <button
                                    class="btn primary"
                                    disabled=!f_import.importable()
                                    on:click=move |_| {
                                        download_import(state, f_import.clone());
                                        state.drive_loupe.set(None);
                                    }
                                >
                                    "Import"
                                </button>
                                <button class="btn" on:click=close>"✕"</button>
                            </div>
                        </div>
                    </div>
                }
                .into_view()
            }}
        </Show>
    }
}

#[component]
fn DrivePanel(state: AppState) -> impl IntoView {
    let do_sign_in = move |_| {
        spawn_local(async move {
            state.busy.set(Some("Signing in to Google…".into()));
            state.drive_error.set(None);
            match drive::sign_in().await {
                Ok(token) => {
                    state.drive_token.set(Some(Rc::new(token)));
                    drive_refresh(state);
                }
                Err(e) => state.drive_error.set(Some(js_err(&e))),
            }
            state.busy.set(None);
        });
    };
    let do_pick_folder = move |_| {
        spawn_local(async move {
            let Some(token) = state.drive_token.get_untracked() else {
                return;
            };
            state.drive_error.set(None);
            match drive::pick_folder(&token).await {
                Ok(Some((id, name))) => {
                    drive::save_folder(&id, &name);
                    state.drive_folder.set(Some((id, name)));
                    state.drive_path.set(Vec::new());
                    drive_refresh(state);
                }
                Ok(None) => {}
                Err(e) => state
                    .drive_error
                    .set(Some(format!("Folder pick failed: {}", js_err(&e)))),
            }
        });
    };
    let do_sign_out = move |_| {
        state.drive_token.set(None);
        state.drive_files.set(Vec::new());
        state.drive_subfolders.set(Vec::new());
        state.drive_path.set(Vec::new());
        state.drive_selected.set(Default::default());
        state.drive_loupe.set(None);
        state.drive_error.set(None);
    };
    let import_selected = move |_| {
        let ids = state.drive_selected.get_untracked();
        let files: Vec<drive::DriveFile> = state
            .drive_files
            .get_untracked()
            .into_iter()
            .filter(|f| ids.contains(&f.id) && f.importable())
            .collect();
        state.drive_selected.set(Default::default());
        state.drive_open.set(false);
        for f in files {
            download_import(state, f);
        }
    };
    // Import every starred file, including ones outside the current folder
    // (resolved by id) and ones already in the filmstrip (imported again as
    // duplicates, so an in-progress edit is never clobbered or skipped).
    let import_starred = move |_| {
        let ids = state.drive_shortlist.get_untracked();
        if ids.is_empty() {
            return;
        }
        state.drive_open.set(false);
        let known = state.drive_files.get_untracked();
        let mut missing: Vec<String> = Vec::new();
        for f in known.iter().filter(|f| ids.contains(&f.id)) {
            if f.importable() {
                download_import(state, f.clone());
            }
        }
        for id in ids.iter().filter(|id| !known.iter().any(|f| &f.id == *id)) {
            missing.push(id.clone());
        }
        if missing.is_empty() {
            return;
        }
        spawn_local(async move {
            let Some(token) = state.drive_token.get_untracked() else {
                return;
            };
            for id in missing {
                match drive::get_file(&token, &id).await {
                    Ok(f) => {
                        if f.importable() {
                            download_import(state, f);
                        }
                    }
                    Err(e) => state
                        .drive_error
                        .set(Some(format!("Starred file lookup failed: {}", js_err(&e)))),
                }
            }
        });
    };

    view! {
        <div class="panel drive-panel">
            <button
                class="drive-head"
                on:click=move |_| state.drive_open.update(|o| *o = !*o)
            >
                <span>{move || if state.drive_open.get() { "▾" } else { "▸" }} " Google Drive"</span>
                <span class="dim">{move || {
                    state.drive_folder.with(|f| {
                        f.as_ref().map(|(_, n)| n.clone()).unwrap_or_default()
                    })
                }}</span>
            </button>
            <Show when=move || state.drive_open.get() fallback=|| ()>
                {move || {
                    if !drive::configured() {
                        view! {
                            <p class="dim">"Drive isn’t configured in this build yet."</p>
                        }
                        .into_view()
                    } else if state.drive_token.with(|t| t.is_none()) {
                        view! {
                            <button class="btn primary" on:click=do_sign_in>
                                "Sign in with Google"
                            </button>
                        }
                        .into_view()
                    } else {
                        view! {
                            <div class="chips">
                                <button class="chip" on:click=do_pick_folder>
                                    {move || {
                                        if state.drive_folder.with(|f| f.is_some()) {
                                            "Change folder"
                                        } else {
                                            "Choose folder"
                                        }
                                    }}
                                </button>
                                <Show
                                    when=move || state.drive_folder.with(|f| f.is_some())
                                    fallback=|| ()
                                >
                                    <button class="chip" on:click=move |_| drive_refresh(state)>
                                        "↻ Refresh"
                                    </button>
                                </Show>
                                <button class="chip" on:click=do_sign_out>"Sign out"</button>
                            </div>
                            // Breadcrumbs: granted root + descended subfolders.
                            <div class="drive-crumbs">
                                <button class="drive-crumb" on:click=move |_| drive_navigate(state, 0)>
                                    {move || {
                                        state.drive_folder.with(|f| {
                                            f.as_ref().map(|(_, n)| n.clone()).unwrap_or_default()
                                        })
                                    }}
                                </button>
                                {move || {
                                    state
                                        .drive_path
                                        .get()
                                        .into_iter()
                                        .enumerate()
                                        .map(|(i, (_, name))| {
                                            view! {
                                                <span class="dim">"›"</span>
                                                <button
                                                    class="drive-crumb"
                                                    on:click=move |_| drive_navigate(state, i + 1)
                                                >
                                                    {name}
                                                </button>
                                            }
                                        })
                                        .collect_view()
                                }}
                            </div>
                            // Toolbar: star filter, sort, tile size, batch import.
                            <div class="drive-toolbar">
                                <div class="chips">
                                    {[
                                        (DriveFilter::All, "All"),
                                        (DriveFilter::Starred, "★ Starred"),
                                        (DriveFilter::Unstarred, "☆ Unstarred"),
                                    ]
                                    .map(|(f, label)| {
                                        view! {
                                            <button
                                                class="chip"
                                                class:active=move || state.drive_filter.get() == f
                                                on:click=move |_| state.drive_filter.set(f)
                                            >
                                                {label}
                                            </button>
                                        }
                                    })}
                                </div>
                                <select
                                    class="drive-sort"
                                    on:change=move |ev| {
                                        let v = event_target_value(&ev);
                                        state.drive_sort.set(if v == "name" {
                                            DriveSort::NameAsc
                                        } else {
                                            DriveSort::DateDesc
                                        });
                                        drive_refresh(state);
                                    }
                                >
                                    <option value="date" selected=move || state.drive_sort.get() == DriveSort::DateDesc>"Newest"</option>
                                    <option value="name" selected=move || state.drive_sort.get() == DriveSort::NameAsc>"Name"</option>
                                </select>
                                <input
                                    type="range"
                                    min="56"
                                    max="160"
                                    step="8"
                                    prop:value=move || state.drive_thumb_px.get()
                                    on:input=move |ev| {
                                        if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                            state.drive_thumb_px.set(v);
                                        }
                                    }
                                    class="drive-size"
                                    title="Thumbnail size"
                                />
                                {move || {
                                    let n = state.drive_selected.with(|s| s.len());
                                    (n > 0).then(|| {
                                        view! {
                                            <button class="btn primary" on:click=import_selected>
                                                {format!("Import {n} selected")}
                                            </button>
                                        }
                                    })
                                }}
                                {move || {
                                    let n = state.drive_shortlist.with(|s| s.len());
                                    (n > 0).then(|| {
                                        view! {
                                            <button class="btn" on:click=import_starred>
                                                {format!("Import {n} starred")}
                                            </button>
                                        }
                                    })
                                }}
                            </div>
                            <div
                                class="drive-files"
                                style:grid-template-columns=move || {
                                    format!(
                                        "repeat(auto-fill, minmax({}px, 1fr))",
                                        state.drive_thumb_px.get()
                                    )
                                }
                            >
                                <For
                                    each=move || state.drive_subfolders.get()
                                    key=|s: &drive::SubFolder| s.id.clone()
                                    children=move |s: drive::SubFolder| {
                                        let sname = s.name.clone();
                                        view! {
                                            <div
                                                class="drive-file drive-folder"
                                                on:click=move |_| drive_enter(state, s.clone())
                                            >
                                                <span class="drive-icon">"📁"</span>
                                                <span class="drive-name">{sname}</span>
                                            </div>
                                        }
                                    }
                                />
                                <For
                                    each=move || {
                                        visible_drive_files(state)
                                            .into_iter()
                                            .enumerate()
                                            .collect::<Vec<_>>()
                                    }
                                    key=|(_, f): &(usize, drive::DriveFile)| f.id.clone()
                                    children=move |(idx, f): (usize, drive::DriveFile)| {
                                        let importable = f.importable();
                                        let thumb_broken = create_rw_signal(false);
                                        let fid_thumb = f.id.clone();
                                        let fid = f.id.clone();
                                        let fid_class = f.id.clone();
                                        let fid2 = f.id.clone();
                                        let fid2_class = f.id.clone();
                                        let fid2_text = f.id.clone();
                                        let fid3 = f.id.clone();
                                        let fid3_move = f.id.clone();
                                        view! {
                                            <div
                                                class="drive-file"
                                                class:disabled=!importable
                                                title=f.name.clone()
                                                on:click=move |_| {
                                                    drive_hover_clear(state);
                                                    if importable {
                                                        state.drive_loupe.set(Some(idx));
                                                    }
                                                }
                                                on:mouseenter=move |ev| {
                                                    if importable {
                                                        drive_hover_soon(
                                                            state,
                                                            fid3.clone(),
                                                            ev.client_x() as f64,
                                                            ev.client_y() as f64,
                                                        );
                                                    }
                                                }
                                                on:mousemove=move |ev| {
                                                    drive_hover_move(
                                                        state,
                                                        &fid3_move,
                                                        ev.client_x() as f64,
                                                        ev.client_y() as f64,
                                                    );
                                                }
                                                on:mouseleave=move |_| drive_hover_clear(state)
                                            >
                                                {move || {
                                                    if thumb_broken.get() {
                                                        view! { <span class="drive-icon">"🖼"</span> }
                                                            .into_view()
                                                    } else {
                                                        view! {
                                                            <img
                                                                src={
                                                                    let id = fid_thumb.clone();
                                                                    move || {
                                                                        drive::thumb_url_sz(
                                                                            &id,
                                                                            drive_grid_thumb_w(state),
                                                                        )
                                                                    }
                                                                }
                                                                loading="lazy"
                                                                decoding="async"
                                                                on:error=move |_| thumb_broken.set(true)
                                                            />
                                                        }
                                                        .into_view()
                                                    }
                                                }}
                                                <button
                                                    class="drive-select"
                                                    class:on=move || state.drive_selected.with(|s| s.contains(&fid_class))
                                                    on:click=move |ev| {
                                                        ev.stop_propagation();
                                                        state.drive_selected.update(|s| {
                                                            if !s.remove(&fid) {
                                                                s.insert(fid.clone());
                                                            }
                                                        });
                                                    }
                                                >
                                                    "✓"
                                                </button>
                                                <button
                                                    class="drive-star"
                                                    class:on=move || state.drive_shortlist.with(|s| s.contains(&fid2_class))
                                                    on:click=move |ev| {
                                                        ev.stop_propagation();
                                                        toggle_star(state, fid2.clone());
                                                    }
                                                >
                                                    {move || {
                                                        if state.drive_shortlist.with(|s| s.contains(&fid2_text)) {
                                                            "★"
                                                        } else {
                                                            "☆"
                                                        }
                                                    }}
                                                </button>
                                                {f.edited.then(|| {
                                                    view! { <span class="drive-badge">"Edited"</span> }
                                                })}
                                                <span class="drive-name">{f.name.clone()}</span>
                                            </div>
                                        }
                                    }
                                />
                            </div>
                            <DriveLoupe state=state/>
                            {move || {
                                state.drive_hover.get().map(|(id, x, y)| {
                                    view! {
                                        <div class="drive-hover" style=drive_hover_style(x, y)>
                                            <img src=drive::thumb_url_sz(&id, 800) decoding="async"/>
                                        </div>
                                    }
                                })
                            }}
                        }
                        .into_view()
                    }
                }}
            </Show>
            {move || {
                state.drive_error.with(|e| {
                    e.clone().map(|msg| {
                        view! { <p class="dim drive-error">{msg}</p> }
                    })
                })
            }}
        </div>
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {    Crop,
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
                <div class="controls">
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

fn crop_is_full(c: &state::CropRect) -> bool {
    c.x <= 0.0005 && c.y <= 0.0005 && c.w >= 0.999 && c.h >= 0.999
}

// Pointer position in full-image normalized coords. The preview canvas shows
// only the crop region outside the Crop tab, so canvas-local coords map
// through the crop rect (identity when uncropped).
fn layer_norm_pos(ev: &web_sys::MouseEvent, crop: state::CropRect) -> Option<(f32, f32)> {
    let el = web::document().query_selector(".canvas-wrap").ok().flatten()?;
    let rect = el.get_bounding_client_rect();
    let nx = ((ev.client_x() as f32 - rect.left() as f32) / rect.width() as f32).clamp(0.0, 1.0);
    let ny = ((ev.client_y() as f32 - rect.top() as f32) / rect.height() as f32).clamp(0.0, 1.0);
    Some((crop.x + nx * crop.w, crop.y + ny * crop.h))
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

fn text_overlay_style(t: &state::TextLayer, opacity: f32, crop: &state::CropRect) -> String {
    let px = format!("{:.2}%", t.font_size * 100.0);
    // Overlay coords are % of the (possibly cropped) preview; layer coords are
    // full-image normalized, so map through the crop rect.
    let left = format!("{:.2}%", (t.x - crop.x) / crop.w * 100.0);
    let top = format!("{:.2}%", (t.y - crop.y) / crop.h * 100.0);
    let align = t.alignment.canvas_value();
    let translate = match t.alignment {
        TextAlign::Left => "translate(0,-50%)",
        TextAlign::Center => "translate(-50%,-50%)",
        TextAlign::Right => "translate(-100%,-50%)",
    };
    let transform = format!("{} rotate({}deg)", translate, t.angle);
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
        left, top, transform, t.font_family, t.font_weight, px, t.color, align, shadow, opacity, stroke
    )
}

fn selected_text_overlay_style(t: &state::TextLayer, opacity: f32, crop: &state::CropRect) -> String {
    let px = format!("{:.2}%", t.font_size * 100.0);
    let left = format!("{:.2}%", (t.x - crop.x) / crop.w * 100.0);
    let top = format!("{:.2}%", (t.y - crop.y) / crop.h * 100.0);
    let align = t.alignment.canvas_value();
    let translate = match t.alignment {
        TextAlign::Left => "translate(0,-50%)",
        TextAlign::Center => "translate(-50%,-50%)",
        TextAlign::Right => "translate(-100%,-50%)",
    };
    let transform = format!("{} rotate({}deg)", translate, t.angle);
    format!(
        "position:absolute;left:{};top:{};transform:{};font-family:'{}',sans-serif;\
         font-weight:{};font-size:{};color:transparent;text-align:{};opacity:{};white-space:nowrap;\
         border:2px dashed #0a84ff;border-radius:4px;background:rgba(10,132,255,0.08);",
        left, top, transform, t.font_family, t.font_weight, px, align, opacity
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

    // --- clone stamp stroke ----------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some(mut pts)) = clone_drag.try_get() else { return };
        let Some(item) = state.current() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return };
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
        let Some(Some(mut pts)) = heal_drag.try_get() else { return };
        let Some(item) = state.current() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return };
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

    // --- text/image layer drag ---------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some((id, nx0, ny0, x0, y0))) = layer_drag.try_get() else { return };
        let Some(item) = state.current() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return };
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

    // --- pen tool drag ---------------------------------------------------------
    window_event_listener(leptos::ev::pointermove, move |ev| {
        let Some(Some((id, drag, nx0, ny0))) = pen_drag.try_get() else { return };
        let Some(item) = state.current() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return };
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
        let Some(item) = state.current() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return };
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
        let Some(Some(drag)) = select_drag.try_get() else { return };
        let Some(item) = state.current() else { return };
        let Some((nx, ny)) = layer_norm_pos(&ev, item.edit.crop) else { return };
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
        state.hi_built.track();
        state.viewport_gen.track();
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
            LAST_NEED.with(|n| n.get())
        } else {
            LAST_NEED.with(|n| n.set((need_w, need_h)));
            (need_w, need_h)
        };
        let want_hi = need_w > w as f64 * 1.05 || need_h > h as f64 * 1.05;

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
        // Exact size render_hi would produce for this request — the cache is
        // sufficient iff it matches, which keeps rebuilds from looping.
        let (fw, fh) = working_dims(photo.full.1, photo.full.2, &item.edit);
        let target = hi_target(req_w, req_h, fw, fh);

        let mut hi_dims: Option<(usize, usize)> = None;
        if want_hi {
            let fp = content_fingerprint(&item, photo.gen);
            hi_dims = HI_CACHE.with(|c| {
                let b = c.borrow();
                let hi = b.as_ref()?;
                if hi.item_id != item.id || hi.fingerprint != fp {
                    return None;
                }
                if hi.w < target.0 || hi.h < target.1 {
                    return None;
                }
                if cropped_view {
                    let (cx2, cy2, cw2, ch2) = crop_px(&item.edit, hi.w, hi.h);
                    canvas.set_width(cw2 as u32);
                    canvas.set_height(ch2 as u32);
                    web::ctx2d(&canvas)
                        .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                            &hi.canvas, cx2 as f64, cy2 as f64, cw2 as f64, ch2 as f64,
                            0.0, 0.0, cw2 as f64, ch2 as f64,
                        )
                        .unwrap();
                } else {
                    canvas.set_width(hi.w as u32);
                    canvas.set_height(hi.h as u32);
                    web::ctx2d(&canvas)
                        .draw_image_with_html_canvas_element(&hi.canvas, 0.0, 0.0)
                        .unwrap();
                }
                Some((hi.w, hi.h))
            });
        }

        if hi_dims.is_none() {
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
            if want_hi {
                // Sharp frame lands 150ms after edits settle; until then the
                // cheap preview render keeps drags responsive.
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
                    if let Some(hi) = render_hi(&item, req_w, req_h) {
                        HI_CACHE.with(|c| *c.borrow_mut() = Some(hi));
                        state.hi_built.update(|g| *g += 1);
                    }
                });
                let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    150,
                );
                cb.forget();
            }
        }

        // Overlay draws below use full-image preview pixel coords; map them
        // onto the canvas backing (hi-res scale and/or the crop offset).
        let (cx_p, cy_p) = if cropped_view {
            let (cx, cy, _, _) = crop_px(&item.edit, w, h);
            (cx as f64, cy as f64)
        } else {
            (0.0, 0.0)
        };
        let ctx = web::ctx2d(&canvas);
        let overlay_open = match (hi_dims, cropped_view) {
            (Some((hw, _)), true) => {
                ctx.save();
                let s = hw as f64 / w as f64;
                let _ = ctx.scale(s, s);
                let _ = ctx.translate(-cx_p, -cy_p);
                true
            }
            (Some((hw, _)), false) => {
                ctx.save();
                let _ = ctx.scale(hw as f64 / w as f64, hw as f64 / w as f64);
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
                    >
                        <canvas node_ref=canvas_ref></canvas>
                        <Show when=move || tab.get() == Tab::Crop fallback=|| ()>
                            <CropOverlay state=state working=working drag=crop_drag pinch=crop_pinch/>
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
                // Video preview is not cropped; identity mapping.
                let style = text_overlay_style(t, l.opacity, &state::CropRect::default());
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
fn CropOverlay(
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
                    snap.clone().zip(pts.first().copied().zip(pts.get(1).copied()))
                else {
                    return;
                };
                let mid = ((a.1 + b.1) / 2.0, (a.2 + b.2) / 2.0);
                let spread = ((a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt();
                if d0 <= f32::EPSILON {
                    return;
                }
                let Some(item) = state.current() else { return };
                let Some((ww, wh)) = working.try_get() else { return };
                let c = zoom_crop(
                    orig,
                    mid0,
                    (mid.0 - mid0.0, mid.1 - mid0.1),
                    spread / d0,
                    item.edit.aspect.ratio().map(|(rw, rh)| rw / rh),
                    ww as f32 / wh as f32,
                );
                state.update_current(|e| e.crop = c);
            });
            return;
        }
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

#[component]
fn CropTab(state: AppState, tab: RwSignal<Tab>) -> impl IntoView {
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

/// 256px JPEG thumbnail blob URL from a pixel buffer, for filmstrip tiles.
async fn thumb_object_url(pixels: &[u8], w: usize, h: usize) -> String {
    let (tp, tw, th) = downscale_pixels(pixels, w, h, 256);
    let canvas = web::create_canvas(tw as u32, th as u32);
    web::put_pixels(&canvas, &tp, tw as u32, th as u32);
    match web::canvas_to_jpeg_blob(&canvas, 0.85).await {
        Ok(b) => Url::create_object_url_with_blob(&b).unwrap_or_default(),
        Err(_) => String::new(),
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
                pd.gen += 1;
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
            pd.gen += 1;
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
            pd.gen += 1;
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
            pd.gen += 1;
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
            pd.gen += 1;
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
                kind: state::SelectionKind::Mask { data: Rc::new(mask), width: w, height: h },
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
        if kind == "text" {
            ensure_fonts();
        }
        let mut new_id = None;
        state.update_current_item(|m| {
            let id = m.next_layer_id;
            m.next_layer_id += 1;
            let layer = match kind {
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
            <RasterLayerEditor state=state/>
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
fn RasterLayerEditor(state: AppState) -> impl IntoView {
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
    let is_photo = move || state.current().map(|m| m.kind == MediaKind::Photo).unwrap_or(false);
    let keep_audio = move || state.current().map(|m| m.edit.keep_audio).unwrap_or(true);
    let format = move || state.photo_format.get();
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
