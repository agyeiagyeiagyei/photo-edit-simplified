//! Batch RAW worker pool: pooled worker instances develop display proxies for
//! an import without queueing every file behind the selected item's hi-res work.

use super::*;

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
