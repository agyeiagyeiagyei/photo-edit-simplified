//! Client for the pixel worker (the fingerprinted
//! public/pixel-worker/run.<hash>.js): hi-res renders and destructive pixel
//! ops run off the main thread. Callers provide a sync fallback for when the
//! worker is unavailable or errors.

use js_sys::{Array, Object, Reflect, Uint8Array};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{ErrorEvent, MessageEvent, Worker, WorkerOptions, WorkerType};

use pes_pixel::recipe::selection_json;
use pes_pixel::types::{Selection, SelectionKind};

use crate::perf;

type Reply = Result<JsValue, JsValue>;

thread_local! {
    static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
    static PENDING: RefCell<HashMap<u32, Box<dyn FnOnce(Reply)>>> = RefCell::new(HashMap::new());
    static PENDING_OP: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
    static NEXT_REQ: Cell<u32> = const { Cell::new(1) };
    /// Worker failed to load or errored — ops use the caller's sync fallback.
    static DEAD: Cell<bool> = const { Cell::new(false) };
    /// Serial queue for destructive ops; each closure captures state at
    /// execution time, so a queued stroke sees the pixels of the prior one.
    static OP_QUEUE: RefCell<VecDeque<Box<dyn FnOnce()>>> = RefCell::new(VecDeque::new());
    static OP_BUSY: Cell<bool> = const { Cell::new(false) };
    /// Second worker instance for RAW/camera proxy develops — a multi-second
    /// demosaic there must not block interactive renders on the main worker.
    static RAW_WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
    static RAW_PENDING: RefCell<HashMap<u32, Box<dyn FnOnce(Reply)>>> = RefCell::new(HashMap::new());
    static RAW_PENDING_OP: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
    static RAW_NEXT_REQ: Cell<u32> = const { Cell::new(1) };
    static RAW_DEAD: Cell<bool> = const { Cell::new(false) };
    /// High-water of the RAW worker's wasm linear memory (bytes). Raw replies
    /// carry `mem`, measured after the develop.
    static RAW_MEM_HIGH: Cell<f64> = const { Cell::new(0.0) };
}

/// Bytes of the RAW worker's wasm memory high-water (0 until the first reply).
pub fn raw_mem_high() -> f64 {
    RAW_MEM_HIGH.with(|m| m.get())
}

/// Worker instance index.html created at parse time so its bundle downloads
/// in parallel with the app wasm. Taken once; the RAW worker makes its own.
fn adopt_precreated() -> Option<Worker> {
    let win = web_sys::window()?;
    let v = Reflect::get(win.as_ref(), &JsValue::from_str("__pesWorker")).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    let _ = Reflect::set(win.as_ref(), &JsValue::from_str("__pesWorker"), &JsValue::NULL);
    v.dyn_into::<Worker>().ok()
}

/// Fingerprinted bootstrap path from `<meta name="pes-worker">` (written by
/// the trunk post-build hook), resolved against the app directory.
fn worker_url() -> Option<String> {
    let win = web_sys::window()?;
    let doc = win.document()?;
    let el = doc.query_selector("meta[name=\"pes-worker\"]").ok().flatten()?;
    let src = el.get_attribute("content")?;
    if src.is_empty() {
        return None;
    }
    let href = win.location().href().ok()?;
    let i = href.rfind('/')?;
    Some(format!("{}{}", &href[..=i], src))
}

pub fn init() {
    let started = WORKER.with(|w| w.borrow().is_some()) || DEAD.with(|d| d.get());
    if started {
        return;
    }
    let worker = match adopt_precreated().or_else(|| {
        let url = worker_url()?;
        let opts = WorkerOptions::new();
        opts.set_type(WorkerType::Module);
        Worker::new_with_options(&url, &opts).ok()
    }) {
        Some(w) => w,
        None => {
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
        let op = PENDING_OP.with(|m| m.borrow_mut().remove(&(id as u32)));
        let cb = PENDING.with(|p| p.borrow_mut().remove(&(id as u32)));
        if let Some(cb) = cb {
            if let Some(op) = op {
                record_worker_reply(&op, &data, &val);
            }
            cb(if ok { Ok(val) } else { Err(val) });
        }
    });
    worker.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    let onerr = Closure::<dyn FnMut(ErrorEvent)>::new(|_| {
        DEAD.with(|d| d.set(true));
        WORKER.with(|w| *w.borrow_mut() = None);
        PENDING_OP.with(|m| m.borrow_mut().clear());
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
    PENDING_OP.with(|m| m.borrow_mut().insert(id, op.to_string()));
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
        PENDING_OP.with(|m| m.borrow_mut().remove(&id));
        return false;
    }
    true
}

/// Record `worker.<op>` from the reply's `ms` (measured inside run.js), plus
/// the render pipeline's own stage breakdown when present.
fn record_worker_reply(op: &str, data: &JsValue, result: &JsValue) {
    if let Some(ms) = Reflect::get(data, &JsValue::from_str("ms"))
        .ok()
        .and_then(|v| v.as_f64())
    {
        perf::record(&format!("worker.{op}"), ms);
    }
    if op != "render" {
        return;
    }
    let Ok(timings) = Reflect::get(result, &JsValue::from_str("timings")) else { return };
    if !timings.is_object() {
        return;
    }
    for stage in ["geometry", "resample", "develop", "color"] {
        if let Some(ms) = Reflect::get(&timings, &JsValue::from_str(stage))
            .ok()
            .and_then(|v| v.as_f64())
        {
            perf::record(&format!("worker.render.{stage}"), ms);
        }
    }
}

fn raw_init() -> bool {
    let started = RAW_WORKER.with(|w| w.borrow().is_some()) || RAW_DEAD.with(|d| d.get());
    if started {
        return RAW_WORKER.with(|w| w.borrow().is_some());
    }
    let Some(url) = worker_url() else {
        RAW_DEAD.with(|d| d.set(true));
        return false;
    };
    let opts = WorkerOptions::new();
    opts.set_type(WorkerType::Module);
    let worker = match Worker::new_with_options(&url, &opts) {
        Ok(w) => w,
        Err(_) => {
            RAW_DEAD.with(|d| d.set(true));
            return false;
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
        let op = RAW_PENDING_OP.with(|m| m.borrow_mut().remove(&(id as u32)));
        let cb = RAW_PENDING.with(|p| p.borrow_mut().remove(&(id as u32)));
        if let Some(cb) = cb {
            if let Some(op) = op {
                record_worker_reply(&op, &data, &val);
            }
            if let Some(mem) = reply_mem(&val) {
                if mem > RAW_MEM_HIGH.with(|m| m.get()) {
                    RAW_MEM_HIGH.with(|m| m.set(mem));
                }
            }
            cb(if ok { Ok(val) } else { Err(val) });
        }
    });
    worker.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    let onerr = Closure::<dyn FnMut(ErrorEvent)>::new(|_| {
        RAW_DEAD.with(|d| d.set(true));
        RAW_WORKER.with(|w| *w.borrow_mut() = None);
        RAW_PENDING_OP.with(|m| m.borrow_mut().clear());
        let cbs: Vec<_> = RAW_PENDING.with(|p| p.borrow_mut().drain().map(|(_, c)| c).collect());
        for cb in cbs {
            cb(Err(JsValue::from_str("worker error")));
        }
    });
    worker.set_onerror(Some(onerr.as_ref().unchecked_ref()));
    onerr.forget();
    RAW_WORKER.with(|w| *w.borrow_mut() = Some(worker));
    true
}

fn raw_call(op: &str, args: &[JsValue], transfer: &[JsValue], cb: Box<dyn FnOnce(Reply)>) -> bool {
    let worker = RAW_WORKER.with(|w| w.borrow().clone());
    let Some(worker) = worker else { return false };
    let id = RAW_NEXT_REQ.with(|n| {
        let i = n.get();
        n.set(i + 1);
        i
    });
    RAW_PENDING.with(|p| p.borrow_mut().insert(id, cb));
    RAW_PENDING_OP.with(|m| m.borrow_mut().insert(id, op.to_string()));
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
        RAW_PENDING.with(|p| p.borrow_mut().remove(&id));
        RAW_PENDING_OP.with(|m| m.borrow_mut().remove(&id));
        return false;
    }
    true
}

fn reply_mem(v: &JsValue) -> Option<f64> {
    Reflect::get(v, &JsValue::from_str("mem"))
        .ok()
        .and_then(|m| m.as_f64())
}

// --- batch RAW worker pool ---------------------------------------------------
//
// Batch import develops display proxies for many files; through the interactive
// RAW worker every file would queue behind the selected item's hi-res work.
// Pooled workers each run one develop at a time, so at most `raw_pool_size()`
// files are in flight and blobs are not resolved to bytes ahead of their turn
// (run.js resolves a Blob on message receipt).

struct PoolJob {
    op: &'static str,
    args: Vec<JsValue>,
    cb: Box<dyn FnOnce(Reply)>,
}

struct PoolSlot {
    worker: Option<Worker>,
    /// One op in flight per slot; more queued messages would each materialise
    /// their Blob in the worker before the prior develop finishes.
    pending: Option<(u32, String, Box<dyn FnOnce(Reply)>)>,
    dead: bool,
}

thread_local! {
    static RAW_POOL: RefCell<Vec<PoolSlot>> = RefCell::new(Vec::new());
    static POOL_QUEUE: RefCell<VecDeque<PoolJob>> = RefCell::new(VecDeque::new());
    static POOL_NEXT_REQ: Cell<u32> = const { Cell::new(1) };
    static POOL_SIZE: Cell<usize> = const { Cell::new(0) };
    static POOL_MEM_LAST: RefCell<Vec<f64>> = RefCell::new(Vec::new());
    /// Peak of the summed wasm memory of the pool workers at reply time.
    static POOL_MEM_HIGH: Cell<f64> = const { Cell::new(0.0) };
}

pub fn raw_pool_mem_high() -> f64 {
    POOL_MEM_HIGH.with(|m| m.get())
}

/// `clamp(hardwareConcurrency - 2, 1, 3)`, capped at one worker per 2 GB of
/// `navigator.deviceMemory`: a 26 MP superpixel develop measures ~450 MB of
/// pool-worker wasm memory on the reference VM, so the memory cap is the
/// binding constraint, not the core count.
pub fn raw_pool_size() -> usize {
    POOL_SIZE.with(|s| {
        if s.get() != 0 {
            return s.get();
        }
        let (hw, mem) = pool_device();
        let by_hw = (hw - 2.0).clamp(1.0, 3.0);
        let by_mem = (mem / 2.0).floor().clamp(1.0, 3.0);
        let n = (by_hw.min(by_mem) as usize).max(1);
        s.set(n);
        n
    })
}

fn pool_device() -> (f64, f64) {
    let Some(win) = web_sys::window() else { return (4.0, 4.0) };
    let nav = win.navigator();
    let hw = nav.hardware_concurrency();
    let hw = if hw.is_finite() && hw >= 1.0 { hw } else { 4.0 };
    // `deviceMemory` is capped at 8 and missing outside Chromium; assume 4.
    let mem = Reflect::get(nav.as_ref(), &JsValue::from_str("deviceMemory"))
        .ok()
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v >= 1.0)
        .unwrap_or(4.0);
    (hw, mem)
}

fn pool_ensure(n: usize) {
    RAW_POOL.with(|p| {
        let mut p = p.borrow_mut();
        while p.len() < n {
            p.push(PoolSlot { worker: None, pending: None, dead: false });
        }
    });
    POOL_MEM_LAST.with(|m| {
        let mut m = m.borrow_mut();
        while m.len() < n {
            m.push(0.0);
        }
    });
}

fn pool_init_slot(i: usize) -> bool {
    if RAW_POOL.with(|p| p.borrow().get(i).is_some_and(|s| s.worker.is_some())) {
        return true;
    }
    let Some(url) = worker_url() else { return false };
    let opts = WorkerOptions::new();
    opts.set_type(WorkerType::Module);
    let worker = match Worker::new_with_options(&url, &opts) {
        Ok(w) => w,
        Err(_) => {
            RAW_POOL.with(|p| {
                if let Some(s) = p.borrow_mut().get_mut(i) {
                    s.dead = true;
                }
            });
            return false;
        }
    };
    let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
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
        let entry = RAW_POOL.with(|p| {
            let mut p = p.borrow_mut();
            let slot = &mut p[i];
            match slot.pending.take() {
                Some((pid, op, cb)) if pid as f64 == id => Some((op, cb)),
                other => {
                    slot.pending = other;
                    None
                }
            }
        });
        let Some((op, cb)) = entry else { return };
        record_worker_reply(&op, &data, &val);
        record_pool_mem(i, &val);
        cb(if ok { Ok(val) } else { Err(val) });
        pool_pump();
    });
    worker.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
    onmsg.forget();
    let onerr = Closure::<dyn FnMut(ErrorEvent)>::new(move |_| {
        let pending = RAW_POOL.with(|p| {
            let mut p = p.borrow_mut();
            let slot = &mut p[i];
            slot.worker = None;
            slot.pending.take()
        });
        if let Some((_, _, cb)) = pending {
            cb(Err(JsValue::from_str("raw pool worker error")));
        }
        pool_pump();
    });
    worker.set_onerror(Some(onerr.as_ref().unchecked_ref()));
    onerr.forget();
    RAW_POOL.with(|p| {
        if let Some(s) = p.borrow_mut().get_mut(i) {
            s.worker = Some(worker);
            s.dead = false;
        }
    });
    true
}

fn record_pool_mem(i: usize, val: &JsValue) {
    let Some(mem) = reply_mem(val) else { return };
    let sum = POOL_MEM_LAST.with(|m| {
        let mut m = m.borrow_mut();
        if i < m.len() {
            m[i] = mem;
        }
        m.iter().sum::<f64>()
    });
    if sum > POOL_MEM_HIGH.with(|m| m.get()) {
        POOL_MEM_HIGH.with(|m| m.set(sum));
    }
}

enum SlotPick {
    Free(usize),
    Busy,
    Unavailable,
}

fn pool_free_slot() -> SlotPick {
    let n = raw_pool_size();
    pool_ensure(n);
    for i in 0..n {
        let (free, has, dead) = RAW_POOL.with(|p| {
            let p = p.borrow();
            let s = &p[i];
            (s.pending.is_none(), s.worker.is_some(), s.dead)
        });
        if !dead && free && has {
            return SlotPick::Free(i);
        }
    }
    for i in 0..n {
        let (free, has, dead) = RAW_POOL.with(|p| {
            let p = p.borrow();
            let s = &p[i];
            (s.pending.is_none(), s.worker.is_some(), s.dead)
        });
        if !dead && free && !has && pool_init_slot(i) {
            return SlotPick::Free(i);
        }
    }
    if RAW_POOL.with(|p| p.borrow().iter().any(|s| !s.dead)) {
        SlotPick::Busy
    } else {
        SlotPick::Unavailable
    }
}

fn post_pool(i: usize, job: PoolJob) -> bool {
    let worker = RAW_POOL.with(|p| p.borrow()[i].worker.clone());
    let Some(worker) = worker else {
        (job.cb)(Err(JsValue::from_str("raw pool slot has no worker")));
        return false;
    };
    let id = POOL_NEXT_REQ.with(|n| {
        let i = n.get();
        n.set(i + 1);
        i
    });
    RAW_POOL.with(|p| {
        p.borrow_mut()[i].pending = Some((id, job.op.to_string(), job.cb));
    });
    let msg = Object::new();
    let _ = Reflect::set(&msg, &JsValue::from_str("id"), &JsValue::from(id));
    let _ = Reflect::set(&msg, &JsValue::from_str("op"), &JsValue::from_str(job.op));
    let arr = Array::new();
    for a in &job.args {
        arr.push(a);
    }
    let _ = Reflect::set(&msg, &JsValue::from_str("args"), &arr);
    if worker.post_message_with_transfer(&msg, &Array::new()).is_err() {
        let pending = RAW_POOL.with(|p| {
            let mut p = p.borrow_mut();
            let s = &mut p[i];
            s.dead = true;
            s.pending.take()
        });
        if let Some((_, _, cb)) = pending {
            cb(Err(JsValue::from_str("raw pool post failed")));
        }
        return false;
    }
    true
}

fn pool_pump() {
    loop {
        let job = POOL_QUEUE.with(|q| q.borrow_mut().pop_front());
        let Some(job) = job else { return };
        match pool_free_slot() {
            SlotPick::Free(i) => {
                post_pool(i, job);
            }
            SlotPick::Busy => {
                POOL_QUEUE.with(|q| q.borrow_mut().push_front(job));
                return;
            }
            SlotPick::Unavailable => {
                (job.cb)(Err(JsValue::from_str("raw worker pool unavailable")));
                fail_pool_queue();
                return;
            }
        }
    }
}

fn fail_pool_queue() {
    let jobs: Vec<PoolJob> = POOL_QUEUE.with(|q| q.borrow_mut().drain(..).collect());
    for job in jobs {
        (job.cb)(Err(JsValue::from_str("raw worker pool unavailable")));
    }
}

/// Queue a display-proxy develop on the batch pool. The callback fires from
/// the worker message, so callers await it like any raw op.
pub fn raw_proxy_pool(
    id: usize,
    src: &JsValue,
    max_edge: u32,
    cb: impl FnOnce(Option<ProxyResult>) + 'static,
) -> bool {
    POOL_QUEUE.with(|q| {
        q.borrow_mut().push_back(PoolJob {
            op: "raw_proxy",
            args: vec![
                JsValue::from(id as u32),
                src.clone(),
                JsValue::from(max_edge),
                JsValue::from_bool(false),
            ],
            cb: Box::new(move |reply| cb(reply.ok().and_then(|v| parse_proxy(&v)))),
        });
    });
    pool_pump();
    true
}

/// Move a queued develop for `id` to the front — a batch that is still
/// importing should not keep the item the user just selected waiting.
pub fn raw_pool_prioritize(id: usize) {
    POOL_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if let Some(pos) = q
            .iter()
            .position(|j| j.args.first().and_then(|a| a.as_f64()) == Some(id as f64))
        {
            if pos > 0 {
                if let Some(job) = q.remove(pos) {
                    q.push_front(job);
                }
            }
        }
    });
}

/// Terminate every pool worker (their wasm memory never shrinks) and fail any
/// queued or in-flight callbacks.
pub fn raw_pool_shutdown() {
    let (workers, pending): (Vec<Worker>, Vec<Box<dyn FnOnce(Reply)>>) = RAW_POOL.with(|p| {
        let mut p = p.borrow_mut();
        let mut workers = Vec::new();
        let mut pending = Vec::new();
        for s in p.iter_mut() {
            if let Some(w) = s.worker.take() {
                workers.push(w);
            }
            if let Some((_, _, cb)) = s.pending.take() {
                pending.push(cb);
            }
            s.dead = false;
        }
        (workers, pending)
    });
    for w in workers {
        w.terminate();
    }
    for cb in pending {
        cb(Err(JsValue::from_str("raw pool shutdown")));
    }
    fail_pool_queue();
}

pub struct ProxyResult {
    /// Transferred from the worker — a JS view, never copied into Rust.
    pub pixels: Uint8Array,
    pub w: usize,
    pub h: usize,
    pub full_w: usize,
    pub full_h: usize,
    /// Tone curve applied by a sensor develop, for the export develop to reuse.
    pub tone: Option<[u8; 256]>,
}

fn parse_proxy(v: &JsValue) -> Option<ProxyResult> {
    let tone = get_buf(v, "tone").and_then(|b| {
        if b.length() == 256 {
            let mut lut = [0u8; 256];
            b.copy_to(&mut lut);
            Some(lut)
        } else {
            None
        }
    });
    Some(ProxyResult {
        pixels: get_buf(v, "pixels")?,
        w: get_u32(v, "w")? as usize,
        h: get_u32(v, "h")? as usize,
        full_w: get_u32(v, "full_w")? as usize,
        full_h: get_u32(v, "full_h")? as usize,
        tone,
    })
}

/// True while a proxy decode is queued or running on the raw worker.
pub fn raw_busy() -> bool {
    RAW_PENDING.with(|p| !p.borrow().is_empty())
}

/// Terminate the raw worker and free its wasm memory (which never shrinks
/// while it lives). The worker is recreated lazily by the next RAW op, so this
/// costs one decoder init + develop if the item is edited again. Callers must
/// check `raw_busy()` first; queued replies are failed rather than dropped.
pub fn raw_shutdown() {
    let worker = RAW_WORKER.with(|w| w.borrow_mut().take());
    if let Some(worker) = worker {
        worker.terminate();
    }
    let cbs: Vec<_> = RAW_PENDING.with(|p| p.borrow_mut().drain().map(|(_, c)| c).collect());
    RAW_PENDING_OP.with(|m| m.borrow_mut().clear());
    for cb in cbs {
        cb(Err(JsValue::from_str("worker shutdown")));
    }
}

/// Ensure the raw worker is up; false when it is unavailable (callers fall
/// back to main-thread decoding).
pub fn proxy_worker_available() -> bool {
    raw_init()
}

/// Develop a RAW to a display-resolution proxy in the dedicated worker. `src`
/// is a Blob (the IndexedDB original — structured-cloned by reference) or a
/// Uint8Array of bytes the app already holds. Returns false when the worker is
/// unavailable (caller falls back).
pub fn raw_proxy(
    id: usize,
    src: &JsValue,
    max_edge: u32,
    cb: impl FnOnce(Option<ProxyResult>) + 'static,
) -> bool {
    if !raw_init() {
        return false;
    }
    raw_call(
        "raw_proxy",
        &[
            JsValue::from(id as u32),
            src.clone(),
            JsValue::from(max_edge),
            JsValue::from_bool(true),
        ],
        &[],
        Box::new(move |reply| cb(reply.ok().and_then(|v| parse_proxy(&v)))),
    )
}

/// Decode the camera's embedded full-size JPEG into a proxy (orientation from
/// the RAW metadata code, since the JPEG carries no EXIF of its own).
pub fn camera_proxy(
    jpeg: &JsValue,
    orientation: u16,
    max_edge: u32,
    cb: impl FnOnce(Option<ProxyResult>) + 'static,
) -> bool {
    if !raw_init() {
        return false;
    }
    raw_call(
        "camera_proxy",
        &[jpeg.clone(), JsValue::from(orientation), JsValue::from(max_edge)],
        &[],
        Box::new(move |reply| cb(reply.ok().and_then(|v| parse_proxy(&v)))),
    )
}

pub struct RawMeta {
    pub exif_tiff: Option<Vec<u8>>,
    pub orientation: u16,
    pub taken: Option<f64>,
    /// Transferred from the worker — a JS view, never copied into Rust.
    pub camera_jpeg: Option<Uint8Array>,
    pub camera_w: usize,
    pub camera_h: usize,
    pub preview: Option<(Vec<u8>, usize, usize)>,
    pub get_decoder_ms: f64,
    pub get_decoder_first: bool,
}

fn parse_raw_meta(v: &JsValue) -> Option<RawMeta> {
    let pw = get_u32(v, "preview_w")? as usize;
    let ph = get_u32(v, "preview_h")? as usize;
    let preview = if pw > 0 && ph > 0 {
        Some((get_buf(v, "preview")?.to_vec(), pw, ph))
    } else {
        None
    };
    Some(RawMeta {
        exif_tiff: get_buf(v, "exif_tiff").map(|b| b.to_vec()),
        orientation: get_u32(v, "orientation")? as u16,
        taken: get_f64(v, "taken"),
        camera_jpeg: get_buf(v, "camera_jpeg"),
        camera_w: get_u32(v, "camera_w")? as usize,
        camera_h: get_u32(v, "camera_h")? as usize,
        preview,
        get_decoder_ms: get_f64(v, "get_decoder_ms").unwrap_or(0.0),
        get_decoder_first: get_bool(v, "get_decoder_first").unwrap_or(false),
    })
}

/// `src` is a Blob (the IndexedDB original — structured-cloned by reference,
/// so the app thread never materialises it) or a Uint8Array of bytes the app
/// already holds. run.js turns a Blob into a Uint8Array inside the worker.
pub fn raw_meta(
    src: &JsValue,
    preview_edge: u32,
    cb: impl FnOnce(Option<RawMeta>) + 'static,
) -> bool {
    if !raw_init() {
        return false;
    }
    raw_call(
        "raw_meta",
        &[src.clone(), JsValue::from(preview_edge)],
        &[],
        Box::new(move |reply| cb(reply.ok().and_then(|v| parse_raw_meta(&v)))),
    )
}

pub struct FullResult {
    /// Transferred from the worker — a JS view, never copied into Rust.
    pub pixels: Uint8Array,
    pub w: usize,
    pub h: usize,
}

/// Full sensor develop in the raw worker. `tone` is the display proxy's tone
/// curve (developed on any worker) so the export matches what is on screen.
pub fn raw_develop(
    id: usize,
    src: &JsValue,
    tone: Option<&[u8; 256]>,
    cb: impl FnOnce(Option<FullResult>) + 'static,
) -> bool {
    if !raw_init() {
        return false;
    }
    let tone = match tone {
        Some(lut) => Uint8Array::from(&lut[..]).into(),
        None => Uint8Array::new_with_length(0).into(),
    };
    raw_call(
        "raw_develop",
        &[JsValue::from(id as u32), src.clone(), tone],
        &[],
        Box::new(move |reply| {
            cb(reply.ok().and_then(|v| {
                Some(FullResult {
                    pixels: get_buf(&v, "pixels")?,
                    w: get_u32(&v, "w")? as usize,
                    h: get_u32(&v, "h")? as usize,
                })
            }))
        }),
    )
}

/// Embedded preview (no demosaic) at display resolution.
pub fn raw_preview(
    src: &JsValue,
    max_edge: u32,
    cb: impl FnOnce(Option<ProxyResult>) + 'static,
) -> bool {
    if !raw_init() {
        return false;
    }
    raw_call(
        "raw_preview",
        &[src.clone(), JsValue::from(max_edge)],
        &[],
        Box::new(move |reply| cb(reply.ok().and_then(|v| parse_proxy(&v)))),
    )
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

fn get_bool(v: &JsValue, key: &str) -> Option<bool> {
    Reflect::get(v, &JsValue::from_str(key)).ok()?.as_bool()
}

fn get_f64(v: &JsValue, key: &str) -> Option<f64> {
    Reflect::get(v, &JsValue::from_str(key)).ok()?.as_f64()
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

/// Ship pixels the app only holds as a JS view. The view stays owned by the
/// caller, so this is a structured-clone copy into the worker rather than a
/// transfer — still one copy fewer than materialising a Rust Vec first.
pub fn load_view(id: usize, pixels: &Uint8Array, w: usize, h: usize) {
    call(
        "load",
        &[
            JsValue::from(id as u32),
            pixels.clone().into(),
            JsValue::from(w as u32),
            JsValue::from(h as u32),
        ],
        &[],
        Box::new(|_| {}),
    );
}

pub fn unload(id: usize) {
    call("unload", &[JsValue::from(id as u32)], &[], Box::new(|_| {}));
    // The RAW worker holds the last proxy's full frame / tone curve for the
    // export reuse; drop that item there too.
    if RAW_WORKER.with(|w| w.borrow().is_some()) {
        raw_call("unload", &[JsValue::from(id as u32)], &[], Box::new(|_| {}));
    }
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
