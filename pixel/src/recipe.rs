//! Edit-param and selection wire formats (JSON), shared by the Drive
//! manifest, the session store, and the pixel-worker protocol.

use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

use crate::curves::{CurvePoint, CurvesSettings};
use crate::levels::{LevelRange, LevelsSettings};
use crate::types::{
    AdjustLayer, Aspect, CropRect, EditParams, PointAdjust, RetouchStroke, Selection,
    SelectionKind,
};

pub fn jget(v: &JsValue, key: &str) -> Option<JsValue> {
    let r = js_sys::Reflect::get(v, &JsValue::from_str(key)).ok()?;
    if r.is_undefined() || r.is_null() {
        None
    } else {
        Some(r)
    }
}

pub fn jnum(v: &JsValue, key: &str) -> Option<f64> {
    jget(v, key)?.as_f64()
}

pub fn jbool(v: &JsValue, key: &str) -> Option<bool> {
    jget(v, key)?.as_bool()
}

fn aspect_wire(a: Aspect) -> &'static str {
    match a {
        Aspect::Original => "original",
        Aspect::Nine16 => "9:16",
        Aspect::One1 => "1:1",
        Aspect::Four5 => "4:5",
        Aspect::Sixteen9 => "16:9",
    }
}

fn levels_json(l: &LevelsSettings) -> String {
    let chans: Vec<String> = l
        .ranges
        .iter()
        .map(|r| format!("[{},{},{},{},{}]", r.black, r.gamma, r.white, r.output_black, r.output_white))
        .collect();
    format!("[{}]", chans.join(","))
}

fn levels_from_json(l: &JsValue) -> Option<LevelsSettings> {
    if !js_sys::Array::is_array(l) {
        return None;
    }
    let arr: js_sys::Array = l.clone().unchecked_into();
    if arr.length() != 4 {
        return None;
    }
    let mut ranges = [LevelRange::default(); 4];
    for (i, chan) in arr.iter().enumerate() {
        if !js_sys::Array::is_array(&chan) {
            return None;
        }
        let row: js_sys::Array = chan.unchecked_into();
        if row.length() != 5 {
            return None;
        }
        let n = |j: u32| row.get(j).as_f64().unwrap_or_default();
        ranges[i] = LevelRange {
            black: n(0),
            gamma: n(1),
            white: n(2),
            output_black: n(3),
            output_white: n(4),
        };
    }
    Some(LevelsSettings { ranges })
}

fn curves_json(c: &CurvesSettings) -> String {
    let chans: Vec<String> = c
        .channels
        .iter()
        .map(|pts| {
            let ps: Vec<String> = pts.iter().map(|p| format!("[{},{}]", p.x, p.y)).collect();
            format!("[{}]", ps.join(","))
        })
        .collect();
    format!("[{}]", chans.join(","))
}

fn curves_from_json(c: &JsValue) -> Option<CurvesSettings> {
    if !js_sys::Array::is_array(c) {
        return None;
    }
    let arr: js_sys::Array = c.clone().unchecked_into();
    if arr.length() != 4 {
        return None;
    }
    let mut channels: [Vec<CurvePoint>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (i, chan) in arr.iter().enumerate() {
        let pts: js_sys::Array = chan.unchecked_into();
        for p in pts.iter() {
            let pair: js_sys::Array = p.unchecked_into();
            if pair.length() != 2 {
                return None;
            }
            channels[i].push(CurvePoint {
                x: pair.get(0).as_f64().unwrap_or_default(),
                y: pair.get(1).as_f64().unwrap_or_default(),
            });
        }
    }
    let parsed = CurvesSettings { channels };
    parsed.is_valid().then_some(parsed)
}

pub fn point_adjust_json(a: &PointAdjust) -> String {
    let mut parts: Vec<String> = Vec::new();
    if a.brightness != 0.0 {
        parts.push(format!("\"b\":{}", a.brightness));
    }
    if a.contrast != 0.0 {
        parts.push(format!("\"c\":{}", a.contrast));
    }
    if a.saturation != 0.0 {
        parts.push(format!("\"s\":{}", a.saturation));
    }
    if a.warmth != 0.0 {
        parts.push(format!("\"w\":{}", a.warmth));
    }
    if !a.levels.is_identity() {
        parts.push(format!("\"levels\":{}", levels_json(&a.levels)));
    }
    if !a.curves.is_identity() {
        parts.push(format!("\"curves\":{}", curves_json(&a.curves)));
    }
    format!("{{{}}}", parts.join(","))
}

pub fn point_adjust_from_json(v: &JsValue) -> PointAdjust {
    PointAdjust {
        brightness: jnum(v, "b").unwrap_or(0.0) as f32,
        contrast: jnum(v, "c").unwrap_or(0.0) as f32,
        saturation: jnum(v, "s").unwrap_or(0.0) as f32,
        warmth: jnum(v, "w").unwrap_or(0.0) as f32,
        levels: jget(v, "levels").and_then(|l| levels_from_json(&l)).unwrap_or_default(),
        curves: jget(v, "curves").and_then(|c| curves_from_json(&c)).unwrap_or_default(),
    }
}

/// Portable selection form: vector selections inline, raster masks as a
/// grayscale PNG so the recipe alone can rebuild them (Drive round-trip).
pub fn selection_portable_json(sel: &Selection) -> String {
    match &sel.kind {
        SelectionKind::Mask { data, width, height } => {
            let png = crate::mask_png::encode_gray(data, *width, *height).unwrap_or_default();
            format!(
                "{{\"t\":\"png\",\"w\":{},\"h\":{},\"f\":{},\"d\":\"{}\"}}",
                width, height, sel.feather, png
            )
        }
        _ => selection_json(sel),
    }
}

/// Worker selection form: raster masks become a reference the worker resolves
/// from its cache by (layer id, mask version) — the bytes never ride every
/// render call.
fn selection_worker_json(sel: &Selection, mask_version: u64) -> String {
    match &sel.kind {
        SelectionKind::Mask { width, height, .. } => format!(
            "{{\"t\":\"ref\",\"w\":{},\"h\":{},\"f\":{},\"mv\":{}}}",
            width, height, sel.feather, mask_version
        ),
        _ => selection_json(sel),
    }
}

fn adjust_layers_json(e: &EditParams, portable: bool) -> Option<String> {
    if e.adjust_layers.is_empty() {
        return None;
    }
    let layers: Vec<String> = e
        .adjust_layers
        .iter()
        .map(|l| {
            let mut parts = vec![format!("\"id\":{}", l.id)];
            if !l.visible {
                parts.push("\"vis\":false".into());
            }
            if l.opacity != 1.0 {
                parts.push(format!("\"op\":{}", l.opacity));
            }
            if l.invert {
                parts.push("\"inv\":true".into());
            }
            if l.mask_version != 0 {
                parts.push(format!("\"mv\":{}", l.mask_version));
            }
            parts.push(format!("\"adj\":{}", point_adjust_json(&l.adjust)));
            if let Some(mask) = &l.mask {
                let wire = if portable {
                    selection_portable_json(mask)
                } else {
                    selection_worker_json(mask, l.mask_version)
                };
                parts.push(format!("\"mask\":{}", wire));
            }
            format!("{{{}}}", parts.join(","))
        })
        .collect();
    Some(format!("[{}]", layers.join(",")))
}

fn adjust_layers_from_json(v: &JsValue) -> Vec<AdjustLayer> {
    if !js_sys::Array::is_array(v) {
        return Vec::new();
    }
    let arr: js_sys::Array = v.clone().unchecked_into();
    let mut out = Vec::new();
    for item in arr.iter() {
        let adjust = jget(&item, "adj").map(|a| point_adjust_from_json(&a)).unwrap_or_default();
        out.push(AdjustLayer {
            id: jnum(&item, "id").unwrap_or(0.0) as u64,
            visible: jbool(&item, "vis").unwrap_or(true),
            opacity: (jnum(&item, "op").unwrap_or(1.0) as f32).clamp(0.0, 1.0),
            mask: jget(&item, "mask").and_then(|m| selection_from_layer_value(&m)),
            invert: jbool(&item, "inv").unwrap_or(false),
            mask_version: jnum(&item, "mv").unwrap_or(0.0) as u64,
            adjust,
        });
    }
    out
}

fn strokes_json(e: &EditParams) -> Option<String> {
    if e.strokes.is_empty() {
        return None;
    }
    let out: Vec<String> = e
        .strokes
        .iter()
        .map(|s| {
            let pts: Vec<String> = s.pts.iter().map(|p| format!("[{},{}]", p.0, p.1)).collect();
            let mut parts = vec![
                format!("\"m\":{}", s.mode),
                format!("\"r\":{}", s.radius),
                format!("\"p\":[{}]", pts.join(",")),
            ];
            if let Some((x, y)) = s.src {
                parts.push(format!("\"src\":[{},{}]", x, y));
            }
            if s.seed != 0 {
                parts.push(format!("\"seed\":{}", s.seed));
            }
            format!("{{{}}}", parts.join(","))
        })
        .collect();
    Some(format!("[{}]", out.join(",")))
}

fn strokes_from_json(v: &JsValue) -> Vec<RetouchStroke> {
    if !js_sys::Array::is_array(v) {
        return Vec::new();
    }
    let arr: js_sys::Array = v.clone().unchecked_into();
    let mut out = Vec::new();
    for item in arr.iter() {
        let mode = jnum(&item, "m").unwrap_or(0.0) as u8;
        if mode > RetouchStroke::CLONE {
            continue;
        }
        let mut pts = Vec::new();
        if let Some(pv) = jget(&item, "p") {
            if js_sys::Array::is_array(&pv) {
                let rows: js_sys::Array = pv.unchecked_into();
                for p in rows.iter() {
                    if !js_sys::Array::is_array(&p) {
                        continue;
                    }
                    let pair: js_sys::Array = p.unchecked_into();
                    if pair.length() == 2 {
                        if let (Some(x), Some(y)) = (pair.get(0).as_f64(), pair.get(1).as_f64()) {
                            pts.push((x as f32, y as f32));
                        }
                    }
                }
            }
        }
        if pts.is_empty() {
            continue;
        }
        let src = jget(&item, "src").and_then(|s| {
            if !js_sys::Array::is_array(&s) {
                return None;
            }
            let pair: js_sys::Array = s.unchecked_into();
            if pair.length() != 2 {
                return None;
            }
            Some((pair.get(0).as_f64()? as f32, pair.get(1).as_f64()? as f32))
        });
        if mode == RetouchStroke::CLONE && src.is_none() {
            continue;
        }
        out.push(RetouchStroke {
            mode,
            radius: (jnum(&item, "r").unwrap_or(0.02) as f32).clamp(0.001, 0.25),
            pts,
            src,
            seed: jnum(&item, "seed").unwrap_or(0.0) as u32,
        });
    }
    out
}

/// Parse a layer mask from its recipe form. `"ref"` masks parse as an
/// unresolved raster (dims but no bytes) that the worker fills from its cache;
/// a layer whose raster never arrives is skipped rather than applied globally.
pub fn selection_from_layer_value(v: &JsValue) -> Option<Selection> {
    let feather = jnum(v, "f").unwrap_or(0.0) as f32;
    match jget(v, "t").and_then(|t| t.as_string())?.as_str() {
        "png" => {
            let (data, dw, dh) = crate::mask_png::decode_gray(&jget(v, "d")?.as_string()?)?;
            let w = jnum(v, "w").unwrap_or(dw as f64) as usize;
            let h = jnum(v, "h").unwrap_or(dh as f64) as usize;
            if (w, h) != (dw, dh) {
                return None;
            }
            Some(Selection {
                kind: SelectionKind::Mask { data: Rc::new(data), width: w, height: h },
                feather,
            })
        }
        "ref" => Some(Selection {
            kind: SelectionKind::Mask {
                data: Rc::new(Vec::new()),
                width: jnum(v, "w").unwrap_or(0.0) as usize,
                height: jnum(v, "h").unwrap_or(0.0) as usize,
            },
            feather,
        }),
        _ => selection_from_value(v, None),
    }
}

/// Serialize edit params to a recipe JSON string for the Drive manifest's
/// edits map. Identity fields are omitted to keep the manifest small; returns
/// None when every param is at its default (no entry needed). Adjustment
/// layers carry their masks inline (raster masks as PNG).
pub fn edit_recipe_json(e: &EditParams) -> Option<String> {
    recipe_json(e, true)
}

fn recipe_json(e: &EditParams, portable_masks: bool) -> Option<String> {
    let d = EditParams::default();
    let mut parts: Vec<String> = Vec::new();
    if e.aspect != d.aspect {
        parts.push(format!("\"aspect\":\"{}\"", aspect_wire(e.aspect)));
    }
    if e.crop != d.crop {
        parts.push(format!(
            "\"crop\":{{\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            e.crop.x, e.crop.y, e.crop.w, e.crop.h
        ));
    }
    if e.rot90 != d.rot90 {
        parts.push(format!("\"rot90\":{}", e.rot90));
    }
    if e.fine_angle != d.fine_angle {
        parts.push(format!("\"fine_angle\":{}", e.fine_angle));
    }
    if e.brightness != d.brightness {
        parts.push(format!("\"brightness\":{}", e.brightness));
    }
    if e.contrast != d.contrast {
        parts.push(format!("\"contrast\":{}", e.contrast));
    }
    if e.saturation != d.saturation {
        parts.push(format!("\"saturation\":{}", e.saturation));
    }
    if e.warmth != d.warmth {
        parts.push(format!("\"warmth\":{}", e.warmth));
    }
    if e.blur != d.blur {
        parts.push(format!("\"blur\":{}", e.blur));
    }
    if e.grain != d.grain {
        parts.push(format!("\"grain\":{}", e.grain));
    }
    if e.grain_gaussian != d.grain_gaussian {
        parts.push(format!("\"grain_gaussian\":{}", e.grain_gaussian));
    }
    if e.grain_mono != d.grain_mono {
        parts.push(format!("\"grain_mono\":{}", e.grain_mono));
    }
    if !e.levels.is_identity() {
        parts.push(format!("\"levels\":{}", levels_json(&e.levels)));
    }
    if !e.raw.is_identity() {
        let d = crate::types::RawAdjust::default();
        let mut f: Vec<String> = Vec::new();
        if e.raw.temp != d.temp {
            f.push(format!("\"t\":{}", e.raw.temp));
        }
        if e.raw.tint != d.tint {
            f.push(format!("\"tn\":{}", e.raw.tint));
        }
        if e.raw.exposure != d.exposure {
            f.push(format!("\"ev\":{}", e.raw.exposure));
        }
        if e.raw.highlights != d.highlights {
            f.push(format!("\"hi\":{}", e.raw.highlights));
        }
        if e.raw.shadows != d.shadows {
            f.push(format!("\"sh\":{}", e.raw.shadows));
        }
        if e.raw.noise != d.noise {
            f.push(format!("\"nr\":{}", e.raw.noise));
        }
        if e.raw.denoise != d.denoise {
            f.push(format!("\"dn\":{}", e.raw.denoise));
        }
        if e.raw.sharpen != d.sharpen {
            f.push(format!("\"sp\":{}", e.raw.sharpen));
        }
        parts.push(format!("\"raw\":{{{}}}", f.join(",")));
    }
    if !e.curves.is_identity() {
        parts.push(format!("\"curves\":{}", curves_json(&e.curves)));
    }
    if let Some(adj) = adjust_layers_json(e, portable_masks) {
        parts.push(format!("\"adj\":{}", adj));
    }
    if let Some(hs) = strokes_json(e) {
        parts.push(format!("\"hs\":{}", hs));
    }
    if e.trim != d.trim {
        if let Some((a, b)) = e.trim {
            parts.push(format!("\"trim\":[{},{}]", a, b));
        }
    }
    if e.keep_audio != d.keep_audio {
        parts.push(format!("\"keep_audio\":{}", e.keep_audio));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("{{\"v\":1,{}}}", parts.join(",")))
    }
}

/// Recipe JSON that always carries every field, for the worker protocol —
/// the worker rebuilds EditParams from it and must see explicit zeros.
pub fn edit_params_full_json(e: &EditParams) -> String {
    let mut e = e.clone();
    e.selection = None;
    // The same wire form as a stored recipe, except layer masks travel as
    // cache references — the worker resolves them from a separately sent
    // raster, so a drag never ships megabytes of mask per frame.
    recipe_json(&e, false).unwrap_or_else(|| "{\"v\":1}".into())
}

/// Parse a recipe JSON string back into EditParams. Unknown/missing fields
/// fall back to defaults; malformed input returns None (caller treats it as
/// "no recipe"). The global selection is never part of a recipe; adjustment
/// layers carry their masks (portable PNG form, or worker cache refs).
pub fn edit_params_from_recipe(json: &str) -> Option<EditParams> {
    let v = js_sys::JSON::parse(json).ok()?;
    let mut e = EditParams::default();
    if let Some(s) = jget(&v, "aspect").and_then(|a| a.as_string()) {
        e.aspect = match s.as_str() {
            "9:16" => Aspect::Nine16,
            "1:1" => Aspect::One1,
            "4:5" => Aspect::Four5,
            "16:9" => Aspect::Sixteen9,
            _ => Aspect::Original,
        };
    }
    if let Some(c) = jget(&v, "crop") {
        e.crop = CropRect {
            x: jnum(&c, "x").unwrap_or(0.0) as f32,
            y: jnum(&c, "y").unwrap_or(0.0) as f32,
            w: jnum(&c, "w").unwrap_or(1.0) as f32,
            h: jnum(&c, "h").unwrap_or(1.0) as f32,
        };
    }
    if let Some(n) = jnum(&v, "rot90") {
        e.rot90 = (n as u8).min(3);
    }
    if let Some(n) = jnum(&v, "fine_angle") {
        e.fine_angle = n as f32;
    }
    if let Some(n) = jnum(&v, "brightness") {
        e.brightness = n as f32;
    }
    if let Some(n) = jnum(&v, "contrast") {
        e.contrast = n as f32;
    }
    if let Some(n) = jnum(&v, "saturation") {
        e.saturation = n as f32;
    }
    if let Some(n) = jnum(&v, "warmth") {
        e.warmth = n as f32;
    }
    if let Some(n) = jnum(&v, "blur") {
        e.blur = n as f32;
    }
    if let Some(n) = jnum(&v, "grain") {
        e.grain = n as f32;
    }
    if let Some(b) = jbool(&v, "grain_gaussian") {
        e.grain_gaussian = b;
    }
    if let Some(b) = jbool(&v, "grain_mono") {
        e.grain_mono = b;
    }
    if let Some(l) = jget(&v, "levels") {
        if let Some(parsed) = levels_from_json(&l) {
            e.levels = parsed;
        }
    }
    if let Some(r) = jget(&v, "raw") {
        let mut ra = crate::types::RawAdjust::default();
        if let Some(t) = jnum(&r, "t") {
            ra.temp = (t as f32).clamp(2000.0, 11000.0);
        }
        if let Some(t) = jnum(&r, "tn") {
            ra.tint = (t as f32).clamp(-100.0, 100.0);
        }
        if let Some(t) = jnum(&r, "ev") {
            ra.exposure = (t as f32).clamp(-5.0, 5.0);
        }
        if let Some(t) = jnum(&r, "hi") {
            ra.highlights = (t as f32).clamp(0.0, 100.0);
        }
        if let Some(t) = jnum(&r, "sh") {
            ra.shadows = (t as f32).clamp(0.0, 100.0);
        }
        if let Some(t) = jnum(&r, "nr") {
            ra.noise = (t as f32).clamp(0.0, 100.0);
        }
        if let Some(t) = jnum(&r, "dn") {
            ra.denoise = (t as f32).clamp(0.0, 100.0);
        }
        if let Some(t) = jnum(&r, "sp") {
            ra.sharpen = (t as f32).clamp(0.0, 100.0);
        }
        e.raw = ra;
    }
    if let Some(c) = jget(&v, "curves") {
        if let Some(parsed) = curves_from_json(&c) {
            e.curves = parsed;
        }
    }
    if let Some(a) = jget(&v, "adj") {
        e.adjust_layers = adjust_layers_from_json(&a);
    }
    if let Some(hs) = jget(&v, "hs") {
        e.strokes = strokes_from_json(&hs);
    }
    if let Some(t) = jget(&v, "trim") {
        if js_sys::Array::is_array(&t) {
            let arr: js_sys::Array = t.unchecked_into();
            if arr.length() == 2 {
                if let (Some(a), Some(b)) = (arr.get(0).as_f64(), arr.get(1).as_f64()) {
                    e.trim = Some((a as f32, b as f32));
                }
            }
        }
    }
    if let Some(b) = jbool(&v, "keep_audio") {
        e.keep_audio = b;
    }
    e.selection = None;
    Some(e)
}

// --- selection wire (worker protocol; masks travel as a separate buffer) ----

/// Serialize a selection's geometry to JSON. Mask selections carry dims only;
/// the mask bytes go in a separate Uint8Array argument.
pub fn selection_json(sel: &Selection) -> String {
    let f = format!(",\"f\":{}", sel.feather);
    match &sel.kind {
        SelectionKind::Rect { x, y, w, h } => {
            format!("{{\"t\":\"rect\",\"x\":{},\"y\":{},\"w\":{},\"h\":{}{}}}", x, y, w, h, f)
        }
        SelectionKind::Lasso(pts) => {
            let ps: Vec<String> = pts.iter().map(|p| format!("[{},{}]", p.0, p.1)).collect();
            format!("{{\"t\":\"lasso\",\"pts\":[{}]{}}}", ps.join(","), f)
        }
        SelectionKind::Mask { width, height, .. } => {
            format!("{{\"t\":\"mask\",\"w\":{},\"h\":{}{}}}", width, height, f)
        }
    }
}

/// Rebuild a selection from its JSON geometry plus the mask bytes for
/// "mask" selections.
pub fn selection_from_json(
    json: &str,
    mask: Option<Vec<u8>>,
) -> Option<Selection> {
    let v = js_sys::JSON::parse(json).ok()?;
    selection_from_value(&v, mask)
}

fn selection_from_value(v: &JsValue, mask: Option<Vec<u8>>) -> Option<Selection> {
    let feather = jnum(v, "f").unwrap_or(0.0) as f32;
    let kind = match jget(v, "t").and_then(|t| t.as_string())?.as_str() {
        "rect" => SelectionKind::Rect {
            x: jnum(v, "x").unwrap_or(0.0) as f32,
            y: jnum(v, "y").unwrap_or(0.0) as f32,
            w: jnum(v, "w").unwrap_or(1.0) as f32,
            h: jnum(v, "h").unwrap_or(1.0) as f32,
        },
        "lasso" => {
            let mut pts = Vec::new();
            if let Some(pv) = jget(v, "pts") {
                if js_sys::Array::is_array(&pv) {
                    let arr: js_sys::Array = pv.unchecked_into();
                    for p in arr.iter() {
                        let pair: js_sys::Array = p.unchecked_into();
                        if pair.length() == 2 {
                            pts.push((
                                pair.get(0).as_f64().unwrap_or_default() as f32,
                                pair.get(1).as_f64().unwrap_or_default() as f32,
                            ));
                        }
                    }
                }
            }
            SelectionKind::Lasso(pts)
        }
        "mask" => SelectionKind::Mask {
            data: Rc::new(mask?),
            width: jnum(v, "w").unwrap_or(0.0) as usize,
            height: jnum(v, "h").unwrap_or(0.0) as usize,
        },
        _ => return None,
    };
    Some(Selection { kind, feather })
}
