//! Session persistence in IndexedDB: the working set (items, edit params,
//! layers, source bytes) is mirrored so any reload — including the service
//! worker's update reload — restores the session exactly. This module owns
//! the DB plumbing and the layer/recipe (de)serialization; main.rs owns
//! when to persist and how to rebuild items.

use idb::{Database, DatabaseEvent, Factory, TransactionMode};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::state::{
    BrushLayer, BrushShape, Layer, LayerKind, PathLayer, PathPoint, RasterLayer, TextAlign,
    TextLayer,
};

const DB_NAME: &str = "pes-session";
const DB_VERSION: u32 = 1;
const ITEMS: &str = "items";
const META: &str = "meta";
/// Raster layer pixel PNGs, keyed "itemId:layerId" — separate from item
/// records so an edit-params write doesn't rewrite multi-MB pixel blobs.
const LAYER_PIXELS: &str = "layer_pixels";

fn err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&format!("session: {e}"))
}

pub async fn open_db() -> Result<Database, JsValue> {
    let factory = Factory::new().map_err(err)?;
    let mut req = factory.open(DB_NAME, Some(DB_VERSION)).map_err(err)?;
    req.on_upgrade_needed(|e| {
        let db = e.database().expect("upgrade db");
        if !db.store_names().iter().any(|n| n == ITEMS) {
            db.create_object_store(ITEMS, Default::default())
                .expect("create items store");
        }
        if !db.store_names().iter().any(|n| n == META) {
            db.create_object_store(META, Default::default())
                .expect("create meta store");
        }
        if !db.store_names().iter().any(|n| n == LAYER_PIXELS) {
            db.create_object_store(LAYER_PIXELS, Default::default())
                .expect("create layer_pixels store");
        }
    });
    req.await.map_err(err)
}

/// Write one item record; `key` is the item id, `value` a JS object
/// (Blob/ArrayBuffer members stored natively).
pub async fn put_item(db: &Database, id: usize, value: &JsValue) -> Result<(), JsValue> {
    let tx = db.transaction(&[ITEMS], TransactionMode::ReadWrite).map_err(err)?;
    let store = tx.object_store(ITEMS).map_err(err)?;
    store
        .put(value, Some(&JsValue::from_f64(id as f64)))
        .map_err(err)?
        .await
        .map_err(err)?;
    tx.commit().map_err(err)?.await.map_err(err)?;
    Ok(())
}

pub async fn delete_items(db: &Database, ids: &[usize]) -> Result<(), JsValue> {
    if ids.is_empty() {
        return Ok(());
    }
    let tx = db.transaction(&[ITEMS], TransactionMode::ReadWrite).map_err(err)?;
    let store = tx.object_store(ITEMS).map_err(err)?;
    for id in ids {
        store
            .delete(JsValue::from_f64(*id as f64))
            .map_err(err)?
            .await
            .map_err(err)?;
    }
    tx.commit().map_err(err)?.await.map_err(err)?;
    Ok(())
}

pub async fn get_item(db: &Database, id: usize) -> Result<Option<JsValue>, JsValue> {
    let tx = db.transaction(&[ITEMS], TransactionMode::ReadOnly).map_err(err)?;
    let store = tx.object_store(ITEMS).map_err(err)?;
    store
        .get(JsValue::from_f64(id as f64))
        .map_err(err)?
        .await
        .map_err(err)
}

/// All item records as (id, value) pairs.
pub async fn all_items(db: &Database) -> Result<Vec<(usize, JsValue)>, JsValue> {
    let tx = db.transaction(&[ITEMS], TransactionMode::ReadOnly).map_err(err)?;
    let store = tx.object_store(ITEMS).map_err(err)?;
    let values = store.get_all(None, None).map_err(err)?.await.map_err(err)?;
    let keys = store.get_all_keys(None, None).map_err(err)?.await.map_err(err)?;
    let mut out = Vec::new();
    for (k, v) in keys.into_iter().zip(values.into_iter()) {
        let Some(id) = k.as_f64() else { continue };
        out.push((id as usize, v));
    }
    Ok(out)
}

pub async fn put_meta(db: &Database, value: &JsValue) -> Result<(), JsValue> {
    let tx = db.transaction(&[META], TransactionMode::ReadWrite).map_err(err)?;
    let store = tx.object_store(META).map_err(err)?;
    store
        .put(value, Some(&JsValue::from_str("session")))
        .map_err(err)?
        .await
        .map_err(err)?;
    tx.commit().map_err(err)?.await.map_err(err)?;
    Ok(())
}

pub async fn get_meta(db: &Database) -> Result<Option<JsValue>, JsValue> {
    let tx = db.transaction(&[META], TransactionMode::ReadOnly).map_err(err)?;
    let store = tx.object_store(META).map_err(err)?;
    store
        .get(JsValue::from_str("session"))
        .map_err(err)?
        .await
        .map_err(err)
}

fn lp_key(item_id: usize, layer_id: usize) -> JsValue {
    JsValue::from_str(&format!("{item_id}:{layer_id}"))
}

pub async fn put_layer_pixels(
    db: &Database,
    item_id: usize,
    layer_id: usize,
    blob: &JsValue,
) -> Result<(), JsValue> {
    let tx = db.transaction(&[LAYER_PIXELS], TransactionMode::ReadWrite).map_err(err)?;
    let store = tx.object_store(LAYER_PIXELS).map_err(err)?;
    store
        .put(blob, Some(&lp_key(item_id, layer_id)))
        .map_err(err)?
        .await
        .map_err(err)?;
    tx.commit().map_err(err)?.await.map_err(err)?;
    Ok(())
}

pub async fn get_layer_pixels(
    db: &Database,
    item_id: usize,
    layer_id: usize,
) -> Result<Option<JsValue>, JsValue> {
    let tx = db.transaction(&[LAYER_PIXELS], TransactionMode::ReadOnly).map_err(err)?;
    let store = tx.object_store(LAYER_PIXELS).map_err(err)?;
    store.get(lp_key(item_id, layer_id)).map_err(err)?.await.map_err(err)
}

pub async fn delete_layer_pixel(
    db: &Database,
    item_id: usize,
    layer_id: usize,
) -> Result<(), JsValue> {
    let tx = db.transaction(&[LAYER_PIXELS], TransactionMode::ReadWrite).map_err(err)?;
    let store = tx.object_store(LAYER_PIXELS).map_err(err)?;
    store.delete(lp_key(item_id, layer_id)).map_err(err)?.await.map_err(err)?;
    tx.commit().map_err(err)?.await.map_err(err)?;
    Ok(())
}

/// Delete every record belonging to an item (its layer pixel PNGs).
pub async fn delete_layer_pixels(db: &Database, item_id: usize) -> Result<(), JsValue> {
    let tx = db.transaction(&[LAYER_PIXELS], TransactionMode::ReadWrite).map_err(err)?;
    let store = tx.object_store(LAYER_PIXELS).map_err(err)?;
    let keys = store.get_all_keys(None, None).map_err(err)?.await.map_err(err)?;
    let prefix = format!("{item_id}:");
    for k in keys {
        if k.as_string().is_some_and(|s| s.starts_with(&prefix)) {
            store.delete(k).map_err(err)?.await.map_err(err)?;
        }
    }
    tx.commit().map_err(err)?.await.map_err(err)?;
    Ok(())
}

// --- JS object helpers --------------------------------------------------------

pub fn jobj() -> js_sys::Object {
    js_sys::Object::new()
}

pub fn jset(obj: &JsValue, key: &str, v: impl Into<JsValue>) {
    let _ = js_sys::Reflect::set(obj, &JsValue::from_str(key), &v.into());
}

pub fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn num(v: f32) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// --- Layer serialization ------------------------------------------------------

fn align_wire(a: TextAlign) -> &'static str {
    match a {
        TextAlign::Left => "left",
        TextAlign::Center => "center",
        TextAlign::Right => "right",
    }
}

fn align_from(s: &str) -> TextAlign {
    match s {
        "left" => TextAlign::Left,
        "right" => TextAlign::Right,
        _ => TextAlign::Center,
    }
}

fn shape_wire(s: BrushShape) -> &'static str {
    match s {
        BrushShape::Smooth => "smooth",
        BrushShape::Square => "square",
        BrushShape::Dot => "dot",
        BrushShape::Triangle => "triangle",
        BrushShape::Custom => "custom",
    }
}

fn shape_from(s: &str) -> BrushShape {
    match s {
        "square" => BrushShape::Square,
        "dot" => BrushShape::Dot,
        "triangle" => BrushShape::Triangle,
        "custom" => BrushShape::Custom,
        _ => BrushShape::Smooth,
    }
}

/// Serialize layers to JSON. Raster layers carry geometry only — their
/// pixels are stored as PNG blobs in the record's layer_pixels map (pixels
/// are immutable after layer creation, so one blob per layer id).
/// Undo histories are not persisted. Returns None for an empty stack.
pub fn layers_json(layers: &[Layer]) -> Option<String> {
    if layers.is_empty() {
        return None;
    }
    let mut out = String::from("[");
    for (i, l) in layers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let head = format!(
            "{{\"id\":{},\"vis\":{},\"op\":{}",
            l.id,
            l.visible,
            num(l.opacity)
        );
        out.push_str(&head);
        match &l.kind {
            LayerKind::Text(t) => {
                out.push_str(&format!(
                    ",\"t\":\"text\",\"text\":\"{}\",\"x\":{},\"y\":{},\"fs\":{},\"ang\":{},\
                     \"ff\":\"{}\",\"fw\":{},\"c\":\"{}\",\"sc\":\"{}\",\"sw\":{},\
                     \"shc\":\"{}\",\"shb\":{},\"shx\":{},\"shy\":{},\"al\":\"{}\"",
                    esc(&t.text),
                    num(t.x),
                    num(t.y),
                    num(t.font_size),
                    num(t.angle),
                    esc(&t.font_family),
                    t.font_weight,
                    esc(&t.color),
                    esc(&t.stroke_color),
                    num(t.stroke_width),
                    esc(&t.shadow_color),
                    num(t.shadow_blur),
                    num(t.shadow_offset_x),
                    num(t.shadow_offset_y),
                    align_wire(t.alignment),
                ));
            }
            LayerKind::Path(p) => {
                let pts: Vec<String> = p
                    .points
                    .iter()
                    .map(|pt| {
                        format!(
                            "[{},{},{},{},{},{},{}]",
                            num(pt.x),
                            num(pt.y),
                            num(pt.in_x),
                            num(pt.in_y),
                            num(pt.out_x),
                            num(pt.out_y),
                            pt.smooth
                        )
                    })
                    .collect();
                out.push_str(&format!(
                    ",\"t\":\"path\",\"pts\":[{}],\"closed\":{},\"fc\":\"{}\",\"sc\":\"{}\",\"sw\":{}",
                    pts.join(","),
                    p.closed,
                    esc(&p.fill_color),
                    esc(&p.stroke_color),
                    num(p.stroke_width),
                ));
            }
            LayerKind::Brush(b) => {
                let strokes: Vec<String> = b
                    .strokes
                    .iter()
                    .map(|s| {
                        let pts: Vec<String> = s
                            .points
                            .iter()
                            .map(|(x, y)| format!("[{},{}]", num(*x), num(*y)))
                            .collect();
                        format!("[{}]", pts.join(","))
                    })
                    .collect();
                out.push_str(&format!(
                    ",\"t\":\"brush\",\"strokes\":[{}],\"c\":\"{}\",\"w\":{},\"shape\":\"{}\",\"sp\":{},\"cp\":\"{}\"",
                    strokes.join(","),
                    esc(&b.color),
                    num(b.width),
                    shape_wire(b.shape),
                    num(b.spacing),
                    esc(&b.custom_path),
                ));
            }
            LayerKind::Raster(r) => {
                out.push_str(&format!(
                    ",\"t\":\"raster\",\"w\":{},\"h\":{},\"x\":{},\"y\":{},\"scale\":{}",
                    r.width,
                    r.height,
                    num(r.x),
                    num(r.y),
                    num(r.scale),
                ));
            }
        }
        out.push('}');
    }
    out.push(']');
    Some(out)
}

fn jstr(v: &JsValue, key: &str) -> Option<String> {
    crate::state::jget(v, key).and_then(|s| s.as_string())
}

/// Parse layers JSON. Raster layers come back with empty pixel buffers and
/// correct dimensions — the caller fills `pixels` after decoding the blob
/// from layer_pixels. Malformed layers are skipped.
pub fn layers_from_json(json: &str) -> Vec<Layer> {
    let mut out = Vec::new();
    let Ok(v) = js_sys::JSON::parse(json) else { return out };
    if !js_sys::Array::is_array(&v) {
        return out;
    }
    let arr: js_sys::Array = v.unchecked_into();
    for lv in arr.iter() {
        let Some(t) = jstr(&lv, "t") else { continue };
        let id = crate::state::jnum(&lv, "id").unwrap_or(0.0) as usize;
        let visible = crate::state::jbool(&lv, "vis").unwrap_or(true);
        let opacity = crate::state::jnum(&lv, "op").unwrap_or(1.0) as f32;
        let kind = match t.as_str() {
            "text" => LayerKind::Text(TextLayer {
                text: jstr(&lv, "text").unwrap_or_else(|| "Text".into()),
                x: crate::state::jnum(&lv, "x").unwrap_or(0.5) as f32,
                y: crate::state::jnum(&lv, "y").unwrap_or(0.5) as f32,
                font_size: crate::state::jnum(&lv, "fs").unwrap_or(0.08) as f32,
                angle: crate::state::jnum(&lv, "ang").unwrap_or(0.0) as f32,
                font_family: jstr(&lv, "ff").unwrap_or_else(|| "Inter".into()),
                font_weight: crate::state::jnum(&lv, "fw").unwrap_or(700.0) as u16,
                color: jstr(&lv, "c").unwrap_or_else(|| "#ffffff".into()),
                stroke_color: jstr(&lv, "sc").unwrap_or_else(|| "#000000".into()),
                stroke_width: crate::state::jnum(&lv, "sw").unwrap_or(0.0) as f32,
                shadow_color: jstr(&lv, "shc").unwrap_or_else(|| "rgba(0,0,0,0.5)".into()),
                shadow_blur: crate::state::jnum(&lv, "shb").unwrap_or(4.0) as f32,
                shadow_offset_x: crate::state::jnum(&lv, "shx").unwrap_or(2.0) as f32,
                shadow_offset_y: crate::state::jnum(&lv, "shy").unwrap_or(2.0) as f32,
                alignment: align_from(&jstr(&lv, "al").unwrap_or_default()),
            }),
            "path" => {
                let mut points = Vec::new();
                if let Some(pts_v) = crate::state::jget(&lv, "pts") {
                    if js_sys::Array::is_array(&pts_v) {
                        let pts: js_sys::Array = pts_v.unchecked_into();
                        for pv in pts.iter() {
                            if !js_sys::Array::is_array(&pv) {
                                continue;
                            }
                            let pa: js_sys::Array = pv.unchecked_into();
                            if pa.length() < 7 {
                                continue;
                            }
                            points.push(PathPoint {
                                x: pa.get(0).as_f64().unwrap_or(0.0) as f32,
                                y: pa.get(1).as_f64().unwrap_or(0.0) as f32,
                                in_x: pa.get(2).as_f64().unwrap_or(0.0) as f32,
                                in_y: pa.get(3).as_f64().unwrap_or(0.0) as f32,
                                out_x: pa.get(4).as_f64().unwrap_or(0.0) as f32,
                                out_y: pa.get(5).as_f64().unwrap_or(0.0) as f32,
                                smooth: pa.get(6).as_bool().unwrap_or(true),
                            });
                        }
                    }
                }
                LayerKind::Path(PathLayer {
                    points,
                    closed: crate::state::jbool(&lv, "closed").unwrap_or(false),
                    fill_color: jstr(&lv, "fc").unwrap_or_else(|| "#ff3b30".into()),
                    stroke_color: jstr(&lv, "sc").unwrap_or_else(|| "#000000".into()),
                    stroke_width: crate::state::jnum(&lv, "sw").unwrap_or(0.01) as f32,
                    history: Vec::new(),
                })
            }
            "brush" => {
                let mut strokes = Vec::new();
                if let Some(st_v) = crate::state::jget(&lv, "strokes") {
                    if js_sys::Array::is_array(&st_v) {
                        let st: js_sys::Array = st_v.unchecked_into();
                        for sv in st.iter() {
                            if !js_sys::Array::is_array(&sv) {
                                continue;
                            }
                            let sa: js_sys::Array = sv.unchecked_into();
                            let mut points = Vec::new();
                            for pv in sa.iter() {
                                if !js_sys::Array::is_array(&pv) {
                                    continue;
                                }
                                let pa: js_sys::Array = pv.unchecked_into();
                                if pa.length() < 2 {
                                    continue;
                                }
                                points.push((
                                    pa.get(0).as_f64().unwrap_or(0.0) as f32,
                                    pa.get(1).as_f64().unwrap_or(0.0) as f32,
                                ));
                            }
                            strokes.push(crate::state::BrushStroke { points });
                        }
                    }
                }
                LayerKind::Brush(BrushLayer {
                    strokes,
                    color: jstr(&lv, "c").unwrap_or_else(|| "#0a84ff".into()),
                    width: crate::state::jnum(&lv, "w").unwrap_or(0.02) as f32,
                    shape: shape_from(&jstr(&lv, "shape").unwrap_or_default()),
                    spacing: crate::state::jnum(&lv, "sp").unwrap_or(1.0) as f32,
                    custom_path: jstr(&lv, "cp").unwrap_or_default(),
                    history: Vec::new(),
                })
            }
            "raster" => LayerKind::Raster(RasterLayer {
                pixels: std::rc::Rc::new(Vec::new()),
                width: crate::state::jnum(&lv, "w").unwrap_or(0.0) as usize,
                height: crate::state::jnum(&lv, "h").unwrap_or(0.0) as usize,
                x: crate::state::jnum(&lv, "x").unwrap_or(0.5) as f32,
                y: crate::state::jnum(&lv, "y").unwrap_or(0.5) as f32,
                scale: crate::state::jnum(&lv, "scale").unwrap_or(1.0) as f32,
            }),
            _ => continue,
        };
        out.push(Layer { id, visible, opacity, kind });
    }
    out
}
