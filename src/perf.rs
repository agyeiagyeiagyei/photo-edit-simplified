//! Perf marks: thin wrappers over `performance.mark()` / `performance.measure()`
//! plus a 500-entry ring buffer exposed as `window.__pes.perf()` (JSON array of
//! `{name, dur_ms, at, detail}`) and `window.__pes.reset()`. Always on; the
//! whole cost is one measure per instrumented step. Mark names are documented
//! in `docs/PERF.md` — QA asserts on them.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use js_sys::{Array, Object, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::Performance;

const RING: usize = 500;

struct Entry {
    name: String,
    dur_ms: f64,
    at: f64,
    detail: Option<String>,
}

thread_local! {
    static RING_BUF: RefCell<VecDeque<Entry>> = RefCell::new(VecDeque::with_capacity(RING));
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
}

fn performance() -> Option<Performance> {
    web_sys::window()?.performance()
}

/// High-resolution milliseconds since navigation start (0 when unavailable).
pub fn now() -> f64 {
    performance().map(|p| p.now()).unwrap_or(0.0)
}

pub fn record(name: &str, dur_ms: f64) {
    record_detail(name, dur_ms, None);
}

pub fn record_detail(name: &str, dur_ms: f64, detail: Option<String>) {
    let at = now();
    push(Entry { name: name.to_string(), dur_ms, at, detail });
}

fn push(entry: Entry) {
    RING_BUF.with(|r| {
        let mut r = r.borrow_mut();
        if r.len() >= RING {
            r.pop_front();
        }
        r.push_back(entry);
    });
}

/// RAII span. Emits a `performance.mark` pair and a `performance.measure` on
/// drop, and pushes the measure into the ring buffer. The guard may be held
/// across `await` (the future is !Send on wasm); it then measures wall time.
pub struct Span {
    name: &'static str,
    start: f64,
    start_mark: String,
    end_mark: String,
    detail: Option<String>,
    done: bool,
}

impl Span {
    pub fn new(name: &'static str) -> Span {
        let id = NEXT_ID.with(|n| {
            let i = n.get();
            n.set(i + 1);
            i
        });
        let start_mark = format!("__pes_{id}_start");
        let end_mark = format!("__pes_{id}_end");
        let start = match performance() {
            Some(p) => {
                let _ = p.mark(&start_mark);
                p.now()
            }
            None => 0.0,
        };
        Span { name, start, start_mark, end_mark, detail: None, done: false }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Span {
        self.detail = Some(detail.into());
        self
    }

    pub fn set_detail(&mut self, detail: impl Into<String>) {
        self.detail = Some(detail.into());
    }

    fn finish_inner(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let end = now();
        if let Some(p) = performance() {
            let _ = p.mark(&self.end_mark);
            let _ = p.measure_with_start_mark_and_end_mark(
                self.name,
                &self.start_mark,
                &self.end_mark,
            );
            p.clear_marks_with_mark_name(&self.start_mark);
            p.clear_marks_with_mark_name(&self.end_mark);
        }
        push(Entry {
            name: self.name.to_string(),
            dur_ms: end - self.start,
            at: end,
            detail: self.detail.take(),
        });
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.finish_inner();
    }
}

fn snapshot() -> JsValue {
    let arr = Array::new();
    RING_BUF.with(|r| {
        for e in r.borrow().iter() {
            let o = Object::new();
            let _ = Reflect::set(&o, &JsValue::from_str("name"), &JsValue::from_str(&e.name));
            let _ = Reflect::set(&o, &JsValue::from_str("dur_ms"), &JsValue::from(e.dur_ms));
            let _ = Reflect::set(&o, &JsValue::from_str("at"), &JsValue::from(e.at));
            let detail = match &e.detail {
                Some(d) => JsValue::from_str(d),
                None => JsValue::NULL,
            };
            let _ = Reflect::set(&o, &JsValue::from_str("detail"), &detail);
            arr.push(&o);
        }
    });
    arr.into()
}

fn reset() {
    RING_BUF.with(|r| r.borrow_mut().clear());
}

/// Publish `window.__pes.perf()` / `window.__pes.reset()`. Idempotent; other
/// helpers may share the `__pes` namespace.
pub fn install() {
    let Some(win) = web_sys::window() else { return };
    let pes = Reflect::get(win.as_ref(), &JsValue::from_str("__pes"))
        .ok()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| {
            let o = Object::new();
            let _ = Reflect::set(win.as_ref(), &JsValue::from_str("__pes"), o.as_ref());
            o.into()
        });
    let perf_fn = Closure::<dyn Fn() -> JsValue>::new(snapshot);
    let _ = Reflect::set(&pes, &JsValue::from_str("perf"), perf_fn.as_ref().unchecked_ref());
    perf_fn.forget();
    let reset_fn = Closure::<dyn Fn()>::new(reset);
    let _ = Reflect::set(&pes, &JsValue::from_str("reset"), reset_fn.as_ref().unchecked_ref());
    reset_fn.forget();
}
