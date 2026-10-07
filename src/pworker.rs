//! Client for the pixel worker (public/pixel-worker/run.js): hi-res renders
//! and destructive pixel ops run off the main thread. Callers provide a sync
//! fallback for when the worker is unavailable or errors.

use js_sys::{Array, Object, Reflect, Uint8Array};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{ErrorEvent, MessageEvent, Worker, WorkerOptions, WorkerType};

use pes_pixel::recipe::selection_json;
use pes_pixel::types::{Selection, SelectionKind};

type Reply = Result<JsValue, JsValue>;

thread_local! {
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
    static PENDING: RefCell<HashMap<u32, Box<dyn FnOnce(Reply)>>> = RefCell::new(HashMap::new());
    static NEXT_REQ: Cell<u32> = const { Cell::new(1) };
    /// Worker failed to load or errored — ops use the caller's sync fallback.
    static DEAD: Cell<bool> = const { Cell::new(false) };
    /// Serial queue for destructive ops; each closure captures state at
    /// execution time, so a queued stroke sees the pixels of the prior one.
    static OP_QUEUE: RefCell<VecDeque<Box<dyn FnOnce()>>> = RefCell::new(VecDeque::new());
    static OP_BUSY: Cell<bool> = const { Cell::new(false) };
}

pub fn init() {
    let started = WORKER.with(|w| w.borrow().is_some()) || DEAD.with(|d| d.get());
    if started {
        return;
    }
    let Some(win) = web_sys::window() else { return };
    let Ok(href) = win.location().href() else { return };
    let Some(i) = href.rfind('/') else { return };
    let url = format!("{}pixel-worker/run.js", &href[..=i]);
    let opts = WorkerOptions::new();
    opts.set_type(WorkerType::Module);
    let worker = match Worker::new_with_options(&url, &opts) {
        Ok(w) => w,
        Err(_) => {
            DEAD.with(|d| d.set(true));
            return;
        }
    };
    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(|e: MessageEvent| {
        let data = e.data();
        let Some(id) = Reflect::get(&data, &JsValue::from_str("id"))
            .ok()
            .and_then(|v| v.as_f64())
        else {
            return;
        };
        let ok = Reflect::get(&data, &JsValue::from_str("ok"))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let key = if ok { "result" } else { "error" };
        let val = Reflect::get(&data, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED);
        let cb = PENDING.with(|p| p.borrow_mut().remove(&(id as u32)));
        if let Some(cb) = cb {
            cb(if ok { Ok(val) } else { Err(val) });
        }
    });
    worker.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    let onerr = Closure::<dyn FnMut(ErrorEvent)>::new(|_| {
        DEAD.with(|d| d.set(true));
        WORKER.with(|w| *w.borrow_mut() = None);
        let cbs: Vec<_> = PENDING.with(|p| p.borrow_mut().drain().map(|(_, c)| c).collect());
        for cb in cbs {
            cb(Err(JsValue::from_str("worker error")));
        }
    });
    worker.set_onerror(Some(onerr.as_ref().unchecked_ref()));
    onerr.forget();
    WORKER.with(|w| *w.borrow_mut() = Some(worker));
}

fn call(op: &str, args: &[JsValue], transfer: &[JsValue], cb: Box<dyn FnOnce(Reply)>) -> bool {
    let worker = WORKER.with(|w| w.borrow().clone());
    let Some(worker) = worker else { return false };
    let id = NEXT_REQ.with(|n| {
        let i = n.get();
        n.set(i + 1);
        i
    });
    PENDING.with(|p| p.borrow_mut().insert(id, cb));
    let msg = Object::new();
    let _ = Reflect::set(&msg, &JsValue::from_str("id"), &JsValue::from(id));
    let _ = Reflect::set(&msg, &JsValue::from_str("op"), &JsValue::from_str(op));
    let arr = Array::new();
    for a in args {
        arr.push(a);
    }
    let _ = Reflect::set(&msg, &JsValue::from_str("args"), &arr);
    let t = Array::new();
    for x in transfer {
        t.push(x);
    }
    if worker.post_message_with_transfer(&msg, &t).is_err() {
        PENDING.with(|p| p.borrow_mut().remove(&id));
        return false;
    }
    true
}

fn get_buf(v: &JsValue, key: &str) -> Option<Uint8Array> {
    let b = Reflect::get(v, &JsValue::from_str(key)).ok()?;
    if b.is_undefined() || b.is_null() {
        return None;
    }
    Some(Uint8Array::new(&b))
}

fn get_u32(v: &JsValue, key: &str) -> Option<u32> {
    Reflect::get(v, &JsValue::from_str(key))
        .ok()?
        .as_f64()
        .map(|n| n as u32)
}

/// Ship an item's full-res buffer to the worker (transfer, no copy on send).
pub fn load(id: usize, pixels: &[u8], w: usize, h: usize) {
    let arr = Uint8Array::from(pixels);
    let buf = arr.buffer();
    call(
        "load",
        &[
            JsValue::from(id as u32),
            arr.into(),
            JsValue::from(w as u32),
            JsValue::from(h as u32),
        ],
        &[buf.into()],
        Box::new(|_| {}),
    );
}

pub fn unload(id: usize) {
    call("unload", &[JsValue::from(id as u32)], &[], Box::new(|_| {}));
}

/// Selection wire args: (sel_json, sel_mask) + transfer list for mask bytes.
fn sel_args(sel: Option<&Selection>) -> (Vec<JsValue>, Vec<JsValue>) {
    match sel {
        None => (vec![JsValue::UNDEFINED, JsValue::UNDEFINED], vec![]),
        Some(s) => {
            let json = JsValue::from_str(&selection_json(s));
            match &s.kind {
                SelectionKind::Mask { data, .. } => {
                    let arr = Uint8Array::from(&data[..]);
                    let buf = arr.buffer();
                    (vec![json, arr.into()], vec![buf.into()])
                }
                _ => (vec![json, JsValue::UNDEFINED], vec![]),
            }
        }
    }
}

fn pts_js(pts: &[(f32, f32)]) -> JsValue {
    let arr = Array::new();
    for p in pts {
        let pair = Array::new();
        pair.push(&JsValue::from(p.0));
        pair.push(&JsValue::from(p.1));
        arr.push(&pair);
    }
    arr.into()
}

pub struct RenderResult {
    pub pixels: Uint8Array,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone)]
pub struct DestructiveResult {
    pub pixels: Vec<u8>,
    pub w: usize,
    pub h: usize,
    pub preview: Vec<u8>,
    pub pw: usize,
    pub ph: usize,
}

pub struct ExtractResult {
    pub layer: Vec<u8>,
    pub lw: usize,
    pub lh: usize,
    pub cut: Option<DestructiveResult>,
}

fn parse_destructive(v: &JsValue) -> Option<DestructiveResult> {
    Some(DestructiveResult {
        pixels: get_buf(v, "pixels")?.to_vec(),
        w: get_u32(v, "w")? as usize,
        h: get_u32(v, "h")? as usize,
        preview: get_buf(v, "preview")?.to_vec(),
        pw: get_u32(v, "pw")? as usize,
        ph: get_u32(v, "ph")? as usize,
    })
}

/// Hi-res render. Ok(result) carries the rendered pixels; Err means the
/// worker failed and the caller should run its sync path.
pub fn render(
    id: usize,
    recipe: &str,
    req_w: f64,
    req_h: f64,
    max_edge: u32,
    sel: Option<&Selection>,
    cb: impl FnOnce(Result<RenderResult, ()>) + 'static,
) -> bool {
    let (sargs, transfer) = sel_args(sel);
    let mut args = vec![
        JsValue::from(id as u32),
        JsValue::from_str(recipe),
        JsValue::from(req_w),
        JsValue::from(req_h),
        JsValue::from(max_edge),
    ];
    args.extend(sargs);
    call(
        "render",
        &args,
        &transfer,
        Box::new(move |reply| {
            cb(match reply {
                Ok(v) => match (get_buf(&v, "pixels"), get_u32(&v, "w"), get_u32(&v, "h")) {
                    (Some(pixels), Some(w), Some(h)) => Ok(RenderResult { pixels, w, h }),
                    _ => Err(()),
                },
                Err(_) => Err(()),
            });
        }),
    )
}

/// Destructive-op plumbing: Ok(None) = op declined (e.g. empty heal stroke);
/// Err = worker failure → sync fallback. The serial queue drains after cb.
fn destructive_call(
    op: &str,
    args: Vec<JsValue>,
    transfer: Vec<JsValue>,
    cb: impl FnOnce(Result<Option<DestructiveResult>, ()>) + 'static,
) -> bool {
    call(
        op,
        &args,
        &transfer,
        Box::new(move |reply| {
            let out = match reply {
                Ok(v) if v.is_null() || v.is_undefined() => Ok(None),
                Ok(v) => parse_destructive(&v).map(Some).ok_or(()),
                Err(_) => Err(()),
            };
            cb(out);
            op_done();
        }),
    )
}

pub fn heal(
    id: usize,
    recipe: &str,
    pts: &[(f32, f32)],
    radius_frac: f32,
    mode: u8,
    cb: impl FnOnce(Result<Option<DestructiveResult>, ()>) + 'static,
) -> bool {
    destructive_call(
        "heal_stroke",
        vec![
            JsValue::from(id as u32),
            JsValue::from_str(recipe),
            pts_js(pts),
            JsValue::from(radius_frac),
            JsValue::from(mode as u32),
        ],
        vec![],
        cb,
    )
}

pub fn clone_stamp(
    id: usize,
    recipe: &str,
    pts: &[(f32, f32)],
    radius_frac: f32,
    dx: i32,
    dy: i32,
    cb: impl FnOnce(Result<Option<DestructiveResult>, ()>) + 'static,
) -> bool {
    destructive_call(
        "clone_stamp",
        vec![
            JsValue::from(id as u32),
            JsValue::from_str(recipe),
            pts_js(pts),
            JsValue::from(radius_frac),
            JsValue::from(dx),
            JsValue::from(dy),
        ],
        vec![],
        cb,
    )
}

pub fn cut(
    id: usize,
    recipe: &str,
    sel: &Selection,
    cb: impl FnOnce(Result<Option<DestructiveResult>, ()>) + 'static,
) -> bool {
    let (sargs, transfer) = sel_args(Some(sel));
    let mut args = vec![JsValue::from(id as u32), JsValue::from_str(recipe)];
    args.extend(sargs);
    destructive_call("cut", args, transfer, cb)
}

pub fn fill(
    id: usize,
    recipe: &str,
    sel: &Selection,
    cb: impl FnOnce(Result<Option<DestructiveResult>, ()>) + 'static,
) -> bool {
    let (sargs, transfer) = sel_args(Some(sel));
    let mut args = vec![JsValue::from(id as u32), JsValue::from_str(recipe)];
    args.extend(sargs);
    destructive_call("fill_selection", args, transfer, cb)
}

pub fn extract(
    id: usize,
    recipe: &str,
    sel: &Selection,
    also_cut: bool,
    cb: impl FnOnce(Result<Option<ExtractResult>, ()>) + 'static,
) -> bool {
    let (sargs, transfer) = sel_args(Some(sel));
    let mut args = vec![JsValue::from(id as u32), JsValue::from_str(recipe)];
    args.extend(sargs);
    args.push(JsValue::from(also_cut));
    call(
        "extract",
        &args,
        &transfer,
        Box::new(move |reply| {
            let out = match reply {
                Ok(v) if v.is_null() || v.is_undefined() => Ok(None),
                Ok(v) => {
                    let parsed = match (
                        get_buf(&v, "layer"),
                        get_u32(&v, "lw"),
                        get_u32(&v, "lh"),
                    ) {
                        (Some(layer), Some(lw), Some(lh)) => Some(ExtractResult {
                            layer: layer.to_vec(),
                            lw: lw as usize,
                            lh: lh as usize,
                            cut: parse_destructive(&v),
                        }),
                        _ => None,
                    };
                    parsed.map(Some).ok_or(())
                }
                Err(_) => Err(()),
            };
            cb(out);
            op_done();
        }),
    )
}

/// Queue a destructive op. `op` calls one of the destructive fns above and
/// returns whether the worker accepted it; when false, `sync` runs on the
/// main thread instead. Queued closures execute one at a time, in order.
pub fn destructive(op: impl FnOnce() -> bool + 'static, sync: impl FnOnce() + 'static) {
    OP_QUEUE.with(|q| {
        q.borrow_mut().push_back(Box::new(move || {
            if !op() {
                sync();
                op_done();
            }
        }))
    });
    pump();
}

fn pump() {
    if OP_BUSY.with(|b| b.get()) {
        return;
    }
    let next = OP_QUEUE.with(|q| q.borrow_mut().pop_front());
    if let Some(f) = next {
        OP_BUSY.with(|b| b.set(true));
        f();
    }
}

fn op_done() {
    OP_BUSY.with(|b| b.set(false));
    pump();
}
