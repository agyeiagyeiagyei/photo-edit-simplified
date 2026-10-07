//! Pixel worker: owns per-item full-resolution buffers off the main thread.
//! The app ships pixels in via `load`, then issues render/destructive ops;
//! results come back as JS objects with transferable ArrayBuffers.

use std::cell::RefCell;
use std::collections::HashMap;

use js_sys::{Array, Object, Reflect, Uint8Array, Uint8ClampedArray};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use pes_pixel::geo::{geometry, hi_target, stamp_stroke};
use pes_pixel::recipe::{edit_params_from_recipe, selection_from_json};
use pes_pixel::types::EditParams;
use pes_pixel::{blur, clone, fill, heal, levels, noise, ops};

thread_local! {
    static BUFFERS: RefCell<HashMap<u32, (Vec<u8>, usize, usize)>> = RefCell::new(HashMap::new());
}

fn with_buffer<R>(id: u32, f: impl FnOnce(&mut (Vec<u8>, usize, usize)) -> R) -> Result<R, JsValue> {
    BUFFERS.with(|b| {
        let mut b = b.borrow_mut();
        let buf = b.get_mut(&id).ok_or_else(|| JsValue::from_str("not loaded"))?;
        Ok(f(buf))
    })
}

fn js_buf(v: Vec<u8>) -> JsValue {
    let arr = Uint8Array::new_with_length(v.len() as u32);
    arr.copy_from(&v);
    arr.buffer().into()
}

fn result_obj(pairs: &[(&str, JsValue)]) -> JsValue {
    let o = Object::new();
    for (k, v) in pairs {
        Reflect::set(&o, &JsValue::from_str(k), v).unwrap();
    }
    o.into()
}

/// Store (or replace) an item's full-res RGBA buffer.
#[wasm_bindgen]
pub fn load(id: u32, pixels: Uint8Array, w: u32, h: u32) {
    BUFFERS.with(|b| {
        b.borrow_mut()
            .insert(id, (pixels.to_vec(), w as usize, h as usize));
    });
}

#[wasm_bindgen]
pub fn unload(id: u32) {
    BUFFERS.with(|b| {
        b.borrow_mut().remove(&id);
    });
}

fn parse_edit(recipe: &str) -> EditParams {
    edit_params_from_recipe(recipe).unwrap_or_default()
}

/// Parametric pipeline (geometry → resample → color ops), mirroring the app's
/// render_hi. Selection comes as JSON + optional mask buffer.
fn render_pipeline(
    full: &[u8],
    fw: usize,
    fh: usize,
    edit: &EditParams,
    seed: u32,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> (Vec<u8>, usize, usize) {
    let (p, ww, wh) = geometry(full, fw, fh, edit);
    let (tw, th) = hi_target(req_w, req_h, ww, wh, max_edge);
    let mut buf = if (tw, th) != (ww, wh) {
        resample(&p, ww, wh, tw, th)
    } else {
        p
    };
    if !edit.is_color_touched() {
        return (buf, tw, th);
    }
    let selection = sel_json.and_then(|j| selection_from_json(&j, sel_mask.map(|m| m.to_vec())));
    if let Some(sel) = selection {
        let mask = ops::selection_mask(&sel, tw, th);
        blur::blur_apply_masked(&mut buf, &mask, tw, th, edit.blur);
        if !edit.levels.is_identity() {
            levels::levels_apply_masked(&mut buf, &mask, &edit.levels.build_tables());
        }
        edit.curves.apply_masked(&mut buf, &mask);
        ops::adjust_masked(
            &mut buf,
            &mask,
            edit.brightness,
            edit.contrast,
            edit.saturation,
            edit.warmth,
        );
        noise::noise_add_masked(
            &mut buf,
            &mask,
            tw,
            th,
            edit.grain,
            edit.grain_gaussian,
            edit.grain_mono,
            seed,
        );
    } else {
        blur::blur_apply(&mut buf, tw, th, edit.blur);
        if !edit.levels.is_identity() {
            levels::levels_apply(&mut buf, &edit.levels.build_tables());
        }
        edit.curves.apply(&mut buf);
        ops::adjust(
            &mut buf,
            edit.brightness,
            edit.contrast,
            edit.saturation,
            edit.warmth,
        );
        noise::noise_add(
            &mut buf,
            tw,
            th,
            edit.grain,
            edit.grain_gaussian,
            edit.grain_mono,
            seed,
        );
    }
    (buf, tw, th)
}

/// Render the hi-res frame. Returns {pixels, w, h}.
#[wasm_bindgen]
pub fn render(
    id: u32,
    recipe: &str,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> Result<JsValue, JsValue> {
    with_buffer(id, |(full, fw, fh)| {
        let edit = parse_edit(recipe);
        let (buf, tw, th) =
            render_pipeline(full, *fw, *fh, &edit, id, req_w, req_h, max_edge, sel_json, sel_mask);
        result_obj(&[
            ("pixels", js_buf(buf)),
            ("w", (tw as u32).into()),
            ("h", (th as u32).into()),
        ])
    })
}

/// Resample via OffscreenCanvas 2D (same bilinear path as the app's canvas).
fn resample(pixels: &[u8], w: usize, h: usize, tw: usize, th: usize) -> Vec<u8> {
    let fallback = || nearest_resample(pixels, w, h, tw, th);
    let Ok(src) = OffscreenCanvas::new(w as u32, h as u32) else {
        return fallback();
    };
    put_pixels(&src, pixels, w as u32, h as u32);
    let Ok(dst) = OffscreenCanvas::new(tw as u32, th as u32) else {
        return fallback();
    };
    let Some(ctx) = ctx2d(&dst) else {
        return fallback();
    };
    if ctx
        .draw_image_with_offscreen_canvas_and_dw_and_dh(&src, 0.0, 0.0, tw as f64, th as f64)
        .is_err()
    {
        return fallback();
    }
    match ctx.get_image_data(0.0, 0.0, tw as f64, th as f64) {
        Ok(data) => data.data().to_vec(),
        Err(_) => fallback(),
    }
}

fn ctx2d(c: &OffscreenCanvas) -> Option<OffscreenCanvasRenderingContext2d> {
    c.get_context("2d").ok()??.dyn_into().ok()
}

fn put_pixels(c: &OffscreenCanvas, pixels: &[u8], w: u32, h: u32) {
    let Some(ctx) = ctx2d(c) else { return };
    let arr = Uint8ClampedArray::from(pixels);
    let Ok(data) = web_sys::ImageData::new_with_js_u8_clamped_array_and_sh(&arr, w, h) else {
        return;
    };
    let _ = ctx.put_image_data(&data, 0.0, 0.0);
}

/// Pure-Rust fallback resampler (nearest) if OffscreenCanvas is unavailable.
fn nearest_resample(pixels: &[u8], w: usize, h: usize, tw: usize, th: usize) -> Vec<u8> {
    let mut out = vec![0u8; tw * th * 4];
    for y in 0..th {
        let sy = y * h / th;
        for x in 0..tw {
            let sx = x * w / tw;
            out[(y * tw + x) * 4..(y * tw + x) * 4 + 4]
                .copy_from_slice(&pixels[(sy * w + sx) * 4..(sy * w + sx) * 4 + 4]);
        }
    }
    out
}

fn downscale(pixels: &[u8], w: usize, h: usize, long_edge: u32) -> (Vec<u8>, usize, usize) {
    let le = w.max(h) as u32;
    if le <= long_edge {
        return (pixels.to_vec(), w, h);
    }
    let scale = long_edge as f64 / le as f64;
    let tw = ((w as f64 * scale).round() as usize).max(1);
    let th = ((h as f64 * scale).round() as usize).max(1);
    (resample(pixels, w, h, tw, th), tw, th)
}

/// Destructive ops make their result the item's new stored buffer (the app
/// resets crop/rotation to identity on apply, so base dims become full dims).
fn commit(full: &mut Vec<u8>, fw: &mut usize, fh: &mut usize, base: &[u8], w: usize, h: usize) {
    full.clear();
    full.extend_from_slice(base);
    *fw = w;
    *fh = h;
}

/// Destructive-op result: new full buffer + 1024px preview.
fn destructive_result(base: Vec<u8>, w: usize, h: usize) -> JsValue {    let (pv, pw, ph) = downscale(&base, w, h, 1024);
    result_obj(&[
        ("pixels", js_buf(base)),
        ("w", (w as u32).into()),
        ("h", (h as u32).into()),
        ("preview", js_buf(pv)),
        ("pw", (pw as u32).into()),
        ("ph", (ph as u32).into()),
    ])
}

fn heal_mode_wire(m: u8) -> heal::HealMode {
    match m {
        1 => heal::HealMode::SmoothFill,
        2 => heal::HealMode::ProximityMatch,
        _ => heal::HealMode::ContentAware,
    }
}

/// Spot heal along a stroke. Returns the destructive-op result, or null when
/// the stroke sampled nothing (caller keeps the old buffer).
#[wasm_bindgen]
pub fn heal_stroke(
    id: u32,
    recipe: &str,
    pts: &JsValue,
    radius_frac: f32,
    mode: u8,
) -> Result<JsValue, JsValue> {
    with_buffer(id, |(full, fw, fh)| {
        let edit = parse_edit(recipe);
        let (mut base, w, h) = geometry(full, *fw, *fh, &edit);
        let diag = ((w * w + h * h) as f32).sqrt();
        let r = (radius_frac * diag).max(1.0);
        let pts = parse_pts(pts);
        let mut coverage = vec![0u8; w * h];
        stamp_stroke(&mut coverage, w, h, &pts, r);
        if !heal::spot_heal(&mut base, &coverage, w, h, 1.0, heal_mode_wire(mode), id) {
            return JsValue::NULL;
        }
        commit(full, fw, fh, &base, w, h);
        destructive_result(base, w, h)
    })
}

/// Clone stamp along a stroke. (dx, dy) is the stroke offset in pixels
/// (caller derives it from the source point and working dims).
#[wasm_bindgen]
pub fn clone_stamp(
    id: u32,
    recipe: &str,
    pts: &JsValue,
    radius_frac: f32,
    dx: i32,
    dy: i32,
) -> Result<JsValue, JsValue> {
    with_buffer(id, |(full, fw, fh)| {
        let edit = parse_edit(recipe);
        let (mut base, w, h) = geometry(full, *fw, *fh, &edit);
        let diag = ((w * w + h * h) as f32).sqrt();
        let r = (radius_frac * diag).max(1.0);
        let pts = parse_pts(pts);
        let mut coverage = vec![0u8; w * h];
        stamp_stroke(&mut coverage, w, h, &pts, r);
        clone::clone_stamp(&mut base, w, h, &coverage, dx, dy);
        commit(full, fw, fh, &base, w, h);
        destructive_result(base, w, h)
    })
}

fn delete_by_mask(base: &mut [u8], mask: &[u8]) {
    for (px, m) in base.chunks_exact_mut(4).zip(mask.iter()) {
        let a = *m as f32 / 255.0;
        px[3] = (px[3] as f32 * (1.0 - a)).min(255.0) as u8;
    }
}

/// Delete selection (alpha *= 1-mask/255) over the working image.
#[wasm_bindgen]
pub fn cut(
    id: u32,
    recipe: &str,
    sel_json: &str,
    sel_mask: Option<Uint8Array>,
) -> Result<JsValue, JsValue> {
    with_buffer(id, |(full, fw, fh)| {
        let edit = parse_edit(recipe);
        let (mut base, w, h) = geometry(full, *fw, *fh, &edit);
        let Some(sel) = selection_from_json(sel_json, sel_mask.map(|m| m.to_vec())) else {
            return JsValue::NULL;
        };
        let mask = ops::selection_mask(&sel, w, h);
        delete_by_mask(&mut base, &mask);
        commit(full, fw, fh, &base, w, h);
        destructive_result(base, w, h)
    })
}

/// Content-aware fill over the selection. Null when the fill declined.
#[wasm_bindgen]
pub fn fill_selection(
    id: u32,
    recipe: &str,
    sel_json: &str,
    sel_mask: Option<Uint8Array>,
) -> Result<JsValue, JsValue> {
    with_buffer(id, |(full, fw, fh)| {
        let edit = parse_edit(recipe);
        let (mut base, w, h) = geometry(full, *fw, *fh, &edit);
        let Some(sel) = selection_from_json(sel_json, sel_mask.map(|m| m.to_vec())) else {
            return JsValue::NULL;
        };
        let mask = ops::selection_mask(&sel, w, h);
        if !fill::content_fill(&mut base, &mask, w, h) {
            return JsValue::NULL;
        }
        commit(full, fw, fh, &base, w, h);
        destructive_result(base, w, h)
    })
}

/// Extract the selection as a full-size RGBA layer (alpha-masked pixels at
/// working dims). `also_cut` additionally punches the region out of the item
/// buffer; the result then carries the destructive-op fields too.
#[wasm_bindgen]
pub fn extract(
    id: u32,
    recipe: &str,
    sel_json: &str,
    sel_mask: Option<Uint8Array>,
    also_cut: bool,
) -> Result<JsValue, JsValue> {
    with_buffer(id, |(full, fw, fh)| {
        let edit = parse_edit(recipe);
        let (mut base, w, h) = geometry(full, *fw, *fh, &edit);
        let Some(sel) = selection_from_json(sel_json, sel_mask.map(|m| m.to_vec())) else {
            return JsValue::NULL;
        };
        let mask = ops::selection_mask(&sel, w, h);
        let layer = ops::extract_masked(&base, &mask);
        let mut pairs: Vec<(&str, JsValue)> = vec![
            ("layer", js_buf(layer)),
            ("lw", (w as u32).into()),
            ("lh", (h as u32).into()),
        ];
        if also_cut {
            delete_by_mask(&mut base, &mask);
            commit(full, fw, fh, &base, w, h);
            let (pv, pw, ph) = downscale(&base, w, h, 1024);
            pairs.push(("pixels", js_buf(base)));
            pairs.push(("w", (w as u32).into()));
            pairs.push(("h", (h as u32).into()));
            pairs.push(("preview", js_buf(pv)));
            pairs.push(("pw", (pw as u32).into()));
            pairs.push(("ph", (ph as u32).into()));
        }
        result_obj(&pairs)
    })
}

fn parse_pts(v: &JsValue) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    if !Array::is_array(v) {
        return out;
    }
    let arr: Array = v.clone().unchecked_into();
    for p in arr.iter() {
        let pair: Array = p.unchecked_into();
        if pair.length() == 2 {
            out.push((
                pair.get(0).as_f64().unwrap_or_default() as f32,
                pair.get(1).as_f64().unwrap_or_default() as f32,
            ));
        }
    }
    out
}

use web_sys::{OffscreenCanvas, OffscreenCanvasRenderingContext2d};
