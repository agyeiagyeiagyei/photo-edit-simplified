//! Edit-param and selection wire formats (JSON), shared by the Drive
//! manifest, the session store, and the pixel-worker protocol.

use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

use crate::types::{Aspect, CropRect, EditParams, Selection, SelectionKind};

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

/// Serialize edit params to a recipe JSON string for the Drive manifest's
/// edits map. Identity fields are omitted to keep the manifest small; returns
/// None when every param is at its default (no entry needed). Selection masks
/// and layers are out of scope for recipes (pixel/stroke data).
pub fn edit_recipe_json(e: &EditParams) -> Option<String> {
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
        let chans: Vec<String> = e
            .levels
            .ranges
            .iter()
            .map(|r| {
                format!(
                    "[{},{},{},{},{}]",
                    r.black, r.gamma, r.white, r.output_black, r.output_white
                )
            })
            .collect();
        parts.push(format!("\"levels\":[{}]", chans.join(",")));
    }
    if !e.curves.is_identity() {
        let chans: Vec<String> = e
            .curves
            .channels
            .iter()
            .map(|pts| {
                let ps: Vec<String> =
                    pts.iter().map(|p| format!("[{},{}]", p.x, p.y)).collect();
                format!("[{}]", ps.join(","))
            })
            .collect();
        parts.push(format!("\"curves\":[{}]", chans.join(",")));
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
    // edit_recipe_json omits identity fields, which parse back as defaults —
    // semantically identical, so reuse it and just handle the all-default case.
    edit_recipe_json(&e).unwrap_or_else(|| "{\"v\":1}".into())
}

/// Parse a recipe JSON string back into EditParams. Unknown/missing fields
/// fall back to defaults; malformed input returns None (caller treats it as
/// "no recipe"). Selection is always None — masks aren't serialized.
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
        if js_sys::Array::is_array(&l) {
            let arr: js_sys::Array = l.unchecked_into();
            if arr.length() == 4 {
                let mut ranges = [crate::levels::LevelRange::default(); 4];
                let mut ok = true;
                for (i, chan) in arr.iter().enumerate() {
                    let row: js_sys::Array = chan.unchecked_into();
                    if row.length() != 5 {
                        ok = false;
                        break;
                    }
                    let n = |j: u32| row.get(j).as_f64().unwrap_or_default();
                    ranges[i] = crate::levels::LevelRange {
                        black: n(0),
                        gamma: n(1),
                        white: n(2),
                        output_black: n(3),
                        output_white: n(4),
                    };
                }
                if ok {
                    e.levels = crate::levels::LevelsSettings { ranges };
                }
            }
        }
    }
    if let Some(c) = jget(&v, "curves") {
        if js_sys::Array::is_array(&c) {
        let arr: js_sys::Array = c.unchecked_into();
        if arr.length() == 4 {
            let mut channels: [Vec<crate::curves::CurvePoint>; 4] =
                [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
            let mut ok = true;
            for (i, chan) in arr.iter().enumerate() {
                let pts: js_sys::Array = chan.unchecked_into();
                for p in pts.iter() {
                    let pair: js_sys::Array = p.unchecked_into();
                    if pair.length() != 2 {
                        ok = false;
                        break;
                    }
                    channels[i].push(crate::curves::CurvePoint {
                        x: pair.get(0).as_f64().unwrap_or_default(),
                        y: pair.get(1).as_f64().unwrap_or_default(),
                    });
                }
                if !ok {
                    break;
                }
            }
            let parsed = crate::curves::CurvesSettings { channels };
            if ok && parsed.is_valid() {
                e.curves = parsed;
            }
        }
        }
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
    let feather = jnum(&v, "f").unwrap_or(0.0) as f32;
    let kind = match jget(&v, "t").and_then(|t| t.as_string())?.as_str() {
        "rect" => SelectionKind::Rect {
            x: jnum(&v, "x").unwrap_or(0.0) as f32,
            y: jnum(&v, "y").unwrap_or(0.0) as f32,
            w: jnum(&v, "w").unwrap_or(1.0) as f32,
            h: jnum(&v, "h").unwrap_or(1.0) as f32,
        },
        "lasso" => {
            let mut pts = Vec::new();
            if let Some(pv) = jget(&v, "pts") {
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
            data: std::rc::Rc::new(mask?),
            width: jnum(&v, "w").unwrap_or(0.0) as usize,
            height: jnum(&v, "h").unwrap_or(0.0) as usize,
        },
        _ => return None,
    };
    Some(Selection { kind, feather })
}
