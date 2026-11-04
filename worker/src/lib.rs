//! Pixel worker: owns per-item full-resolution buffers off the main thread.
//! The app ships pixels in via `load`, then issues render/destructive ops;
//! results come back as JS objects with transferable ArrayBuffers.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use js_sys::{Array, Object, Reflect, Uint16Array, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use pes_pixel::geo::{crop_px, geometry, geometry_wide, hi_target, stamp_stroke, working_dims};
use pes_pixel::pipeline::{self, Frame, Target};
use pes_pixel::rawops;
use pes_pixel::recipe::{edit_params_from_recipe, selection_from_json};
use pes_pixel::types::EditParams;
use pes_pixel::{clone, color, fill, heal, ops, resample, wide};

mod srgb_icc;

thread_local! {
    static BUFFERS: RefCell<HashMap<u32, (Vec<u8>, usize, usize)>> = RefCell::new(HashMap::new());
    /// Un-toned 16-bit develop for RAW items, at the stored frame's dims. The
    /// neutral pipeline renders from this and applies the camera tone curve
    /// per tick, so a highlight edit acts on unclipped data. Dropped whenever
    /// the item's stored pixels are replaced (load, destructive commit,
    /// unload).
    static NEUTRAL: RefCell<HashMap<u32, Neutral>> = RefCell::new(HashMap::new());
    /// `get_decoder_ms` of the first RAW opened in this worker. The first call
    /// pays rawler's camera-database build; later calls are the steady state.
    static DECODER_FIRST_MS: Cell<Option<f64>> = const { Cell::new(None) };
    /// Full sensor frame from the last proxy develop. The display upgrade and
    /// the export otherwise demosaic the same file twice; the export takes
    /// this instead. One slot: the app exports what it just selected. Empty
    /// when the proxy was a half-size superpixel develop.
    static RAW_FULL: RefCell<Option<RawFull>> = RefCell::new(None);
    /// Tone curve from the last proxy develop. A half-size proxy cannot serve
    /// the export, but the full develop reuses this curve so both match.
    static RAW_TONE: RefCell<Option<(u32, [u8; 256])>> = RefCell::new(None);
    /// Post-geometry, post-resample pixels for the last render. Colour-only
    /// edits (a slider drag) restore this instead of rotating and downscaling
    /// the full frame again. Cleared whenever the stored full buffer changes
    /// (load, unload, destructive commit), so an entry can never outlive the
    /// pixels it was derived from.
    static BASE_CACHE: RefCell<Option<BaseCache>> = const { RefCell::new(None) };
    /// Post-crop, post-resample pixels for the last crop-window render. The
    /// cropped view shows only this region, so a colour-only edit copies the
    /// window rather than the whole frame. Cleared with the base cache.
    static WINDOW_CACHE: RefCell<Option<WindowCache>> = const { RefCell::new(None) };
    /// 16-bit counterparts of the two caches above, for the neutral path.
    static WIDE_BASE_CACHE: RefCell<Option<WideBaseCache>> = const { RefCell::new(None) };
    static WIDE_WINDOW_CACHE: RefCell<Option<WideWindowCache>> = const { RefCell::new(None) };
}

struct Neutral {
    pixels: Vec<u16>,
    w: usize,
    h: usize,
    white: f32,
    /// Camera-tone curve; None when the develop had no usable preview curve,
    /// in which case the neutral frame is already the output. Kept as the
    /// curve itself so each render can repair its clipped tail to the
    /// current highlights value.
    tone: Option<[u8; 256]>,
}

struct RawFull {
    id: u32,
    rgba: Vec<u8>,
    neutral: Vec<u16>,
    white: f32,
    tone: Option<[u8; 256]>,
    w: usize,
    h: usize,
}

struct BaseCache {
    id: u32,
    rot90: u8,
    fine_bits: u32,
    tw: usize,
    th: usize,
    /// Pre-develop, pre-colour: `rawops` runs on a copy every render so
    /// exposure changes never compound into the cached pixels.
    pixels: Vec<u8>,
}

struct WindowCache {
    id: u32,
    rot90: u8,
    fine_bits: u32,
    /// Crop rect in working pixels.
    crop: (usize, usize, usize, usize),
    tw: usize,
    th: usize,
    pixels: Vec<u8>,
}

/// 16-bit counterparts of the two caches above, for the neutral pipeline.
struct WideBaseCache {
    id: u32,
    rot90: u8,
    fine_bits: u32,
    tw: usize,
    th: usize,
    pixels: Vec<u16>,
}

struct WideWindowCache {
    id: u32,
    rot90: u8,
    fine_bits: u32,
    crop: (usize, usize, usize, usize),
    tw: usize,
    th: usize,
    pixels: Vec<u16>,
}

fn clear_render_caches() {
    BASE_CACHE.with(|c| *c.borrow_mut() = None);
    WINDOW_CACHE.with(|c| *c.borrow_mut() = None);
    WIDE_BASE_CACHE.with(|c| *c.borrow_mut() = None);
    WIDE_WINDOW_CACHE.with(|c| *c.borrow_mut() = None);
}

fn drop_neutral(id: u32) {
    NEUTRAL.with(|n| {
        n.borrow_mut().remove(&id);
    });
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

fn js_u16_buf(v: Vec<u16>) -> JsValue {
    let arr = Uint16Array::new_with_length(v.len() as u32);
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

/// Linear memory currently committed by this wasm instance. RAW develops are
/// the memory peak, so replies carry it and the app tracks the high-water.
fn wasm_mem_bytes() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        std::arch::wasm32::memory_size(0) as f64 * 65536.0
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}

/// Store (or replace) an item's full-res RGBA buffer.
#[wasm_bindgen]
pub fn load(id: u32, pixels: Uint8Array, w: u32, h: u32) {
    clear_render_caches();
    drop_neutral(id);
    BUFFERS.with(|b| {
        b.borrow_mut()
            .insert(id, (pixels.to_vec(), w as usize, h as usize));
    });
}

/// Store (or replace) an item's un-toned 16-bit develop. Its dims must match
/// the stored RGBA buffer's: it replaces that buffer's develop stage, not its
/// geometry. `tone` is the camera-tone LUT (256 entries), or empty when the
/// develop had no usable preview curve.
#[wasm_bindgen]
pub fn load_neutral(id: u32, pixels: Uint16Array, w: u32, h: u32, white: f32, tone: Uint8Array) {
    let tone = if tone.length() == 256 {
        let mut lut = [0u8; 256];
        tone.copy_to(&mut lut);
        Some(lut)
    } else {
        None
    };
    WIDE_BASE_CACHE.with(|c| *c.borrow_mut() = None);
    WIDE_WINDOW_CACHE.with(|c| *c.borrow_mut() = None);
    NEUTRAL.with(|n| {
        n.borrow_mut().insert(
            id,
            Neutral {
                pixels: pixels.to_vec(),
                w: w as usize,
                h: h as usize,
                white,
                tone,
            },
        );
    });
}

/// Decode an encoded photo (JPEG/PNG) straight into this worker's stored
/// buffer. The full-res RGBA never crosses back to the app thread, so the
/// app owns no full-size copy at rest.
#[wasm_bindgen]
pub fn load_photo(id: u32, bytes: Uint8Array) -> Result<JsValue, JsValue> {
    let (px, w, h) = rawcore::photo_rgba(&bytes.to_vec())
        .ok_or_else(|| JsValue::from_str("photo decode failed"))?;
    clear_render_caches();
    drop_neutral(id);
    BUFFERS.with(|b| {
        b.borrow_mut().insert(id, (px, w, h));
    });
    Ok(result_obj(&[
        ("w", (w as u32).into()),
        ("h", (h as u32).into()),
        ("mem", wasm_mem_bytes().into()),
    ]))
}

/// Decode a stored display proxy into this worker's buffer and also hand the
/// RGBA back as a transferred buffer. The app needs a main-thread view to seed
/// the hi-res canvas and crop the focus loupe, but the decode and the worker's
/// copy both stay off the app thread — on reload the proxy decode was the
/// longest main-thread task.
#[wasm_bindgen]
pub fn load_photo_view(id: u32, bytes: Uint8Array) -> Result<JsValue, JsValue> {
    let (px, w, h) = rawcore::photo_rgba(&bytes.to_vec())
        .ok_or_else(|| JsValue::from_str("photo decode failed"))?;
    clear_render_caches();
    drop_neutral(id);
    let view = js_buf(px.clone());
    BUFFERS.with(|b| {
        b.borrow_mut().insert(id, (px, w, h));
    });
    Ok(result_obj(&[
        ("pixels", view),
        ("w", (w as u32).into()),
        ("h", (h as u32).into()),
        ("mem", wasm_mem_bytes().into()),
    ]))
}

/// Copy a native-resolution window out of the stored buffer. The focus loupe
/// asks for a display-sized rect, so only that rect travels back.
#[wasm_bindgen]
pub fn crop_window(
    id: u32,
    sx: u32,
    sy: u32,
    cw: u32,
    ch: u32,
) -> Result<JsValue, JsValue> {
    with_buffer(id, |(full, w, h)| {
        let (sx, sy, cw, ch) = (sx as usize, sy as usize, cw as usize, ch as usize);
        if cw == 0 || ch == 0 || sx + cw > *w || sy + ch > *h {
            return Err(JsValue::from_str("crop out of bounds"));
        }
        let crop = crop_rows(full, *w, sx, sy, cw, ch, 4);
        Ok(result_obj(&[
            ("pixels", js_buf(crop)),
            ("w", (cw as u32).into()),
            ("h", (ch as u32).into()),
        ]))
    })?
}

/// Crop a margin-expanded window out of the stored buffer and run the
/// pointwise edit pipeline over it at source density — the editor hover
/// loupe's direct path, previously a main-thread crop of the full buffer.
#[wasm_bindgen]
pub fn loupe_window(
    id: u32,
    recipe: &str,
    sx: u32,
    sy: u32,
    cw: u32,
    ch: u32,
    seed: u32,
) -> Result<JsValue, JsValue> {
    let edit = parse_edit(recipe);
    with_buffer(id, |(full, fw, fh)| {
        let (sx, sy, cw, ch) = (sx as usize, sy as usize, cw as usize, ch as usize);
        if cw == 0 || ch == 0 || sx + cw > *fw || sy + ch > *fh {
            return Err(JsValue::from_str("loupe window out of bounds"));
        }
        let margin = (edit.blur / 100.0 * 8.0 * 3.0).ceil() as usize + 2;
        let x0 = sx.saturating_sub(margin);
        let y0 = sy.saturating_sub(margin);
        let x1 = (sx + cw + margin).min(*fw);
        let y1 = (sy + ch + margin).min(*fh);
        let (ew, eh) = (x1 - x0, y1 - y0);
        let win = crop_rows(full, *fw, x0, y0, ew, eh, 4);
        let mut frame = Frame::new(win, ew, eh);
        pipeline::finish(&mut frame, &edit, seed, None, 1.0);
        let win = frame.pixels;
        let (ix, iy) = (sx - x0, sy - y0);
        let mut out = Vec::with_capacity(cw * ch * 4);
        for row in 0..ch {
            let start = ((iy + row) * ew + ix) * 4;
            out.extend_from_slice(&win[start..start + cw * 4]);
        }
        Ok(result_obj(&[
            ("pixels", js_buf(out)),
            ("w", (cw as u32).into()),
            ("h", (ch as u32).into()),
        ]))
    })?
}

#[wasm_bindgen]
pub fn unload(id: u32) {
    clear_render_caches();
    drop_neutral(id);
    BUFFERS.with(|b| {
        b.borrow_mut().remove(&id);
    });
    RAW_FULL.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.as_ref().is_some_and(|f| f.id == id) {
            *slot = None;
        }
    });
    RAW_TONE.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.as_ref().is_some_and(|(cid, _)| *cid == id) {
            *slot = None;
        }
    });
}

fn parse_edit(recipe: &str) -> EditParams {
    edit_params_from_recipe(recipe).unwrap_or_default()
}

/// Geometry + resample front half of the render pipeline, with the base
/// cache. Returns pre-develop, pre-colour pixels.
fn base_pipeline(
    id: u32,
    full: &[u8],
    fw: usize,
    fh: usize,
    edit: &EditParams,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
) -> (Vec<u8>, usize, usize, f64, f64, f64) {
    let t0 = js_sys::Date::now();
    // Target dims are pure arithmetic, so the cache key is computable without
    // rotating anything.
    let (ww, wh) = working_dims(fw, fh, edit);
    let (tw, th) = hi_target(req_w, req_h, ww, wh, max_edge);
    let fine_bits = edit.fine_angle.to_bits();
    let cached = BASE_CACHE.with(|c| {
        c.borrow().as_ref().is_some_and(|b| {
            b.id == id
                && b.rot90 == edit.rot90
                && b.fine_bits == fine_bits
                && b.tw == tw
                && b.th == th
        })
    });
    let (buf, t1, t2) = if cached {
        // A tonal edit only needs a fresh copy of the base; the ~27 MB
        // memcpy is far cheaper than geometry over the sensor frame.
        let v = BASE_CACHE.with(|c| {
            c.borrow().as_ref().map(|b| b.pixels.clone()).unwrap_or_default()
        });
        let t = js_sys::Date::now();
        (v, t, t)
    } else {
        let (p, gw, gh) = geometry(full, fw, fh, edit);
        let t = js_sys::Date::now();
        let buf = pipeline::resize(&p, gw, gh, Target::new(tw, th)).pixels;
        // Store the pre-develop base: later renders re-run rawops on the
        // copy, so exposure edits never compound into the cached pixels.
        BASE_CACHE.with(|c| {
            *c.borrow_mut() = Some(BaseCache {
                id,
                rot90: edit.rot90,
                fine_bits,
                tw,
                th,
                pixels: buf.clone(),
            })
        });
        (buf, t, js_sys::Date::now())
    };
    let hit = if cached { 1.0 } else { 0.0 };
    (buf, tw, th, t1 - t0, t2 - t1, hit)
}

/// Parametric pipeline (geometry → resample → color ops), mirroring the app's
/// render_hi. Selection comes as JSON + optional mask buffer.
fn render_pipeline(
    id: u32,
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
) -> (Vec<u8>, usize, usize, [f64; 5]) {
    let (buf, tw, th, geometry_ms, resample_ms, hit) =
        base_pipeline(id, full, fw, fh, edit, req_w, req_h, max_edge);
    let mut frame = Frame::new(buf, tw, th);
    let t0 = js_sys::Date::now();
    let px_scale = pipeline::px_scale_for(fw, fh, Target::new(tw, th));
    let wide_buf = pipeline::develop(&mut frame, edit, px_scale);
    let t1 = js_sys::Date::now();
    let mask = if edit.is_color_touched() {
        sel_json
            .and_then(|j| selection_from_json(&j, sel_mask.map(|m| m.to_vec())))
            .map(|sel| ops::selection_mask(&sel, tw, th))
    } else {
        None
    };
    pipeline::colour(&mut frame, edit, seed, mask.as_deref(), wide_buf);
    let t2 = js_sys::Date::now();
    (frame.pixels, tw, th, [geometry_ms, resample_ms, t1 - t0, t2 - t1, hit])
}

/// The neutral frame's dims when this item renders from it: a neutral develop
/// is loaded at exactly the stored buffer's size and the edit has no stage
/// the neutral pipeline lacks.
fn neutral_dims(id: u32, edit: &EditParams) -> Option<(usize, usize)> {
    if !pipeline::neutral_eligible(edit) {
        return None;
    }
    let (bw, bh) = BUFFERS.with(|b| b.borrow().get(&id).map(|(_, w, h)| (*w, *h)))?;
    NEUTRAL.with(|n| {
        let n = n.borrow();
        let f = n.get(&id)?;
        (f.w == bw && f.h == bh).then_some((f.w, f.h))
    })
}

/// Geometry + resample on the neutral frame, with a 16-bit base cache keyed
/// like the u8 one. Neutral pixel edits (exposure, highlights, shadows, WB)
/// rerun develop on a copy of this base, so they never compound.
fn wide_base_pipeline(
    id: u32,
    fw: usize,
    fh: usize,
    edit: &EditParams,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
) -> (Vec<u16>, usize, usize, [f64; 3]) {
    let t0 = js_sys::Date::now();
    let (ww, wh) = working_dims(fw, fh, edit);
    let (tw, th) = hi_target(req_w, req_h, ww, wh, max_edge);
    let fine_bits = edit.fine_angle.to_bits();
    let cached = WIDE_BASE_CACHE.with(|c| {
        c.borrow().as_ref().is_some_and(|b| {
            b.id == id
                && b.rot90 == edit.rot90
                && b.fine_bits == fine_bits
                && b.tw == tw
                && b.th == th
        })
    });
    let (buf, t1, t2) = if cached {
        let v = WIDE_BASE_CACHE.with(|c| {
            c.borrow().as_ref().map(|b| b.pixels.clone()).unwrap_or_default()
        });
        let t = js_sys::Date::now();
        (v, t, t)
    } else {
        let (v, t1, t2) = NEUTRAL.with(|n| {
            let n = n.borrow();
            let src = n.get(&id).expect("neutral frame missing");
            let (p, gw, gh) = geometry_wide(&src.pixels, fw, fh, edit);
            let t1 = js_sys::Date::now();
            let frame = pipeline::resize_wide(&p, gw, gh, Target::new(tw, th));
            let t2 = js_sys::Date::now();
            (frame.pixels, t1, t2)
        });
        WIDE_BASE_CACHE.with(|c| {
            *c.borrow_mut() = Some(WideBaseCache {
                id,
                rot90: edit.rot90,
                fine_bits,
                tw,
                th,
                pixels: v.clone(),
            })
        });
        (v, t1, t2)
    };
    let hit = if cached { 1.0 } else { 0.0 };
    (buf, tw, th, [t1 - t0, t2 - t1, hit])
}

fn wide_window_pipeline(
    id: u32,
    fw: usize,
    fh: usize,
    edit: &EditParams,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
) -> (Vec<u16>, usize, usize, [f64; 3]) {
    let t0 = js_sys::Date::now();
    let (ww, wh) = working_dims(fw, fh, edit);
    let (cx, cy, cw, ch) = crop_px(edit, ww, wh);
    let (tw, th) = hi_target(req_w, req_h, cw, ch, max_edge);
    let fine_bits = edit.fine_angle.to_bits();
    let cached = WIDE_WINDOW_CACHE.with(|c| {
        c.borrow().as_ref().is_some_and(|w| {
            w.id == id
                && w.rot90 == edit.rot90
                && w.fine_bits == fine_bits
                && w.crop == (cx, cy, cw, ch)
                && w.tw == tw
                && w.th == th
        })
    });
    let (buf, t1, t2) = if cached {
        let v = WIDE_WINDOW_CACHE.with(|c| {
            c.borrow().as_ref().map(|w| w.pixels.clone()).unwrap_or_default()
        });
        let t = js_sys::Date::now();
        (v, t, t)
    } else {
        let (v, t1, t2) = NEUTRAL.with(|n| {
            let n = n.borrow();
            let src = n.get(&id).expect("neutral frame missing");
            let (p, gw, gh) = geometry_wide(&src.pixels, fw, fh, edit);
            let t1 = js_sys::Date::now();
            let win: Vec<u16> = if (cx, cy, cw, ch) == (0, 0, gw, gh) {
                p.into_owned()
            } else {
                crop_rows(&p, gw, cx, cy, cw, ch, 4)
            };
            let frame = pipeline::resize_wide(&win, cw, ch, Target::new(tw, th));
            let t2 = js_sys::Date::now();
            (frame.pixels, t1, t2)
        });
        WIDE_WINDOW_CACHE.with(|c| {
            *c.borrow_mut() = Some(WideWindowCache {
                id,
                rot90: edit.rot90,
                fine_bits,
                crop: (cx, cy, cw, ch),
                tw,
                th,
                pixels: v.clone(),
            })
        });
        (v, t1, t2)
    };
    let hit = if cached { 1.0 } else { 0.0 };
    (buf, tw, th, [t1 - t0, t2 - t1, hit])
}

/// Develop, camera tone and colour on a neutral frame, then the single 8-bit
/// quantisation. Returns the u8 pixels and (develop+tone, colour) timings.
fn neutral_finish(
    buf: &mut [u16],
    w: usize,
    h: usize,
    edit: &EditParams,
    seed: u32,
    white: f32,
    tone: Option<&[u8; 256]>,
    mask: Option<&[u8]>,
) -> (Vec<u8>, f64, f64) {
    let t0 = js_sys::Date::now();
    rawops::develop_neutral_wide(buf, w, h, &edit.raw, white);
    if let Some(lut) = tone {
        let table = rawops::tone_table_for(lut, edit.raw.highlights);
        rawops::apply_tone_wide(buf, &table);
    }
    let t1 = js_sys::Date::now();
    if edit.is_color_touched() {
        color::apply_color_effects_wide(buf, w, h, edit, seed, mask);
    }
    let t2 = js_sys::Date::now();
    let mut out = vec![0u8; w * h * 4];
    wide::narrow_dithered(buf, &mut out, w, h);
    (out, t1 - t0, t2 - t1)
}

fn selection_mask_for(
    edit: &EditParams,
    w: usize,
    h: usize,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> Option<Vec<u8>> {
    if !edit.is_color_touched() {
        return None;
    }
    sel_json
        .and_then(|j| selection_from_json(&j, sel_mask.map(|m| m.to_vec())))
        .map(|sel| ops::selection_mask(&sel, w, h))
}

fn render_reply(pixels: Vec<u8>, tw: usize, th: usize, t: [f64; 5]) -> JsValue {
    let timings = result_obj(&[
        ("geometry", t[0].into()),
        ("resample", t[1].into()),
        ("develop", t[2].into()),
        ("color", t[3].into()),
        ("cache", t[4].into()),
    ]);
    result_obj(&[
        ("pixels", js_buf(pixels)),
        ("w", (tw as u32).into()),
        ("h", (th as u32).into()),
        ("timings", timings),
        ("mem", wasm_mem_bytes().into()),
    ])
}

fn neutral_render(
    id: u32,
    fw: usize,
    fh: usize,
    edit: &EditParams,
    seed: u32,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> (Vec<u8>, usize, usize, [f64; 5]) {
    let (mut buf, tw, th, t) = wide_base_pipeline(id, fw, fh, edit, req_w, req_h, max_edge);
    let mask = selection_mask_for(edit, tw, th, sel_json, sel_mask);
    let (out, dev_ms, col_ms) = NEUTRAL.with(|n| {
        let n = n.borrow();
        let src = n.get(&id).expect("neutral frame missing");
        neutral_finish(&mut buf, tw, th, edit, seed, src.white, src.tone.as_ref(), mask.as_deref())
    });
    (out, tw, th, [t[0], t[1], dev_ms, col_ms, t[2]])
}

fn neutral_window_render(
    id: u32,
    fw: usize,
    fh: usize,
    edit: &EditParams,
    seed: u32,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> (Vec<u8>, usize, usize, [f64; 5]) {
    let (mut buf, tw, th, t) = wide_window_pipeline(id, fw, fh, edit, req_w, req_h, max_edge);
    let mask = if edit.is_color_touched() {
        let (ww, wh) = working_dims(fw, fh, edit);
        let (cx, cy, cw, ch) = crop_px(edit, ww, wh);
        sel_json
            .and_then(|j| selection_from_json(&j, sel_mask.map(|m| m.to_vec())))
            .map(|sel| {
                let full = ops::selection_mask(&sel, ww, wh);
                let win = crop_rows(&full, ww, cx, cy, cw, ch, 1);
                if (tw, th) != (cw, ch) {
                    nearest_resample_mask(&win, cw, ch, tw, th)
                } else {
                    win
                }
            })
    } else {
        None
    };
    let (out, dev_ms, col_ms) = NEUTRAL.with(|n| {
        let n = n.borrow();
        let src = n.get(&id).expect("neutral frame missing");
        neutral_finish(&mut buf, tw, th, edit, seed, src.white, src.tone.as_ref(), mask.as_deref())
    });
    (out, tw, th, [t[0], t[1], dev_ms, col_ms, t[2]])
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
    let edit = parse_edit(recipe);
    if let Some((fw, fh)) = neutral_dims(id, &edit) {
        let (buf, tw, th, t) = neutral_render(
            id, fw, fh, &edit, id, req_w, req_h, max_edge, sel_json, sel_mask,
        );
        return Ok(render_reply(buf, tw, th, t));
    }
    with_buffer(id, |(full, fw, fh)| {
        let (buf, tw, th, t) = render_pipeline(
            id, full, *fw, *fh, &edit, id, req_w, req_h, max_edge, sel_json, sel_mask,
        );
        render_reply(buf, tw, th, t)
    })
}

/// Base for the GPU preview: geometry + resample only, before develop and
/// colour. Shares `BASE_CACHE` with `render`, so CPU and GPU renders of the
/// same (item, geometry, target) never rebuild the base. Returns
/// {pixels, w, h, timings:{geometry, resample, cache}}.
#[wasm_bindgen]
pub fn render_base(
    id: u32,
    recipe: &str,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
) -> Result<JsValue, JsValue> {
    with_buffer(id, |(full, fw, fh)| {
        let edit = parse_edit(recipe);
        let (buf, tw, th, geometry_ms, resample_ms, hit) =
            base_pipeline(id, full, *fw, *fh, &edit, req_w, req_h, max_edge);
        let timings = result_obj(&[
            ("geometry", geometry_ms.into()),
            ("resample", resample_ms.into()),
            ("cache", hit.into()),
        ]);
        result_obj(&[
            ("pixels", js_buf(buf)),
            ("w", (tw as u32).into()),
            ("h", (th as u32).into()),
            ("timings", timings),
            ("mem", wasm_mem_bytes().into()),
        ])
    })
}

/// Copy a sub-rect out of a `bpp`-element-per-pixel buffer, row by row.
fn crop_rows<T: Copy>(
    p: &[T],
    w: usize,
    cx: usize,
    cy: usize,
    cw: usize,
    ch: usize,
    bpp: usize,
) -> Vec<T> {
    let mut out = Vec::with_capacity(cw * ch * bpp);
    for y in cy..cy + ch {
        let s = (y * w + cx) * bpp;
        out.extend_from_slice(&p[s..s + cw * bpp]);
    }
    out
}

fn nearest_resample_mask(p: &[u8], w: usize, h: usize, tw: usize, th: usize) -> Vec<u8> {
    let mut out = vec![0u8; tw * th];
    for y in 0..th {
        let sy = y * h / th;
        for x in 0..tw {
            out[y * tw + x] = p[sy * w + x * w / tw];
        }
    }
    out
}

/// Render only the crop window the cropped view shows. The whole-frame
/// hi-res cache is capped at the display proxy's long edge, so a tight crop
/// would blit only a fraction of the device pixels; this renders the window
/// itself, at native resolution up to `max_edge`. `req_w`/`req_h` are the
/// device pixels the window must cover. Everything before `rawops`/colour is
/// cached per crop rect, so a slider tick copies the window instead of
/// rotating and cropping the frame again.
fn render_window_pipeline(
    id: u32,
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
) -> (Vec<u8>, usize, usize, [f64; 5]) {
    let t0 = js_sys::Date::now();
    let (ww, wh) = working_dims(fw, fh, edit);
    let (cx, cy, cw, ch) = crop_px(edit, ww, wh);
    let (tw, th) = hi_target(req_w, req_h, cw, ch, max_edge);
    let fine_bits = edit.fine_angle.to_bits();
    let cached = WINDOW_CACHE.with(|c| {
        c.borrow().as_ref().is_some_and(|w| {
            w.id == id
                && w.rot90 == edit.rot90
                && w.fine_bits == fine_bits
                && w.crop == (cx, cy, cw, ch)
                && w.tw == tw
                && w.th == th
        })
    });
    let (buf, t1, t2) = if cached {
        let v = WINDOW_CACHE.with(|c| {
            c.borrow().as_ref().map(|w| w.pixels.clone()).unwrap_or_default()
        });
        let t = js_sys::Date::now();
        (v, t, t)
    } else {
        let (p, gw, gh) = geometry(full, fw, fh, edit);
        let t = js_sys::Date::now();
        // Identity geometry borrows the stored frame: copy only the window's
        // rows. A rotation has already produced a full working frame.
        let win = if (cx, cy, cw, ch) == (0, 0, gw, gh) {
            p.into_owned()
        } else {
            crop_rows(&p, gw, cx, cy, cw, ch, 4)
        };
        let buf = pipeline::resize(&win, cw, ch, Target::new(tw, th)).pixels;
        WINDOW_CACHE.with(|c| {
            *c.borrow_mut() = Some(WindowCache {
                id,
                rot90: edit.rot90,
                fine_bits,
                crop: (cx, cy, cw, ch),
                tw,
                th,
                pixels: buf.clone(),
            })
        });
        (buf, t, js_sys::Date::now())
    };
    let mut frame = Frame::new(buf, tw, th);
    let px_scale = pipeline::px_scale_for(fw, fh, Target::new(ww, wh))
        * pipeline::px_scale_for(cw, ch, Target::new(tw, th));
    let wide_buf = pipeline::develop(&mut frame, edit, px_scale);
    let t3 = js_sys::Date::now();
    let hit = if cached { 1.0 } else { 0.0 };
    let mask = if edit.is_color_touched() {
        sel_json
            .and_then(|j| selection_from_json(&j, sel_mask.map(|m| m.to_vec())))
            .map(|sel| {
                // Build the mask over the full working frame (selection
                // coordinates are normalized to it), then take the window and
                // scale it.
                let mask = ops::selection_mask(&sel, ww, wh);
                let win = crop_rows(&mask, ww, cx, cy, cw, ch, 1);
                if (tw, th) != (cw, ch) {
                    nearest_resample_mask(&win, cw, ch, tw, th)
                } else {
                    win
                }
            })
    } else {
        None
    };
    pipeline::colour(&mut frame, edit, seed, mask.as_deref(), wide_buf);
    let t4 = js_sys::Date::now();
    (frame.pixels, tw, th, [t1 - t0, t2 - t1, t3 - t2, t4 - t3, hit])
}

/// Render the crop window the cropped view displays. Returns {pixels, w, h}.
#[wasm_bindgen]
pub fn render_window(
    id: u32,
    recipe: &str,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> Result<JsValue, JsValue> {
    let edit = parse_edit(recipe);
    if let Some((fw, fh)) = neutral_dims(id, &edit) {
        let (buf, tw, th, t) = neutral_window_render(
            id, fw, fh, &edit, id, req_w, req_h, max_edge, sel_json, sel_mask,
        );
        return Ok(render_reply(buf, tw, th, t));
    }
    with_buffer(id, |(full, fw, fh)| {
        let (buf, tw, th, t) = render_window_pipeline(
            id, full, *fw, *fh, &edit, id, req_w, req_h, max_edge, sel_json, sel_mask,
        );
        render_reply(buf, tw, th, t)
    })
}

fn downscale(pixels: &[u8], w: usize, h: usize, long_edge: u32) -> (Vec<u8>, usize, usize) {
    resample::fit_long_edge(pixels, w, h, long_edge)
}

/// Destructive ops make their result the item's new stored buffer. The app
/// bakes rotation into the result and keeps the crop, which is applied at
/// render time over the working frame.
fn commit(
    id: u32,
    full: &mut Vec<u8>,
    fw: &mut usize,
    fh: &mut usize,
    base: &[u8],
    w: usize,
    h: usize,
) {
    clear_render_caches();
    // The neutral develop describes the pre-op pixels; after a destructive
    // op it no longer matches the stored frame.
    drop_neutral(id);
    full.clear();
    full.extend_from_slice(base);
    *fw = w;
    *fh = h;
}

/// Destructive-op result: the worker already holds the new full buffer, so
/// only dims and the 1024px preview travel back.
fn destructive_result(base: &[u8], w: usize, h: usize) -> JsValue {
    let (pv, pw, ph) = downscale(base, w, h, 1024);
    result_obj(&[
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
        let (base, w, h) = geometry(full, *fw, *fh, &edit);
        let mut base = base.into_owned();
        let diag = ((w * w + h * h) as f32).sqrt();
        let r = (radius_frac * diag).max(1.0);
        let pts = parse_pts(pts);
        let mut coverage = vec![0u8; w * h];
        stamp_stroke(&mut coverage, w, h, &pts, r);
        if !heal::spot_heal(&mut base, &coverage, w, h, 1.0, heal_mode_wire(mode), id) {
            return JsValue::NULL;
        }
        commit(id, full, fw, fh, &base, w, h);
        destructive_result(&base, w, h)
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
        let (base, w, h) = geometry(full, *fw, *fh, &edit);
        let mut base = base.into_owned();
        let diag = ((w * w + h * h) as f32).sqrt();
        let r = (radius_frac * diag).max(1.0);
        let pts = parse_pts(pts);
        let mut coverage = vec![0u8; w * h];
        stamp_stroke(&mut coverage, w, h, &pts, r);
        clone::clone_stamp(&mut base, w, h, &coverage, dx, dy);
        commit(id, full, fw, fh, &base, w, h);
        destructive_result(&base, w, h)
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
        let (base, w, h) = geometry(full, *fw, *fh, &edit);
        let mut base = base.into_owned();
        let Some(sel) = selection_from_json(sel_json, sel_mask.map(|m| m.to_vec())) else {
            return JsValue::NULL;
        };
        let mask = ops::selection_mask(&sel, w, h);
        delete_by_mask(&mut base, &mask);
        commit(id, full, fw, fh, &base, w, h);
        destructive_result(&base, w, h)
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
        let (base, w, h) = geometry(full, *fw, *fh, &edit);
        let mut base = base.into_owned();
        let Some(sel) = selection_from_json(sel_json, sel_mask.map(|m| m.to_vec())) else {
            return JsValue::NULL;
        };
        let mask = ops::selection_mask(&sel, w, h);
        if !fill::content_fill(&mut base, &mask, w, h) {
            return JsValue::NULL;
        }
        commit(id, full, fw, fh, &base, w, h);
        destructive_result(&base, w, h)
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
        let (base, w, h) = geometry(full, *fw, *fh, &edit);
        let mut base = base.into_owned();
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
            commit(id, full, fw, fh, &base, w, h);
            let (pv, pw, ph) = downscale(&base, w, h, 1024);
            pairs.push(("w", (w as u32).into()));
            pairs.push(("h", (h as u32).into()));
            pairs.push(("preview", js_buf(pv)));
            pairs.push(("pw", (pw as u32).into()));
            pairs.push(("ph", (ph as u32).into()));
        }
        result_obj(&pairs)
    })
}

fn proxy_result(
    px: Vec<u8>,
    w: usize,
    h: usize,
    neutral: Vec<u16>,
    white: f32,
    full_w: usize,
    full_h: usize,
    tone: Option<[u8; 256]>,
) -> JsValue {
    result_obj(&[
        ("pixels", js_buf(px)),
        ("w", (w as u32).into()),
        ("h", (h as u32).into()),
        // Un-toned 16-bit develop at the proxy's size; empty for camera
        // JPEGs and embedded previews, which have no demosaic to redevelop.
        ("neutral", js_u16_buf(neutral)),
        ("white", white.into()),
        ("full_w", (full_w as u32).into()),
        ("full_h", (full_h as u32).into()),
        // The tone curve used for these pixels; the export develop reuses it
        // so display and export share one response. Null for camera JPEGs.
        ("tone", tone.map(|t| js_buf(t.to_vec())).unwrap_or(JsValue::NULL)),
        ("mem", wasm_mem_bytes().into()),
    ])
}

/// Demosaic a RAW in the worker and return a display-resolution proxy, so
/// import previews never block the main thread or need the full buffer. Large
/// Bayer sensors develop at half size (2×2 binning); a full-size frame stays
/// here for `raw_develop` to reuse, and either way the tone curve does.
/// `retain` keeps those caches — false on the batch pool, whose frame the
/// interactive worker's export can never reuse.
#[wasm_bindgen]
pub fn raw_proxy(
    id: u32,
    bytes: Uint8Array,
    max_edge: u32,
    retain: bool,
) -> Result<JsValue, JsValue> {
    let p = rawcore::decode_raw_proxy(&bytes.to_vec(), max_edge)
        .map_err(|e| JsValue::from_str(&e))?;
    let (px, pw, ph) = downscale(&p.rgba, p.w, p.h, max_edge);
    // The proxy's neutral must land at the proxy buffer's dims: the app loads
    // it into the render worker under the proxy's id, whose geometry runs on
    // those u8 pixels.
    let neutral = if (p.w, p.h) == (pw, ph) {
        p.neutral.clone()
    } else {
        resample::resample_wide(&p.neutral, p.w, p.h, pw, ph)
    };
    let (full_w, full_h, white, tone, half) = (p.full_w, p.full_h, p.white, p.tone, p.half);
    if retain {
        RAW_FULL.with(|c| {
            *c.borrow_mut() = if half {
                None
            } else {
                Some(RawFull {
                    id,
                    rgba: p.rgba,
                    neutral: p.neutral,
                    white,
                    tone,
                    w: p.w,
                    h: p.h,
                })
            }
        });
        RAW_TONE.with(|c| *c.borrow_mut() = tone.map(|t| (id, t)));
    }
    Ok(proxy_result(px, pw, ph, neutral, white, full_w, full_h, tone))
}

/// Decode the camera's embedded full-size JPEG (no EXIF of its own) into a
/// display-resolution proxy, correcting orientation from the RAW metadata.
#[wasm_bindgen]
pub fn camera_proxy(jpeg: Uint8Array, orientation: u16, max_edge: u32) -> Result<JsValue, JsValue> {
    let (rgba, w, h) = rawcore::camera_jpeg_rgba(&jpeg.to_vec(), orientation)
        .ok_or_else(|| JsValue::from_str("camera JPEG decode failed"))?;
    let (px, pw, ph) = downscale(&rgba, w, h, max_edge);
    Ok(proxy_result(px, pw, ph, Vec::new(), 1.0, w, h, None))
}

/// RAW metadata in one round-trip: EXIF payload, orientation, capture time,
/// the camera JPEG, and (when `preview_edge` > 0) a display-resolution
/// preview. Keeps rawler out of the app bundle.
#[wasm_bindgen]
pub fn raw_meta(bytes: Uint8Array, preview_edge: u32) -> Result<JsValue, JsValue> {
    let m = rawcore::raw_meta(&bytes.to_vec())
        .ok_or_else(|| JsValue::from_str("unsupported RAW"))?;
    let (pv, pw, ph) = match (&m.preview, preview_edge) {
        (Some((px, w, h)), e) if e > 0 => downscale(px, *w, *h, e),
        _ => (Vec::new(), 0, 0),
    };
    let first = DECODER_FIRST_MS.with(|c| {
        let first = c.get().is_none();
        if first {
            c.set(Some(m.get_decoder_ms));
        }
        first
    });
    Ok(result_obj(&[
        ("exif_tiff", m.exif_tiff.map(js_buf).unwrap_or(JsValue::NULL)),
        ("orientation", (m.orientation as u32).into()),
        ("taken", m.taken.map(JsValue::from_f64).unwrap_or(JsValue::NULL)),
        ("camera_jpeg", m.camera_jpeg.map(js_buf).unwrap_or(JsValue::NULL)),
        ("camera_w", (m.camera_w as u32).into()),
        ("camera_h", (m.camera_h as u32).into()),
        ("preview", js_buf(pv)),
        ("preview_w", (pw as u32).into()),
        ("preview_h", (ph as u32).into()),
        ("get_decoder_ms", m.get_decoder_ms.into()),
        ("get_decoder_first", first.into()),
        ("mem", wasm_mem_bytes().into()),
    ]))
}

/// Full sensor develop — the multi-second demosaic. Runs here so the app
/// thread never blocks on RAW decoding. Reuses the last proxy develop's frame
/// when it is the same item, or its tone curve when the proxy was half-size; a
/// stale frame is dropped before developing. `tone` is the proxy's LUT when the
/// app has one from another worker (the batch pool) — an empty array means the
/// app has no curve for this item.
#[wasm_bindgen]
pub fn raw_develop(id: u32, bytes: Uint8Array, tone: Uint8Array) -> Result<JsValue, JsValue> {
    let explicit: Option<[u8; 256]> = if tone.length() == 256 {
        let mut lut = [0u8; 256];
        tone.copy_to(&mut lut);
        Some(lut)
    } else {
        None
    };
    let cached = RAW_FULL.with(|c| {
        let mut slot = c.borrow_mut();
        let hit = slot.as_ref().map(|f| f.id == id).unwrap_or(false);
        let taken = slot.take();
        if hit {
            taken
        } else {
            None
        }
    });
    let (rgba, neutral, w, h, white, tone) = match cached {
        Some(f) => (f.rgba, f.neutral, f.w, f.h, f.white, f.tone),
        None => {
            let tone = explicit.or_else(|| {
                RAW_TONE.with(|c| {
                    let mut slot = c.borrow_mut();
                    let hit = slot.as_ref().is_some_and(|(cid, _)| *cid == id);
                    let taken = slot.take();
                    if hit {
                        taken.map(|(_, t)| t)
                    } else {
                        None
                    }
                })
            });
            let d = rawcore::decode_raw_wide(&bytes.to_vec(), tone.as_ref())
                .map_err(|e| JsValue::from_str(&e))?;
            (d.rgba, d.neutral, d.w, d.h, d.white, d.tone)
        }
    };
    let tone_field = tone.map(|t| js_buf(t.to_vec())).unwrap_or(JsValue::NULL);
    Ok(result_obj(&[
        ("pixels", js_buf(rgba)),
        ("neutral", js_u16_buf(neutral)),
        ("w", (w as u32).into()),
        ("h", (h as u32).into()),
        ("white", white.into()),
        ("tone", tone_field),
        ("mem", wasm_mem_bytes().into()),
    ]))
}

/// Embedded preview (no demosaic) at display resolution.
#[wasm_bindgen]
pub fn raw_preview(bytes: Uint8Array, max_edge: u32) -> Result<JsValue, JsValue> {
    let (rgba, w, h) = rawcore::preview_rgba(&bytes.to_vec())
        .ok_or_else(|| JsValue::from_str("no embedded preview"))?;
    let (px, pw, ph) = downscale(&rgba, w, h, max_edge);
    Ok(proxy_result(px, pw, ph, Vec::new(), 1.0, w, h, None))
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

/// Row-wise RGBA source over the transferred canvas readback. The encoder only
/// ever asks for one row at a time, so this avoids copying the whole frame into
/// wasm memory (~100 MB at 26 MP, enough to crash the renderer on big exports).
struct CanvasRows {
    rgba: Uint8Array,
    w: u16,
    h: u16,
}

impl jpeg_encoder::ImageBuffer for CanvasRows {
    fn get_jpeg_color_type(&self) -> jpeg_encoder::JpegColorType {
        jpeg_encoder::JpegColorType::Ycbcr
    }

    fn width(&self) -> u16 {
        self.w
    }

    fn height(&self) -> u16 {
        self.h
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let stride = self.w as u32 * 4;
        let start = y as u32 * stride;
        let row = self.rgba.subarray(start, start + stride).to_vec();
        for px in row.chunks_exact(4) {
            let (luma, cb, cr) = jpeg_encoder::rgb_to_ycbcr(px[0], px[1], px[2]);
            buffers[0].push(luma);
            buffers[1].push(cb);
            buffers[2].push(cr);
        }
    }
}

/// RGBA rows borrowed straight from a wasm-side buffer for encoding, so a
/// rendered export never has to be copied back out to JS first.
struct SliceRows<'a> {
    rgba: &'a [u8],
    w: u16,
    h: u16,
}

impl jpeg_encoder::ImageBuffer for SliceRows<'_> {
    fn get_jpeg_color_type(&self) -> jpeg_encoder::JpegColorType {
        jpeg_encoder::JpegColorType::Ycbcr
    }

    fn width(&self) -> u16 {
        self.w
    }

    fn height(&self) -> u16 {
        self.h
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let stride = self.w as usize * 4;
        let start = y as usize * stride;
        let row = &self.rgba[start..start + stride];
        for px in row.chunks_exact(4) {
            let (luma, cb, cr) = jpeg_encoder::rgb_to_ycbcr(px[0], px[1], px[2]);
            buffers[0].push(luma);
            buffers[1].push(cb);
            buffers[2].push(cr);
        }
    }
}

fn encode_jpeg_slice(rgba: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, JsValue> {
    if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
        return Err(JsValue::from_str("bad jpeg dimensions"));
    }
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    enc.add_icc_profile(srgb_icc::SRGB_ICC)
        .map_err(|e| JsValue::from_str(&format!("icc: {e}")))?;
    enc.encode_image(SliceRows { rgba, w: w as u16, h: h as u16 })
        .map_err(|e| JsValue::from_str(&format!("jpeg: {e}")))?;
    Ok(out)
}

fn encode_png_slice(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, JsValue> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc
            .write_header()
            .map_err(|e| JsValue::from_str(&format!("png: {e}")))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| JsValue::from_str(&format!("png: {e}")))?;
    }
    Ok(out)
}

/// Encode an item's stored buffer as PNG for session persistence. Runs in the
/// worker so the app never holds the raster to bake it.
#[wasm_bindgen]
pub fn encode_png(id: u32) -> Result<JsValue, JsValue> {
    let bytes = with_buffer(id, |(full, w, h)| {
        encode_png_slice(full, *w as u32, *h as u32)
    })??;
    Ok(js_buf(bytes))
}

fn export_mask(
    edit: &EditParams,
    ww: usize,
    wh: usize,
    rect: (usize, usize, usize, usize),
    ew: usize,
    eh: usize,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> Option<Vec<u8>> {
    if !edit.is_color_touched() {
        return None;
    }
    let (cx, cy, cw, ch) = rect;
    sel_json
        .and_then(|j| selection_from_json(&j, sel_mask.map(|m| m.to_vec())))
        .map(|sel| {
            let full = ops::selection_mask(&sel, ww, wh);
            let win = crop_rows(&full, ww, cx, cy, cw, ch, 1);
            if (ew, eh) != (cw, ch) {
                nearest_resample_mask(&win, cw, ch, ew, eh)
            } else {
                win
            }
        })
}

/// Render exactly the export rectangle at export dims. The rect is in working
/// coordinates (the app derives it from dims + crop + aspect lock); the worker
/// applies geometry, crop, resample and the full develop/colour pipeline.
fn export_pixels(
    id: u32,
    fw: usize,
    fh: usize,
    edit: &EditParams,
    rect: (usize, usize, usize, usize),
    ew: usize,
    eh: usize,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> Result<(Vec<u8>, usize, usize), JsValue> {
    let (ww, wh) = working_dims(fw, fh, edit);
    let (cx, cy, cw, ch) = rect;
    if cw == 0 || ch == 0 || cx + cw > ww || cy + ch > wh || ew == 0 || eh == 0 {
        return Err(JsValue::from_str("export rect out of bounds"));
    }
    let neutral = NEUTRAL.with(|n| {
        n.borrow()
            .get(&id)
            .is_some_and(|n| n.w == fw && n.h == fh)
            && pipeline::neutral_eligible(edit)
    });
    if neutral {
        let (mut buf, out_w, out_h) = NEUTRAL.with(|n| {
            let n = n.borrow();
            let src = n
                .get(&id)
                .ok_or_else(|| JsValue::from_str("neutral frame missing"))?;
            let (p, gw, gh) = geometry_wide(&src.pixels, fw, fh, edit);
            let win = if (cx, cy, cw, ch) == (0, 0, gw, gh) {
                p.into_owned()
            } else {
                crop_rows(&p, gw, cx, cy, cw, ch, 4)
            };
            let f = pipeline::resize_wide(&win, cw, ch, Target::new(ew, eh));
            Ok::<_, JsValue>((f.pixels, f.w, f.h))
        })?;
        let mask = export_mask(edit, ww, wh, rect, out_w, out_h, sel_json, sel_mask);
        let (out, _, _) = NEUTRAL.with(|n| {
            let n = n.borrow();
            let src = n
                .get(&id)
                .ok_or_else(|| JsValue::from_str("neutral frame missing"))?;
            Ok::<_, JsValue>(neutral_finish(
                &mut buf,
                out_w,
                out_h,
                edit,
                id,
                src.white,
                src.tone.as_ref(),
                mask.as_deref(),
            ))
        })?;
        return Ok((out, out_w, out_h));
    }
    with_buffer(id, |(full, _, _)| {
        let (p, gw, gh) = geometry(full, fw, fh, edit);
        let win = if (cx, cy, cw, ch) == (0, 0, gw, gh) {
            p.into_owned()
        } else {
            crop_rows(&p, gw, cx, cy, cw, ch, 4)
        };
        let resized = pipeline::resize(&win, cw, ch, Target::new(ew, eh));
        let mut frame = Frame::new(resized.pixels, resized.w, resized.h);
        let px_scale = pipeline::px_scale_for(fw, fh, Target::new(ww, wh))
            * pipeline::px_scale_for(cw, ch, Target::new(ew, eh));
        let wide_buf = pipeline::develop(&mut frame, edit, px_scale);
        let mask = export_mask(edit, ww, wh, rect, frame.w, frame.h, sel_json, sel_mask);
        pipeline::colour(&mut frame, edit, id, mask.as_deref(), wide_buf);
        Ok::<_, JsValue>((frame.pixels, frame.w, frame.h))
    })?
}

/// Full export render + encode in one call, so a layer-free export never puts
/// a full-size RGBA frame on the app thread. `format`: 0 = JPEG, 1 = PNG.
/// Returns the encoded bytes as a transferable ArrayBuffer.
#[wasm_bindgen]
pub fn export_photo(
    id: u32,
    recipe: &str,
    cx: u32,
    cy: u32,
    cw: u32,
    ch: u32,
    ew: u32,
    eh: u32,
    format: u8,
    quality: u8,
    sel_json: Option<String>,
    sel_mask: Option<Uint8Array>,
) -> Result<JsValue, JsValue> {
    let edit = parse_edit(recipe);
    let (fw, fh) = BUFFERS
        .with(|b| b.borrow().get(&id).map(|(_, w, h)| (*w, *h)))
        .ok_or_else(|| JsValue::from_str("not loaded"))?;
    let (px, out_w, out_h) = export_pixels(
        id,
        fw,
        fh,
        &edit,
        (cx as usize, cy as usize, cw as usize, ch as usize),
        ew as usize,
        eh as usize,
        sel_json,
        sel_mask,
    )?;
    let bytes = if format == 1 {
        encode_png_slice(&px, out_w as u32, out_h as u32)?
    } else {
        encode_jpeg_slice(&px, out_w as u32, out_h as u32, quality)?
    };
    Ok(result_obj(&[
        ("bytes", js_buf(bytes)),
        ("w", (out_w as u32).into()),
        ("h", (out_h as u32).into()),
    ]))
}

/// Encode RGBA pixels as a 4:4:4 JPEG carrying the sRGB ICC profile.
/// `rgba` arrives as a transferred ArrayBuffer from the export canvas, so
/// the encode runs off the main thread. Returns the JPEG bytes as a
/// transferable ArrayBuffer.
#[wasm_bindgen]
pub fn encode_jpeg(rgba: Uint8Array, w: u32, h: u32, quality: u8) -> Result<JsValue, JsValue> {
    if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
        return Err(JsValue::from_str("bad jpeg dimensions"));
    }
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    // F_1_1 keeps full chroma (4:4:4); the default 4:2:0 smears colour on
    // high-frequency detail such as foliage and fabric.
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    enc.add_icc_profile(srgb_icc::SRGB_ICC)
        .map_err(|e| JsValue::from_str(&format!("icc: {e}")))?;
    enc.encode_image(CanvasRows { rgba, w: w as u16, h: h as u16 })
        .map_err(|e| JsValue::from_str(&format!("jpeg: {e}")))?;
    Ok(js_buf(out))
}
