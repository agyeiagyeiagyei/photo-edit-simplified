mod drive;
mod exif;
mod zip;
mod perf;
mod pworker;
mod raw;
mod session;
mod state;
mod web;

use pes_pixel::{blur, clone, color, curves, fill, heal, levels, ops, rawops, stroke, wand};
use pes_pixel::geo::{crop_px, geometry, hi_target, stamp_stroke, working_dims};
use pes_pixel::types::RawAdjust;

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use leptos::leptos_dom::helpers::window_event_listener;
use leptos::*;
use leptos::batch;
use js_sys::Uint8Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Blob, File, Url};

use state::{AppState, Aspect, BrushStroke, DriveFilter, DriveSort, EditParams, Layer, LayerKind, MediaItem, MediaKind, PathPoint, PhotoFormat, SelectTool, Selection, SelectionKind, TextAlign, Tool};

/// RGBA pixels the app holds without copying them into Rust when they came
/// back from a worker: a view over the transferred ArrayBuffer, or an owned
/// buffer for main-thread decodes and destructive-op results.
#[derive(Clone)]
enum Pixels {
    View(Uint8Array),
    Owned(Rc<Vec<u8>>),
}

impl Pixels {
    fn view(v: Uint8Array) -> Pixels {
        Pixels::View(v)
    }

    fn owned(v: Vec<u8>) -> Pixels {
        Pixels::Owned(Rc::new(v))
    }

    fn len(&self) -> usize {
        match self {
            Pixels::View(v) => v.length() as usize,
            Pixels::Owned(v) => v.len(),
        }
    }

    /// Borrow owned pixels; a view has to be materialised first.
    fn as_cow(&self) -> Cow<'_, [u8]> {
        match self {
            Pixels::Owned(v) => Cow::Borrowed(v.as_slice()),
            Pixels::View(v) => Cow::Owned(v.to_vec()),
        }
    }

    /// Run a slice-based routine without cloning owned pixels.
    fn with_slice<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        match self {
            Pixels::Owned(v) => f(v.as_slice()),
            Pixels::View(v) => f(&v.to_vec()),
        }
    }

    fn put_on_canvas(&self, canvas: &web_sys::HtmlCanvasElement, w: u32, h: u32) {
        match self {
            Pixels::View(v) => web::put_pixels_view(canvas, v, w, h),
            Pixels::Owned(v) => web::put_pixels(canvas, v, w, h),
        }
    }

    fn load_into_worker(&self, id: usize, w: usize, h: usize) {
        match self {
            Pixels::View(v) => pworker::load_view(id, v, w, h),
            Pixels::Owned(v) => pworker::load(id, v, w, h),
        }
    }

    /// Copy a window of rows out of the buffer without materialising the whole
    /// image (the focus loupe runs this on every pointer move).
    fn crop_rows(&self, w: usize, sy: usize, sx: usize, cw: usize, ch: usize) -> Vec<u8> {
        let mut crop = vec![0u8; cw * ch * 4];
        for row in 0..ch {
            let start = ((sy + row) * w + sx) * 4;
            let dst = &mut crop[row * cw * 4..(row + 1) * cw * 4];
            match self {
                Pixels::View(v) => {
                    let seg = v.subarray(start as u32, (start + cw * 4) as u32);
                    seg.copy_to(dst);
                }
                Pixels::Owned(v) => dst.copy_from_slice(&v[start..start + cw * 4]),
            }
        }
        crop
    }

    async fn to_png_blob(&self, w: usize, h: usize) -> Result<Blob, JsValue> {
        let canvas = web::create_canvas(w as u32, h as u32);
        self.put_on_canvas(&canvas, w as u32, h as u32);
        web::canvas_to_blob(&canvas, "image/png").await
    }

    fn downscale(&self, w: usize, h: usize, long_edge: u32) -> (Vec<u8>, usize, usize) {
        let scale = (long_edge as f32 / w.max(h) as f32).min(1.0);
        let tw = (w as f32 * scale).max(1.0) as u32;
        let th = (h as f32 * scale).max(1.0) as u32;
        let src = web::create_canvas(w as u32, h as u32);
        self.put_on_canvas(&src, w as u32, h as u32);
        let dst = web::create_canvas(tw, th);
        let ctx = web::ctx2d(&dst);
        ctx.draw_image_with_html_canvas_element_and_dw_and_dh(&src, 0.0, 0.0, tw as f64, th as f64)
            .unwrap();
        web::image_data_from_ctx(&ctx, tw, th)
    }
}

/// Geometry on a cached source, materialised for callers that mutate.
fn owned_geometry(
    full: &Pixels,
    fw: usize,
    fh: usize,
    edit: &EditParams,
) -> (Vec<u8>, usize, usize) {
    let src = full.as_cow();
    let (p, w, h) = geometry(&src, fw, fh, edit);
    (p.into_owned(), w, h)
}

// --- media cache (non-reactive) ---------------------------------------------

#[derive(Clone)]
struct PhotoData {
    /// Full-res RGBA. None until materialized (imports decode preview-only);
    /// may be evicted under memory pressure, but never while `dirty`.
    full: Option<(Pixels, usize, usize)>,
    /// Source dimensions, known without holding the pixels.
    full_dims: (usize, usize),
    preview: (Vec<u8>, usize, usize),
    /// A destructive op landed since the last session save persisted the
    /// raster — until then this cache copy is the only full-res source of
    /// truth, so dirty items are exempt from eviction.
    dirty: bool,
    /// Bumped whenever full/preview are rebuilt in place (heal, cut, …) so the
    /// hi-res render cache knows its source pixels changed.
    gen: u64,
    /// RAW import: full pixels are developed lazily from the original bytes,
    /// which live in IndexedDB rather than in memory.
    raw: bool,
    /// Original RAW byte length — keeps the session signature stable once the
    /// bytes are no longer held in memory.
    raw_len: u64,
    /// The original made it into the raw_src store. False means the fallback
    /// is in play: full pixels were developed eagerly and the bytes stay in
    /// `raw_fallback` so the session record can embed them.
    raw_persisted: bool,
}

struct Cache {
    photos: HashMap<usize, Rc<PhotoData>>,
    /// Compressed source blobs (JPEG/PNG/HEIC-converted) for on-demand
    /// full-res re-decode. File-backed for local imports, so cheap to hold.
    photo_src: HashMap<usize, Blob>,
    video_blobs: HashMap<usize, Blob>,
    video_meta: HashMap<usize, (f64, u32, u32)>, // duration, w, h
    /// RAW originals that could not be written to IndexedDB (quota, private
    /// mode) — only kept so persistence can fall back to the old embedded-blob
    /// record. Empty on the normal path.
    raw_fallback: HashMap<usize, Rc<Vec<u8>>>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache {
        photos: HashMap::new(),
        photo_src: HashMap::new(),
        video_blobs: HashMap::new(),
        video_meta: HashMap::new(),
        raw_fallback: HashMap::new(),
    });
}

fn get_photo(id: usize) -> Option<Rc<PhotoData>> {
    CACHE.with(|c| c.borrow().photos.get(&id).cloned())
}

/// Decoded full-res budget across all items (~512 MB, e.g. ~5 25MP photos).
/// The worker mirrors the same set, so the true footprint is ~2x this plus
/// decode transients — keep it well under the renderer's memory cap.
/// Dirty items (a destructive op landed since the last session save) are
/// exempt — their cache copy is the only source of truth until persisted.
const FULL_BUDGET: usize = 512 << 20;

thread_local! {
    static FULL_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    static FULL_WAITERS: RefCell<HashMap<usize, Vec<Box<dyn FnOnce()>>>> =
        RefCell::new(HashMap::new());
    static LAST_USE: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    static USE_TICK: Cell<u64> = const { Cell::new(0) };
}

fn full_present(id: usize) -> bool {
    CACHE.with(|c| {
        c.borrow()
            .photos
            .get(&id)
            .map(|p| p.full.is_some())
            .unwrap_or(false)
    })
}

fn touch_full(id: usize) {
    let t = USE_TICK.with(|t| {
        t.set(t.get() + 1);
        t.get()
    });
    LAST_USE.with(|l| l.borrow_mut().insert(id, t));
}

/// Drop least-recently-used clean full-res buffers until the decoded total
/// fits the budget; the worker's copy goes with each eviction.
fn evict_full_lru() {
    let evicted: Vec<usize> = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let mut total: usize = c
            .photos
            .values()
            .filter_map(|p| p.full.as_ref().map(|f| f.0.len()))
            .sum();
        if total <= FULL_BUDGET {
            return Vec::new();
        }
        let mut candidates: Vec<(u64, usize, usize)> = c
            .photos
            .iter()
            .filter(|(_, p)| !p.dirty && p.full.is_some())
            .map(|(id, p)| {
                (
                    LAST_USE.with(|l| l.borrow().get(id).copied()).unwrap_or(0),
                    *id,
                    p.full.as_ref().map(|f| f.0.len()).unwrap_or(0),
                )
            })
            .collect();
        candidates.sort_by_key(|c| c.0);
        let mut evicted = Vec::new();
        for (_, id, len) in candidates {
            if total <= FULL_BUDGET {
                break;
            }
            if let Some(p) = c.photos.get_mut(&id) {
                Rc::make_mut(p).full = None;
                total -= len;
                evicted.push(id);
            }
        }
        evicted
    });
    for id in evicted {
        pworker::unload(id);
    }
}

/// Install a freshly decoded full-res buffer: cache + worker + LRU, then run
/// waiters and nudge items so pending hi-res renders reschedule.
fn materialize_full(state: AppState, id: usize, pixels: Pixels, w: usize, h: usize) {
    pixels.load_into_worker(id, w, h);
    // Sensor pixels supersede the display proxy (and any render it fed).
    if proxy_in_mem(id) {
        drop_proxy(id);
    }
    CACHE.with(|c| {
        if let Some(p) = c.borrow_mut().photos.get_mut(&id) {
            let p = Rc::make_mut(p);
            p.full_dims = (w, h);
            p.full = Some((pixels, w, h));
        }
    });
    touch_full(id);
    evict_full_lru();
    let waiters = FULL_WAITERS.with(|w| w.borrow_mut().remove(&id)).unwrap_or_default();
    for w in waiters {
        w();
    }
    state.items.update(|_| {});
}

/// Decode an item's full-res pixels on demand from its source blob (healed
/// PNG or imported JPEG/PNG) or RAW bytes. No-op when present or in flight;
/// `waiter` runs once the buffer lands.
fn ensure_full(state: AppState, id: usize, waiter: Option<Box<dyn FnOnce()>>) {
    touch_full(id);
    // The develop is about to take ~100MB; the camera loupe copy is moot
    // once these pixels exist, so don't let them stack.
    camera_loupe_drop(id);
    if full_present(id) {
        if let Some(w) = waiter {
            w();
        }
        return;
    }
    if let Some(w) = waiter {
        FULL_WAITERS.with(|ws| ws.borrow_mut().entry(id).or_default().push(w));
    }
    if !FULL_INFLIGHT.with(|f| f.borrow_mut().insert(id)) {
        return;
    }
    let src = CACHE.with(|c| c.borrow().photo_src.get(&id).cloned());
    let is_raw = CACHE.with(|c| {
        c.borrow().photos.get(&id).map(|p| p.raw).unwrap_or(false)
    });
    spawn_local(async move {
        let decoded = if let Some(blob) = src {
            match web::load_image(&blob).await {
                Ok((img, url)) => {
                    let _ = Url::revoke_object_url(&url);
                    let (px, w, h) = web::rgba_from_image(&img);
                    Some((Pixels::owned(px), w, h))
                }
                Err(_) => None,
            }
        } else if is_raw {
            // Lazy develop: hand the stored Blob to the worker, which reads it
            // itself — the app thread never sees the file bytes.
            match session::load_raw_blob(id).await {
                Some(blob) => raw_develop_async(id, blob.as_ref()).await,
                None => None,
            }
        } else {
            None
        };
        if is_raw && decoded.is_some() && !pworker::raw_busy() {
            // The full frame is on this thread now; the RAW worker's wasm
            // (hundreds of MB after a 24 MP develop, and never shrinking)
            // would otherwise sit idle through composite + encode — the
            // peak of the whole export. Recreated lazily on the next RAW op.
            pworker::raw_shutdown();
        }
        FULL_INFLIGHT.with(|f| f.borrow_mut().remove(&id));
        let still_tracked = CACHE.with(|c| c.borrow().photos.contains_key(&id));
        match (decoded, still_tracked) {
            (Some((px, w, h)), true) => materialize_full(state, id, px, w, h),
            _ => {
                // Gone or undecodable — drop waiters; their fns re-check and bail.
                let waiters =
                    FULL_WAITERS.with(|w| w.borrow_mut().remove(&id)).unwrap_or_default();
                for w in waiters {
                    w();
                }
            }
        }
    });
}

async fn sleep_ms(ms: i32) {
    let p = js_sys::Promise::new(&mut |res, _| {
        let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(&res, ms);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(p).await;
}

/// Awaitable ensure_full for the export path. False when undecodable.
async fn ensure_full_async(state: AppState, id: usize) -> bool {
    if full_present(id) {
        return true;
    }
    let done = Rc::new(Cell::new(false));
    let done2 = done.clone();
    ensure_full(state, id, Some(Box::new(move || done2.set(true))));
    for _ in 0..600 {
        if done.get() {
            break;
        }
        sleep_ms(50).await;
    }
    full_present(id)
}

// --- display proxies for lazy RAW items ---------------------------------------
// A RAW import shows the camera's embedded JPEG immediately, then develops a
// display-resolution proxy in a second worker. Proxies live outside PhotoData:
// their lifetime is independent of preview rebuilds, and their worker ids sit
// in a separate namespace so a proxy and the sensor buffer can coexist for the
// same item while the develop lands.

/// Long edge of a display proxy — the same cap as the hi-res cache, so the
/// settled canvas renders from a proxy at full display density.
const PROXY_EDGE: u32 = HI_MAX_EDGE as u32;
/// Worker id namespace for proxy buffers (item ids stay small).
const PROXY_WID: usize = 0x8000_0000;
/// Main-side proxy budget. Each live proxy is mirrored in the worker, so the
/// real footprint is ~2x this (three 3200×2133 proxies).
const PROXY_BUDGET: usize = 160 << 20;
/// The half-size sensor proxy develops in ~1 s; wait only briefly so the
/// selection settles without making a wasted develop expensive.
const PROXY_DEVELOP_DELAY_MS: i32 = 750;

struct ProxyData {
    pixels: Pixels,
    w: usize,
    h: usize,
    /// True sensor-frame dims (the proxy pixels are capped below this).
    frame: (usize, usize),
    /// False while this is the camera JPEG, true once sensor pixels landed.
    developed: bool,
}

thread_local! {
    static PROXY_CACHE: RefCell<HashMap<usize, Rc<ProxyData>>> = RefCell::new(HashMap::new());
    static PROXY_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    static PROXY_LAST_USE: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    /// Proxy-content version, mixed into render fingerprints. Never use
    /// photo.gen for this — for RAW items gen > 0 means "healed".
    static PROXY_GEN: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    /// Item whose sensor develop the UI is waiting on.
    static PROXY_UPGRADE_WANT: Cell<Option<usize>> = const { Cell::new(None) };
    static PROXY_UPGRADE_BUSY: Cell<bool> = const { Cell::new(false) };
    static PROXY_UPGRADE_TIMER: Cell<u32> = const { Cell::new(0) };
    /// Newest hover decode. A request superseded while another decode runs is
    /// dropped; selected-item requests are never gated.
    static PROXY_DECODE_WANT: Cell<Option<usize>> = const { Cell::new(None) };
    /// Tone curve fitted when an item's display proxy developed, keyed by item.
    /// The export develop passes it explicitly so pixels match the screen even
    /// when the develop runs in a fresh worker (pool develop, reload).
    static RAW_TONES: RefCell<HashMap<usize, [u8; 256]>> = RefCell::new(HashMap::new());
    /// Items whose display proxy a batch pool worker is developing. The
    /// interactive pump must not start a duplicate develop for these.
    static POOL_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    /// Queued + in-flight pool develops for the current import batch.
    static POOL_JOBS: Cell<usize> = const { Cell::new(0) };
    /// Open ingest scopes (overlapping drops share one batch mark).
    static POOL_SCOPES: Cell<usize> = const { Cell::new(0) };
    static POOL_BATCH_START: Cell<f64> = const { Cell::new(0.0) };
    static POOL_BATCH_FILES: Cell<usize> = const { Cell::new(0) };
}

fn proxy_wid(id: usize) -> usize {
    PROXY_WID | id
}

fn proxy_gen(id: usize) -> u64 {
    PROXY_GEN.with(|g| g.borrow().get(&id).copied().unwrap_or(0))
}

fn bump_proxy_gen(id: usize) {
    PROXY_GEN.with(|g| {
        let mut g = g.borrow_mut();
        *g.entry(id).or_insert(0) += 1;
    });
}

fn proxy_in_mem(id: usize) -> bool {
    PROXY_CACHE.with(|c| c.borrow().contains_key(&id))
}

fn proxy_data(id: usize) -> Option<Rc<ProxyData>> {
    PROXY_CACHE.with(|c| c.borrow().get(&id).cloned())
}

fn proxy_developed(id: usize) -> bool {
    PROXY_CACHE.with(|c| c.borrow().get(&id).map(|p| p.developed).unwrap_or(false))
}

fn touch_proxy(id: usize) {
    let t = USE_TICK.with(|t| {
        t.set(t.get() + 1);
        t.get()
    });
    PROXY_LAST_USE.with(|l| l.borrow_mut().insert(id, t));
}

/// Forget an item's proxy: worker copy, cache entry, and — so any render the
/// superseded pixels could feed is invalidated — a version bump.
fn drop_proxy(id: usize) {
    let had = PROXY_CACHE.with(|c| c.borrow_mut().remove(&id).is_some());
    PROXY_LAST_USE.with(|l| l.borrow_mut().remove(&id));
    if !had {
        return;
    }
    pworker::unload(proxy_wid(id));
    bump_proxy_gen(id);
    LOUPE_SRC.with(|s| {
        if matches!(&*s.borrow(), Some((sid, ..)) if *sid == id) {
            *s.borrow_mut() = None;
        }
    });
}

/// Full cleanup when an item leaves the session.
fn purge_proxy(id: usize) {
    drop_proxy(id);
    RAW_TONES.with(|t| t.borrow_mut().remove(&id));
    PROXY_GEN.with(|g| g.borrow_mut().remove(&id));
    PROXY_UPGRADE_WANT.with(|w| {
        if w.get() == Some(id) {
            w.set(None);
        }
    });
    PROXY_DECODE_WANT.with(|w| {
        if w.get() == Some(id) {
            w.set(None);
        }
    });
}

/// Drop least-recently-used proxies while over budget. The item just landed,
/// the selected item, and both hover loupes are protected — evicting what the
/// UI is about to draw would only start another decode.
fn evict_proxy_lru(state: AppState, keep: usize) {
    let victims: Vec<usize> = {
        let total: usize =
            PROXY_CACHE.with(|c| c.borrow().values().map(|p| p.pixels.len()).sum());
        if total <= PROXY_BUDGET {
            return;
        }
        let selected = state.selected.get_untracked();
        let loupe = state.loupe.get_untracked().map(|(id, ..)| id);
        let canvas_loupe = state.canvas_loupe.get_untracked().map(|(id, ..)| id);
        let mut candidates: Vec<(u64, usize, usize)> = PROXY_CACHE.with(|c| {
            c.borrow()
                .iter()
                .filter(|(id, _)| {
                    **id != keep
                        && Some(**id) != selected
                        && Some(**id) != loupe
                        && Some(**id) != canvas_loupe
                })
                .map(|(id, p)| {
                    (
                        PROXY_LAST_USE.with(|l| l.borrow().get(id).copied()).unwrap_or(0),
                        *id,
                        p.pixels.len(),
                    )
                })
                .collect()
        });
        candidates.sort_by_key(|c| c.0);
        let mut total = total;
        let mut victims = Vec::new();
        for (_, id, len) in candidates {
            if total <= PROXY_BUDGET {
                break;
            }
            victims.push(id);
            total -= len;
        }
        victims
    };
    for id in victims {
        drop_proxy(id);
    }
}

struct DecodedProxy {
    pixels: Pixels,
    w: usize,
    h: usize,
    frame: (usize, usize),
    developed: bool,
}

/// Install a decoded proxy: cache, worker, dims, version bump. False when the
/// item stopped being lazy (removed, healed, or full pixels landed) meanwhile.
fn land_proxy(state: AppState, id: usize, decoded: DecodedProxy) -> bool {
    let _land = perf::Span::new("proxy.land").detail(id.to_string());
    let lazy = CACHE.with(|c| {
        c.borrow()
            .photos
            .get(&id)
            .map(|p| p.raw && p.full.is_none())
            .unwrap_or(false)
    });
    if !lazy {
        return false;
    }
    // Never downgrade a sensor develop that landed while this was decoding.
    if !decoded.developed && proxy_developed(id) {
        return false;
    }
    decoded.pixels.load_into_worker(proxy_wid(id), decoded.w, decoded.h);
    CACHE.with(|c| {
        if let Some(p) = c.borrow_mut().photos.get_mut(&id) {
            let p = Rc::make_mut(p);
            if p.full.is_none() && p.raw && decoded.frame.0 > 0 && decoded.frame.1 > 0 {
                p.full_dims = decoded.frame;
            }
        }
    });
    PROXY_CACHE.with(|c| {
        c.borrow_mut().insert(
            id,
            Rc::new(ProxyData {
                pixels: decoded.pixels,
                w: decoded.w,
                h: decoded.h,
                frame: decoded.frame,
                developed: decoded.developed,
            }),
        );
    });
    touch_proxy(id);
    bump_proxy_gen(id);
    evict_proxy_lru(state, id);
    state.items.update(|_| {});
    true
}

struct StoredProxy {
    kind: String,
    orientation: u16,
    /// Blob for records written since the zero-copy change; an ArrayBuffer for
    /// records persisted before it. Both are handed to the worker as-is.
    bytes: JsValue,
    frame: (usize, usize),
    /// Tone curve the pixels were developed with; absent for camera JPEGs and
    /// records written before the curve was persisted.
    tone: Option<[u8; 256]>,
}

fn camera_proxy_record(jpeg: &Uint8Array, orientation: u16, frame: (usize, usize)) -> JsValue {
    let o = js_sys::Object::new();
    session::jset(&o, "kind", "camera");
    session::jset(&o, "orientation", orientation as f64);
    let bytes: JsValue = match web::view_to_blob(jpeg, "image/jpeg") {
        Ok(b) => b.into(),
        Err(_) => jpeg.buffer().into(),
    };
    session::jset(&o, "bytes", bytes);
    session::jset(&o, "full_w", frame.0 as f64);
    session::jset(&o, "full_h", frame.1 as f64);
    o.into()
}

fn developed_proxy_record(
    blob: &Blob,
    frame: (usize, usize),
    tone: Option<[u8; 256]>,
) -> JsValue {
    let o = js_sys::Object::new();
    session::jset(&o, "kind", "raw");
    session::jset(&o, "orientation", 1.0);
    session::jset(&o, "bytes", blob);
    session::jset(&o, "full_w", frame.0 as f64);
    session::jset(&o, "full_h", frame.1 as f64);
    if let Some(t) = tone {
        session::jset(&o, "tone", Uint8Array::from(&t[..]));
    }
    o.into()
}

fn parse_proxy_record(v: &JsValue) -> Option<StoredProxy> {
    let kind = state::jget(v, "kind")?.as_string()?;
    let orientation = state::jnum(v, "orientation").unwrap_or(1.0) as u16;
    let bytes = state::jget(v, "bytes")?;
    let empty = if bytes.is_instance_of::<Blob>() {
        bytes.unchecked_ref::<Blob>().size() == 0.0
    } else {
        js_sys::Uint8Array::new(&bytes).length() == 0
    };
    if empty {
        return None;
    }
    let frame = (
        state::jnum(v, "full_w").unwrap_or(0.0) as usize,
        state::jnum(v, "full_h").unwrap_or(0.0) as usize,
    );
    let tone = state::jget(v, "tone").and_then(|t| {
        let b = Uint8Array::new(&t);
        if b.length() == 256 {
            let mut lut = [0u8; 256];
            b.copy_to(&mut lut);
            Some(lut)
        } else {
            None
        }
    });
    Some(StoredProxy { kind, orientation, bytes, frame, tone })
}

async fn read_proxy_record(id: usize) -> Option<StoredProxy> {
    let db = session::open_db().await.ok()?;
    let v = session::get_proxy(&db, id).await.ok()??;
    parse_proxy_record(&v)
}

/// Orientation-corrected pixel dims of an embedded JPEG whose own dims are not.
fn oriented_dims(w: usize, h: usize, orientation: u16) -> (usize, usize) {
    if (5..=8).contains(&orientation) {
        (h, w)
    } else {
        (w, h)
    }
}

/// Poll a worker callback until it fires (the raw worker has no awaitable
/// API). 60 s covers a slow develop on a large sensor.
async fn await_worker<T: 'static>(
    start: impl FnOnce(Box<dyn FnOnce(Option<T>)>) -> bool,
) -> Option<T> {
    let out: Rc<RefCell<Option<T>>> = Rc::new(RefCell::new(None));
    let done = Rc::new(Cell::new(false));
    let (o, d) = (out.clone(), done.clone());
    let started = start(Box::new(move |r| {
        *o.borrow_mut() = r;
        d.set(true);
    }));
    if !started {
        return None;
    }
    for _ in 0..6000 {
        if done.get() {
            break;
        }
        sleep_ms(10).await;
    }
    let result = out.borrow_mut().take();
    result
}

async fn camera_proxy_async(jpeg: &JsValue, orientation: u16) -> Option<pworker::ProxyResult> {
    let jpeg = jpeg.clone();
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::camera_proxy(&jpeg, orientation, PROXY_EDGE, cb)).await;
    perf::record("proxy.camera_decode", perf::now() - t0);
    r
}

async fn raw_proxy_async(id: usize, src: &JsValue) -> Option<pworker::ProxyResult> {
    let src = src.clone();
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::raw_proxy(id, &src, PROXY_EDGE, cb)).await;
    perf::record("proxy.raw_develop", perf::now() - t0);
    store_tone(id, r.as_ref().and_then(|p| p.tone));
    r
}

fn store_tone(id: usize, tone: Option<[u8; 256]>) {
    if let Some(t) = tone {
        RAW_TONES.with(|m| {
            m.borrow_mut().insert(id, t);
        });
    }
}

fn tone_of(id: usize) -> Option<[u8; 256]> {
    RAW_TONES.with(|m| m.borrow().get(&id).copied())
}

/// RAW header metadata + preview from the raw worker.
async fn raw_meta_async(src: &JsValue, preview_edge: u32) -> Option<pworker::RawMeta> {
    let src = src.clone();
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::raw_meta(&src, preview_edge, cb)).await;
    perf::record("proxy.raw_meta", perf::now() - t0);
    if let Some(m) = &r {
        if m.get_decoder_first {
            perf::record("raw.get_decoder", m.get_decoder_ms);
        }
    }
    r
}

/// Full sensor develop in the raw worker. The pixels come back as a view over
/// the transferred buffer — no Rust copy on the app thread.
async fn raw_develop_async(id: usize, src: &JsValue) -> Option<(Pixels, usize, usize)> {
    let src = src.clone();
    let tone = tone_of(id);
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::raw_develop(id, &src, tone.as_ref(), cb)).await;
    perf::record("proxy.raw_develop_full", perf::now() - t0);
    r.map(|f| (Pixels::view(f.pixels), f.w, f.h))
}

/// Embedded preview (no demosaic) at display resolution.
async fn raw_preview_async(src: &JsValue) -> Option<pworker::ProxyResult> {
    let src = src.clone();
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::raw_preview(&src, PROXY_EDGE, cb)).await;
    perf::record("proxy.raw_preview", perf::now() - t0);
    r
}

/// A worker-callable source for an item's original: the IndexedDB Blob
/// (structured-cloned by reference), or the in-memory fallback when the bytes
/// never reached storage.
async fn raw_source_src(id: usize) -> Option<JsValue> {
    if let Some(b) = CACHE.with(|c| c.borrow().raw_fallback.get(&id).cloned()) {
        return Some(Uint8Array::from(&b[..]).into());
    }
    session::load_raw_blob(id).await.map(JsValue::from)
}

/// Decode a stored proxy record. A "raw" record is already sensor pixels —
/// only a JPEG decode remains; a "camera" record still needs the WASM decode.
async fn decode_stored_proxy(id: usize, rec: StoredProxy) -> Option<DecodedProxy> {
    if rec.kind == "raw" {
        let blob = match rec.bytes.dyn_into::<Blob>() {
            Ok(b) => b,
            Err(v) => web::view_to_blob(&js_sys::Uint8Array::new(&v), "image/jpeg").ok()?,
        };
        let (img, url) = web::load_image(&blob).await.ok()?;
        let _ = Url::revoke_object_url(&url);
        let (px, w, h) = web::rgba_from_image(&img);
        store_tone(id, rec.tone);
        Some(DecodedProxy { pixels: Pixels::owned(px), w, h, frame: rec.frame, developed: true })
    } else {
        let r = camera_proxy_async(&rec.bytes, rec.orientation).await?;
        let frame = if rec.frame.0 > 0 { rec.frame } else { (r.full_w, r.full_h) };
        Some(DecodedProxy { pixels: Pixels::view(r.pixels), w: r.w, h: r.h, frame, developed: false })
    }
}

/// First decode for an item with no proxy record: prefer the camera JPEG (and
/// persist a record for next time), else develop fully in the worker.
async fn decode_camera_from_raw(id: usize, src: &JsValue) -> Option<DecodedProxy> {
    if let Some(meta) = raw_meta_async(src, 0).await {
        if let Some(jpeg) = meta.camera_jpeg {
            let orientation = meta.orientation;
            let frame = oriented_dims(meta.camera_w, meta.camera_h, orientation);
            if let Some(r) = camera_proxy_async(&jpeg.clone().into(), orientation).await {
                let frame = if frame.0 > 0 { frame } else { (r.full_w, r.full_h) };
                if let Ok(db) = session::open_db().await {
                    let _ = session::put_proxy(
                        &db,
                        id,
                        &camera_proxy_record(&jpeg, orientation, frame),
                    )
                    .await;
                }
                return Some(DecodedProxy {
                    pixels: Pixels::view(r.pixels),
                    w: r.w,
                    h: r.h,
                    frame,
                    developed: false,
                });
            }
        }
    }
    let r = raw_proxy_async(id, src).await?;
    Some(DecodedProxy {
        pixels: Pixels::view(r.pixels),
        w: r.w,
        h: r.h,
        frame: (r.full_w, r.full_h),
        developed: true,
    })
}

async fn decode_proxy_source(id: usize, priority: bool) -> Option<DecodedProxy> {
    if !priority && PROXY_DECODE_WANT.with(|w| w.get()) != Some(id) {
        return None;
    }
    if let Some(rec) = read_proxy_record(id).await {
        return decode_stored_proxy(id, rec).await;
    }
    if !priority && PROXY_DECODE_WANT.with(|w| w.get()) != Some(id) {
        return None;
    }
    let src = raw_source_src(id).await?;
    decode_camera_from_raw(id, &src).await
}

/// Ask for sensor pixels for `id`, debounced so a develop only starts after
/// the selection has been stable for a moment.
fn request_upgrade(state: AppState, id: usize) {
    PROXY_UPGRADE_WANT.with(|w| w.set(Some(id)));
    schedule_pump(state, PROXY_DEVELOP_DELAY_MS);
}

fn schedule_pump(state: AppState, ms: i32) {
    PROXY_UPGRADE_TIMER.with(|t| t.set(t.get() + 1));
    let gen = PROXY_UPGRADE_TIMER.with(|t| t.get());
    let cb = Closure::once(move || {
        if PROXY_UPGRADE_TIMER.with(|t| t.get()) != gen {
            return;
        }
        pump_upgrade(state);
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        ms,
    );
    cb.forget();
}

/// Start the single in-flight sensor develop if the UI still wants it.
fn pump_upgrade(state: AppState) {
    if PROXY_UPGRADE_BUSY.with(|b| b.get()) {
        return;
    }
    let Some(id) = PROXY_UPGRADE_WANT.with(|w| w.get()) else { return };
    let Some(photo) = get_photo(id) else { return };
    if !photo.raw || photo.full.is_some() {
        return;
    }
    // The user moved on; a new selection re-arms the request.
    if state.selected.get_untracked() != Some(id) {
        return;
    }
    if !proxy_in_mem(id) {
        // Still decoding the camera proxy: it anchors the frame dims and is
        // the fallback if the develop fails, so wait for it.
        if PROXY_INFLIGHT.with(|f| f.borrow().contains(&id)) {
            schedule_pump(state, 600);
        }
        return;
    }
    if proxy_developed(id) {
        return;
    }
    if POOL_INFLIGHT.with(|s| s.borrow().contains(&id)) {
        // A batch worker is already developing this proxy: let its queued job
        // jump ahead and land for the selection instead of developing twice.
        pworker::raw_pool_prioritize(id);
        schedule_pump(state, 600);
        return;
    }
    if pworker::raw_busy() {
        schedule_pump(state, 700);
        return;
    }
    PROXY_UPGRADE_BUSY.with(|b| b.set(true));
    spawn_local(async move {
        let decoded = match raw_source_src(id).await {
            Some(src) => raw_proxy_async(id, &src).await,
            None => None,
        };
        PROXY_UPGRADE_BUSY.with(|b| b.set(false));
        if let Some(r) = decoded {
            let frame = if r.full_w > 0 && r.full_h > 0 {
                (r.full_w, r.full_h)
            } else {
                photo.full_dims
            };
            let landed = land_proxy(
                state,
                id,
                DecodedProxy { pixels: Pixels::view(r.pixels), w: r.w, h: r.h, frame, developed: true },
            );
            if landed {
                if let Some(p) = proxy_data(id) {
                    persist_developed_proxy(id, p.pixels.clone(), p.w, p.h, p.frame, tone_of(id));
                }
            }
        }
        pump_upgrade(state);
    });
}

/// Persist a sensor-developed proxy as a q0.9 JPEG, so a reload skips both the
/// RAW read and the demosaic. Skipped if the item stopped being lazy while the
/// encode ran (heal/cut bake new pixels and purge proxy records).
fn persist_developed_proxy(
    id: usize,
    px: Pixels,
    w: usize,
    h: usize,
    frame: (usize, usize),
    tone: Option<[u8; 256]>,
) {
    spawn_local(async move {
        let canvas = web::create_canvas(w as u32, h as u32);
        px.put_on_canvas(&canvas, w as u32, h as u32);
        let Ok(blob) = web::canvas_to_jpeg_blob(&canvas, 0.9).await else { return };
        let still_lazy = CACHE.with(|c| {
            c.borrow()
                .photos
                .get(&id)
                .map(|p| p.raw && p.full.is_none())
                .unwrap_or(false)
        });
        if !still_lazy {
            return;
        }
        let Ok(db) = session::open_db().await else { return };
        let _ = session::put_proxy(&db, id, &developed_proxy_record(&blob, frame, tone)).await;
    });
}

/// Eagerly develop an imported RAW's display proxy on the batch pool, so a
/// batch of files renders without each selection paying the demosaic and the
/// interactive worker stays free for the selected item. Failures are silent:
/// the item still upgrades on selection through the normal pump.
fn develop_proxy_pool(state: AppState, id: usize, name: String) {
    let eligible = CACHE.with(|c| {
        c.borrow()
            .photos
            .get(&id)
            .map(|p| p.raw && p.full.is_none())
            .unwrap_or(false)
    });
    if !eligible || proxy_developed(id) {
        return;
    }
    // The selected item keeps developing on the interactive worker: that worker
    // retains a full-size proxy frame (non-superpixel sensors), which the export
    // develop reuses. Batch files have no such reuse to lose.
    if state.selected.get_untracked() == Some(id) {
        return;
    }
    if !POOL_INFLIGHT.with(|s| s.borrow_mut().insert(id)) {
        return;
    }
    pool_batch_add();
    spawn_local(async move {
        let t0 = perf::now();
        let r = match raw_source_src(id).await {
            Some(src) => {
                await_worker(move |cb| pworker::raw_proxy_pool(id, &src, PROXY_EDGE, cb)).await
            }
            None => None,
        };
        if let Some(p) = r {
            store_tone(id, p.tone);
            let frame = if p.full_w > 0 && p.full_h > 0 {
                (p.full_w, p.full_h)
            } else {
                (p.w, p.h)
            };
            let landed = land_proxy(
                state,
                id,
                DecodedProxy {
                    pixels: Pixels::view(p.pixels.clone()),
                    w: p.w,
                    h: p.h,
                    frame,
                    developed: true,
                },
            );
            if landed {
                persist_developed_proxy(id, Pixels::view(p.pixels), p.w, p.h, frame, p.tone);
            }
        }
        perf::record_detail("pool.develop", perf::now() - t0, Some(name));
        POOL_INFLIGHT.with(|s| s.borrow_mut().remove(&id));
        pool_batch_done();
    });
}

fn pool_batch_begin() {
    if POOL_SCOPES.with(|s| s.get()) == 0 {
        POOL_BATCH_START.with(|s| s.set(perf::now()));
        POOL_BATCH_FILES.with(|f| f.set(0));
    }
    POOL_SCOPES.with(|s| s.set(s.get() + 1));
}

fn pool_batch_add() {
    POOL_BATCH_FILES.with(|f| f.set(f.get() + 1));
    POOL_JOBS.with(|j| j.set(j.get() + 1));
}

fn pool_batch_done() {
    let left = POOL_JOBS.with(|j| j.get().saturating_sub(1));
    POOL_JOBS.with(|j| j.set(left));
    pool_batch_finish_if_idle();
}

fn pool_batch_end() {
    let n = POOL_SCOPES.with(|s| s.get()).saturating_sub(1);
    POOL_SCOPES.with(|s| s.set(n));
    pool_batch_finish_if_idle();
}

/// Once the import has enqueued every RAW and the pool has drained, record the
/// batch mark and terminate the pool workers — their wasm memory never shrinks
/// and a batch develop measures ~450 MB per worker.
fn pool_batch_finish_if_idle() {
    if POOL_SCOPES.with(|s| s.get()) != 0 || POOL_JOBS.with(|j| j.get()) != 0 {
        return;
    }
    let files = POOL_BATCH_FILES.with(|f| f.replace(0));
    if files == 0 {
        return;
    }
    let start = POOL_BATCH_START.with(|s| s.replace(0.0));
    if start > 0.0 {
        perf::record_detail(
            "ingest.raw_batch",
            perf::now() - start,
            Some(format!("{files} files, pool {}", pworker::raw_pool_size())),
        );
    }
    pworker::raw_pool_shutdown();
}

/// Make a display proxy available. Non-RAW items use the ordinary full decode.
/// `develop` marks calls that want sensor pixels (selection, settled render) —
/// it queues the background develop; hover-only calls just load the JPEG.
fn ensure_proxy(state: AppState, id: usize, develop: bool) {
    let Some(photo) = get_photo(id) else { return };
    if !photo.raw {
        ensure_full(state, id, None);
        return;
    }
    if photo.full.is_some() {
        return;
    }
    touch_proxy(id);
    if proxy_in_mem(id) {
        if develop && !proxy_developed(id) {
            request_upgrade(state, id);
        }
        return;
    }
    if develop {
        request_upgrade(state, id);
    }
    if !pworker::proxy_worker_available() {
        // No worker: decode on the main thread rather than stall on a proxy
        // that can never upgrade.
        ensure_full(state, id, None);
        return;
    }
    PROXY_DECODE_WANT.with(|w| w.set(Some(id)));
    if !PROXY_INFLIGHT.with(|f| f.borrow_mut().insert(id)) {
        return;
    }
    spawn_local(async move {
        let decoded = decode_proxy_source(id, develop).await;
        PROXY_INFLIGHT.with(|f| f.borrow_mut().remove(&id));
        if let Some(d) = decoded {
            land_proxy(state, id, d);
        }
    });
}

/// Worker id + pixel dims to render from: sensor pixels when loaded, else the
/// display proxy.
fn render_source(id: usize, photo: &PhotoData) -> (usize, (usize, usize)) {
    if let Some((_, w, h)) = &photo.full {
        (id, (*w, *h))
    } else if let Some(p) = proxy_data(id) {
        (proxy_wid(id), (p.w, p.h))
    } else {
        (id, photo.full_dims)
    }
}

/// Drop an item from the filmstrip: revoke its object URLs and free its
/// cached pixels/video blob.
fn remove_item(state: AppState, item_id: usize) {
    let mut removed = None;
    state.items.update(|v| {
        if let Some(pos) = v.iter().position(|m| m.id == item_id) {
            let m = v.remove(pos);
            removed = Some((m.object_url, m.thumb_url, pos));
        }
    });
    if removed.is_some() {
        state.list_gen.update(|g| *g += 1);
    }
    if let Some((url, thumb, _)) = &removed {
        let _ = Url::revoke_object_url(url);
        let _ = Url::revoke_object_url(thumb);
    }
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.photos.remove(&item_id);
        c.photo_src.remove(&item_id);
        c.video_blobs.remove(&item_id);
        c.video_meta.remove(&item_id);
        c.raw_fallback.remove(&item_id);
    });
    LAST_USE.with(|l| l.borrow_mut().remove(&item_id));
    FULL_WAITERS.with(|w| w.borrow_mut().remove(&item_id));
    purge_proxy(item_id);
    pworker::unload(item_id);
    // An item removed before its first session save never reaches the
    // persist pass that purges `gone` records; clear its RAW/preview blobs
    // here so they can't leak storage.
    spawn_local(async move {
        if let Ok(db) = session::open_db().await {
            let _ = session::delete_raw_assets(&db, &[item_id]).await;
            let _ = session::delete_layer_pixels(&db, item_id).await;
        }
    });
    // Hover loupes clear on mouseleave, which never fires when the hovered
    // thumb has just left the DOM — drop any loupe showing the removed item.
    if state.loupe.get_untracked().map(|(id, ..)| id) == Some(item_id) {
        loupe_clear(state);
    }
    if state.canvas_loupe.get_untracked().map(|(id, ..)| id) == Some(item_id) {
        canvas_loupe_clear(state);
    }
    camera_loupe_clear(item_id);
    if state.selected.get_untracked() == Some(item_id) {
        let pos = removed.map(|(_, _, p)| p).unwrap_or(0);
        let next = state.items.with(|v| v.get(pos).or_else(|| v.last()).map(|m| m.id));
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

struct RawIngest {
    full: Option<(Pixels, usize, usize)>,
    preview: (Vec<u8>, usize, usize),
    dims: (usize, usize),
    object_url: String,
    thumb_url: String,
    exif: Option<Rc<Vec<u8>>>,
    taken: Option<f64>,
    stored: bool,
    /// Only populated when `stored` is false: the file bytes kept on the app
    /// thread because the worker can no longer reach them through IndexedDB.
    fallback: Option<Vec<u8>>,
}

/// Materialise a RAW source on the app thread. Used only on the fallback path
/// (the original could not be persisted), so a copy here is acceptable.
async fn raw_bytes_from_src(src: &JsValue) -> Option<Vec<u8>> {
    if src.is_instance_of::<Blob>() {
        web::blob_to_bytes(src.unchecked_ref()).await.ok()
    } else {
        Some(js_sys::Uint8Array::new(src).to_vec())
    }
}

/// Preview-first RAW ingest. Decode the embedded camera JPEG (no demosaic),
/// persist the original source and preview PNG to IndexedDB, and leave the
/// full develop for first open/export. Falls back to an eager develop when
/// the file has no usable preview or the source could not be stored.
async fn prepare_raw(id: usize, src: &JsValue, mtime: Option<f64>) -> Option<RawIngest> {
    let meta = raw_meta_async(src, 1024).await?;
    let taken = meta.taken.or(mtime);
    let exif = meta.exif_tiff.map(Rc::new);

    let (pp, pw2, ph2) = meta.preview?;
    let canvas = web::create_canvas(pw2 as u32, ph2 as u32);
    web::put_pixels(&canvas, &pp, pw2 as u32, ph2 as u32);
    let png = web::canvas_to_blob(&canvas, "image/png").await.ok()?;

    // The camera's full-size JPEG is the instant display proxy; storing it
    // lets a reload open editable at 3200px without touching the RAW.
    let camera = meta.camera_jpeg.map(|jpeg| {
        let orientation = meta.orientation;
        (jpeg, orientation, oriented_dims(meta.camera_w, meta.camera_h, orientation))
    });

    let stored = {
        let db = session::open_db().await.ok()?;
        let a = session::put_raw_src(&db, id, src).await;
        let b = session::put_preview(&db, id, png.as_ref()).await;
        let ok = a.is_ok() && b.is_ok();
        if ok {
            if let Some((jpeg, orientation, frame)) = &camera {
                let rec = camera_proxy_record(jpeg, *orientation, *frame);
                let _ = session::put_proxy(&db, id, &rec).await;
            }
        }
        ok
    };

    let full = if stored {
        None
    } else {
        web::log("RAW bytes not persisted; developing eagerly");
        raw_develop_async(id, src).await
    };
    let fallback = if stored { None } else { raw_bytes_from_src(src).await };
    let (dims, preview) = match &full {
        Some((fpx, fw, fh)) => ((*fw, *fh), fpx.downscale(*fw, *fh, 1024)),
        // Camera frame dims keep the hi-res target sized to the sensor, not
        // the small embedded preview rawler exposes.
        None => (
            camera.as_ref().map(|(_, _, f)| *f).unwrap_or((pw2, ph2)),
            (pp, pw2, ph2),
        ),
    };
    let object_url = Url::create_object_url_with_blob(&png).ok()?;
    let thumb_url = thumb_object_url(&preview.0, preview.1, preview.2).await;
    Some(RawIngest {
        full,
        preview,
        dims,
        object_url,
        thumb_url,
        exif,
        taken,
        stored,
        fallback,
    })
}

fn main() {
    console_error_panic_hook::set_once();
    perf::install();
    pworker::init();
    let _mount = perf::Span::new("app.mount");
    mount_to_body(|| view! { <App/> });
}

// --- ingestion ---------------------------------------------------------------

fn ingest_files(state: AppState, files: Vec<File>) {
    spawn_local(async move {
        pool_batch_begin();
        for file in files {
            let name = file.name();
            let mtime = Some(file.last_modified() / 1000.0);
            let id = state.next_id.get_untracked();
            state.next_id.set(id + 1);

            if file.type_().starts_with("video/") {
                state.busy.set(Some(format!("Loading {name}…")));
                let blob: Blob = file.into();
                let url = Url::create_object_url_with_blob(&blob).unwrap();
                let probe = perf::Span::new("ingest.decode").detail(name.clone());
                let probed = probe_video(&url).await;
                drop(probe);
                if let Some((dur, w, h)) = probed {
                    CACHE.with(|c| {
                        let mut c = c.borrow_mut();
                        c.video_blobs.insert(id, blob);
                        c.video_meta.insert(id, (dur, w, h));
                    });
                    let push = perf::Span::new("ingest.push").detail(name.clone());
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
                        taken: mtime,
                        drive_file_id: None,
                        drive_parent_id: None,
                        drive_baked: false,
                    });
                    drop(push);
                } else {
                    web::log("video probe failed");
                }
            } else if raw::is_raw_name(&name) {
                state.busy.set(Some(format!("Loading RAW {name}…")));
                let raw_len = file.size() as u64;
                let src: JsValue = file.clone().into();
                let decode = perf::Span::new("ingest.decode").detail(name.clone());
                let prepared = prepare_raw(id, &src, mtime).await;
                drop(decode);
                let Some(mut ing) = prepared else {
                    web::log(&format!("RAW decode failed for {name}"));
                    state.busy.set(None);
                    continue;
                };
                if let Some(b) = ing.fallback.take() {
                    CACHE.with(|c| c.borrow_mut().raw_fallback.insert(id, Rc::new(b)));
                }
                if let Some((full_rgba, fw, fh)) = &ing.full {
                    full_rgba.load_into_worker(id, *fw, *fh);
                    touch_full(id);
                }
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: ing.full,
                            full_dims: ing.dims,
                            preview: ing.preview,
                            dirty: false,
                            gen: 0,
                            raw: true,
                            raw_len,
                            raw_persisted: ing.stored,
                        }),
                    )
                });
                evict_full_lru();
                let pool_name = name.clone();
                let push = perf::Span::new("ingest.push").detail(name.clone());
                push_item(state, MediaItem {
                    id,
                    kind: MediaKind::Photo,
                    name,
                    object_url: ing.object_url,
                    thumb_url: ing.thumb_url,
                    width: ing.dims.0,
                    height: ing.dims.1,
                    edit: EditParams::default(),
                    layers: Vec::new(),
                    next_layer_id: 0,
                    exif: ing.exif,
                    taken: ing.taken,
                    drive_file_id: None,
                    drive_parent_id: None,
                    drive_baked: false,
                });
                drop(push);
                develop_proxy_pool(state, id, pool_name);
            } else {
                state.busy.set(Some(format!("Loading {name}…")));
                let mut blob: Blob = file.clone().into();
                let lname = name.to_lowercase();
                let is_jpeg = !is_heic(&file)
                    && (lname.ends_with(".jpg") || lname.ends_with(".jpeg"));
                let is_png = lname.ends_with(".png");
                let read = perf::Span::new("ingest.read").detail(name.clone());
                let exif_tiff = if is_jpeg || is_png {
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
                } else {
                    None
                };
                drop(read);
                let taken = exif_tiff.as_deref().and_then(exif::taken_from_tiff).or(mtime);
                let exif = exif_tiff.map(Rc::new);
                if is_heic(&file) {
                    state.busy.set(Some(format!("Converting HEIC {name}…")));
                    let decode = perf::Span::new("ingest.decode").detail(name.clone());
                    let converted = web::heic_to_jpeg(blob).await;
                    drop(decode);
                    match converted {
                        Ok(b) => blob = b,
                        Err(_) => {
                            state.busy.set(None);
                            continue;
                        }
                    }
                }
                let decode = perf::Span::new("ingest.decode").detail(name.clone());
                let loaded = web::load_image(&blob).await;
                drop(decode);
                let Ok((img, url)) = loaded else {
                    state.busy.set(None);
                    continue;
                };
                let (fw, fh) = (img.natural_width() as usize, img.natural_height() as usize);
                let preview_span = perf::Span::new("ingest.preview").detail(name.clone());
                let preview = downscale(&img, 1024);
                drop(preview_span);
                let thumb_span = perf::Span::new("ingest.thumb").detail(name.clone());
                let thumb_url = thumb_object_url(&preview.0, preview.1, preview.2).await;
                drop(thumb_span);
                CACHE.with(|c| {
                    let mut c = c.borrow_mut();
                    c.photo_src.insert(id, blob);
                    c.photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: None,
                            full_dims: (fw, fh),
                            preview,
                            dirty: false,
                            gen: 0,
                            raw: false,
                            raw_len: 0,
                            raw_persisted: true,
                        }),
                    );
                });
                let push = perf::Span::new("ingest.push").detail(name.clone());
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
                    taken,
                    drive_file_id: None,
                    drive_parent_id: None,
                    drive_baked: false,
                });
                drop(push);
            }
            state.busy.set(None);
        }
        pool_batch_end();
    });
}

/// Insert in capture-time order (taken, then name, then id) so the filmstrip
/// reads chronologically no matter which order async imports complete in.
fn push_item(state: AppState, item: MediaItem) {
    let id = item.id;
    let k = (item.taken.unwrap_or(f64::INFINITY), item.name.clone(), item.id);
    batch(|| {
        state.items.update(|v| {
            let pos = v
                .iter()
                .position(|m| {
                    m.taken
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&k.0)
                        .then_with(|| m.name.cmp(&k.1))
                        .then_with(|| m.id.cmp(&k.2))
                        == std::cmp::Ordering::Greater
                })
                .unwrap_or(v.len());
            v.insert(pos, item);
        });
        state.list_gen.update(|g| *g += 1);
        if state.selected.get_untracked().is_none() {
            state.selected.set(Some(id));
        }
    });
}

/// Import a photo/RAW already downloaded from Drive as raw bytes.
/// `drive_edited` is the file's pes_edited appProperty: baked pixels, so any
/// stored recipe for the id is ignored (it would double-apply).
async fn ingest_drive_bytes(
    state: AppState,
    drive_file_id: String,
    drive_parent_id: String,
    name: String,
    drive_edited: bool,
    bytes: Vec<u8>,
) {
    {
        let id = state.next_id.get_untracked();
        state.next_id.set(id + 1);

        // Params describe work-in-progress on the original pixels; a file the
        // app already flattened starts from identity.
        let (edit, drive_baked) = if drive_edited {
            (EditParams::default(), true)
        } else {
            let recipe = state.drive_edits.with(|m| m.get(&drive_file_id).cloned());
            match recipe.as_deref().and_then(state::edit_params_from_recipe) {
                Some(e) => {
                    if e != EditParams::default() {
                        state.drive_error.set(Some(format!("Edits restored for {name}")));
                    }
                    (e, false)
                }
                None => (EditParams::default(), false),
            }
        };

        if raw::is_raw_name(&name) {
            let raw_len = bytes.len() as u64;
            let Ok(blob) = web::bytes_to_blob(&bytes, "application/octet-stream") else {
                return;
            };
            drop(bytes);
            let src: JsValue = blob.into();
            let Some(mut ing) = prepare_raw(id, &src, None).await else {
                web::log(&format!("RAW decode failed for {name}"));
                return;
            };
            if let Some(b) = ing.fallback.take() {
                CACHE.with(|c| c.borrow_mut().raw_fallback.insert(id, Rc::new(b)));
            }
            if let Some((full_rgba, fw, fh)) = &ing.full {
                full_rgba.load_into_worker(id, *fw, *fh);
                touch_full(id);
            }
            CACHE.with(|c| {
                c.borrow_mut().photos.insert(
                    id,
                    Rc::new(PhotoData {
                        full: ing.full,
                        full_dims: ing.dims,
                        preview: ing.preview,
                        dirty: false,
                        gen: 0,
                        raw: true,
                        raw_len,
                        raw_persisted: ing.stored,
                    }),
                )
            });
            evict_full_lru();
            let pool_name = name.clone();
            push_item(state, MediaItem {
                id,
                kind: MediaKind::Photo,
                name,
                object_url: ing.object_url,
                thumb_url: ing.thumb_url,
                width: ing.dims.0,
                height: ing.dims.1,
                edit,
                layers: Vec::new(),
                next_layer_id: 0,
                exif: ing.exif,
                taken: ing.taken,
                drive_file_id: Some(drive_file_id),
                drive_parent_id: Some(drive_parent_id),
                drive_baked,
            });
            develop_proxy_pool(state, id, pool_name);
        } else {
            let lname = name.to_lowercase();
            let mime = if lname.ends_with(".png") { "image/png" } else { "image/jpeg" };
            let exif_tiff = if lname.ends_with(".png") {
                exif::tiff_from_png(&bytes)
            } else if lname.ends_with(".jpg") || lname.ends_with(".jpeg") {
                exif::tiff_from_jpeg(&bytes)
            } else {
                None
            };
            let taken = exif_tiff.as_deref().and_then(exif::taken_from_tiff);
            let exif = exif_tiff.map(Rc::new);
            let Ok(blob) = web::bytes_to_blob(&bytes, mime) else {
                return;
            };
            let Ok((img, url)) = web::load_image(&blob).await else {
                return;
            };
            let (fw, fh) = (img.natural_width() as usize, img.natural_height() as usize);
            let preview = downscale(&img, 1024);
            let thumb_url = thumb_object_url(&preview.0, preview.1, preview.2).await;
            CACHE.with(|c| {
                let mut c = c.borrow_mut();
                c.photo_src.insert(id, blob);
                c.photos.insert(
                    id,
                    Rc::new(PhotoData {
                        full: None,
                        full_dims: (fw, fh),
                        preview,
                        dirty: false,
                        gen: 0,
                        raw: false,
                        raw_len: 0,
                        raw_persisted: true,
                    }),
                );
            });
            push_item(state, MediaItem {
                id,
                kind: MediaKind::Photo,
                name,
                object_url: url,
                thumb_url,
                width: fw,
                height: fh,
                edit,
                layers: Vec::new(),
                next_layer_id: 0,
                exif,
                taken,
                drive_file_id: Some(drive_file_id),
                drive_parent_id: Some(drive_parent_id),
                drive_baked,
            });
        }
    }
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

/// The cropped view renders only its crop window, so that window can afford
/// native density far past the whole-frame cap.
const WINDOW_MAX_EDGE: usize = 4096;

struct HiCache {
    item_id: usize,
    edit_fp: u64,
    source_fp: u64,
    canvas: web_sys::HtmlCanvasElement,
    w: usize,
    h: usize,
}

/// Crop-window render for the cropped view: the canvas holds only the crop
/// region at display (up to native) density.
struct HiWindow {
    item_id: usize,
    edit_fp: u64,
    source_fp: u64,
    /// Crop rect bits (normalized x, y, w, h). The same normalized region is
    /// the same framing even when the source's pixel density changes, so a
    /// proxy→full swap keeps the old window on screen while it re-renders.
    crop: [u32; 4],
    canvas: web_sys::HtmlCanvasElement,
    w: usize,
    h: usize,
}

/// Which sharp frame currently backs the canvas, for mapping overlays from
/// preview working coordinates onto it.
enum Sharp {
    /// Whole working image from [`HiCache`].
    Whole { w: usize },
    /// Crop window from [`HiWindow`]; `cw` is the crop width in preview px.
    Window { w: usize, cw: usize },
}

fn crop_key(c: &state::CropRect) -> [u32; 4] {
    [c.x.to_bits(), c.y.to_bits(), c.w.to_bits(), c.h.to_bits()]
}

struct PreviewGeo {
    item_id: usize,
    gen: u64,
    rot90: u8,
    fine_bits: u32,
    pixels: Rc<Vec<u8>>,
    w: usize,
    h: usize,
}

thread_local! {
    static HI_CACHE: RefCell<Option<HiCache>> = const { RefCell::new(None) };
    /// Crop-window render for the cropped view (separate from the whole-frame
    /// cache: it holds a different region at a different density).
    static HI_WINDOW: RefCell<Option<HiWindow>> = const { RefCell::new(None) };
    /// Rotated/fine-angle preview, reused across colour-only renders. The
    /// identity transform is not cached — callers borrow the source directly.
    static PREVIEW_GEO: RefCell<Option<PreviewGeo>> = const { RefCell::new(None) };
    /// Generation guard for the debounced hi rebuild; only the latest fires.
    static HI_DEBOUNCE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Generation guard for an in-flight worker render; only the latest may
    /// land in HI_CACHE.
    static HI_REQ_GEN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Last nonzero canvas display need (CSS px × DPR). Reactive remounts
    /// briefly hand us a not-yet-inserted canvas with zero client size.
    static LAST_NEED: std::cell::Cell<(f64, f64)> = const { std::cell::Cell::new((0.0, 0.0)) };
    /// A trailing rAF pass is armed; further changes this frame only set DIRTY.
    static PREVIEW_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Changes arrived since the leading-edge paint; the trailing pass runs.
    static PREVIEW_DIRTY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Geometry applied to the 1024px preview, cached across renders.
fn preview_geometry_cached(
    item_id: usize,
    photo_gen: u64,
    preview: &(Vec<u8>, usize, usize),
    edit: &EditParams,
) -> Option<(Rc<Vec<u8>>, usize, usize)> {
    if edit.rot90 % 4 == 0 && edit.fine_angle.abs() <= 0.01 {
        return None;
    }
    let (rot90, fine_bits) = (edit.rot90, edit.fine_angle.to_bits());
    PREVIEW_GEO.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(g) = c.as_ref() {
            if g.item_id == item_id
                && g.gen == photo_gen
                && g.rot90 == rot90
                && g.fine_bits == fine_bits
            {
                return Some((g.pixels.clone(), g.w, g.h));
            }
        }
        let (p, w, h) = geometry(&preview.0, preview.1, preview.2, edit);
        let pixels = Rc::new(p.into_owned());
        *c = Some(PreviewGeo {
            item_id,
            gen: photo_gen,
            rot90,
            fine_bits,
            pixels: pixels.clone(),
            w,
            h,
        });
        Some((pixels, w, h))
    })
}

/// Edit identity for the hi caches: everything in the recipe and layer set
/// that changes rendered pixels EXCEPT the crop rect (crop only reframes the
/// blit) and the source generation (kept separate so a source swap can show
/// the stale frame while the new one renders).
fn edit_fingerprint(item: &MediaItem) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let e = &item.edit;
    h.write_u8(e.rot90);
    for v in [e.fine_angle, e.brightness, e.contrast, e.saturation, e.warmth, e.blur, e.grain] {
        h.write_u32(v.to_bits());
    }
    for v in [
        e.raw.temp,
        e.raw.tint,
        e.raw.exposure,
        e.raw.highlights,
        e.raw.shadows,
        e.raw.noise,
        e.raw.sharpen,
    ] {
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

/// Identity of the pixels a render was produced from: the preview generation
/// and the RAW proxy generation.
fn source_fingerprint(photo_gen: u64, proxy_gen: u64) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u64(photo_gen);
    h.write_u64(proxy_gen);
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
fn render_hi(item: &MediaItem, req_w: f64, req_h: f64) -> Option<HiCache> {
    let photo = get_photo(item.id)?;
    let edit_fp = edit_fingerprint(item);
    let source_fp = source_fingerprint(photo.gen, proxy_gen(item.id));
    let (fpix, fw, fh) = photo.full.as_ref()?;
    let src = fpix.as_cow();
    let (p, ww, wh) = geometry(&src, *fw, *fh, &item.edit);
    let (tw, th) = hi_target(req_w, req_h, ww, wh, HI_MAX_EDGE as u32);
    let mut buf = if (tw, th) != (ww, wh) {
        resample_pixels(&p, ww, wh, tw, th)
    } else {
        p.into_owned()
    };
    if !item.edit.raw.is_identity() {
        rawops::apply(&mut buf, tw, th, &item.edit.raw);
    }
    if item.edit.is_color_touched() {
        if let Some(sel) = &item.edit.selection {
            let mask = ops::selection_mask(sel, tw, th);
            color::apply_color_effects_masked(&mut buf, &mask, tw, th, &item.edit, item.id as u32);
        } else {
            color::apply_color_effects(&mut buf, tw, th, &item.edit, item.id as u32);
        }
    }
    let canvas = web::create_canvas(tw as u32, th as u32);
    web::put_pixels(&canvas, &buf, tw as u32, th as u32);
    composite_layers(&canvas, &item.layers, tw, th);
    Some(HiCache { item_id: item.id, edit_fp, source_fp, canvas, w: tw, h: th })
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

/// Largest centered exact-ratio box inside a crop box (preview and developed dims can drift).
fn aspect_fit(cx: usize, cy: usize, cw: usize, ch: usize, aspect: Aspect) -> (usize, usize, usize, usize) {
    let Some((rw, rh)) = aspect.ratio() else {
        return (cx, cy, cw, ch);
    };
    let r = rw as f64 / rh as f64;
    let by_h = (ch as f64 * r).round().max(1.0) as usize;
    let (nw, nh) = if by_h <= cw {
        (by_h, ch)
    } else {
        let by_w = ((cw as f64 / r).round().max(1.0) as usize).min(ch);
        (cw, by_w)
    };
    (cx + (cw - nw) / 2, cy + (ch - nh) / 2, nw, nh)
}

/// Render a photo through the full edit pipeline into an encoded blob.
/// Returns (blob, mime, filename).
async fn render_photo_export(
    state: AppState,
    item: &MediaItem,
) -> Option<(Blob, &'static str, String)> {
    let _total = perf::Span::new("export.total").detail(item.name.clone());
    let ensure = perf::Span::new("export.ensure_full");
    let ensured = ensure_full_async(state, item.id).await;
    drop(ensure);
    if !ensured {
        return None;
    }
    let photo = get_photo(item.id)?;
    let (pix, fw, fh) = photo.full.as_ref()?;
    // A full-res export must fit a ~4 GB box. An untouched unrotated frame is
    // put on the canvas straight from the cached buffer (a JS view for RAW
    // develops); a Rust copy only exists when geometry or colour edits need a
    // mutable frame, and it is dropped before crop/encode.
    let mut geometry_span = perf::Span::new("export.geometry");
    let plain_geometry = item.edit.rot90 % 4 == 0 && item.edit.fine_angle.abs() <= 0.01;
    let untouched = item.edit.raw.is_identity() && !item.edit.is_color_touched();
    let geometry = if plain_geometry {
        None
    } else {
        Some(pix.with_slice(|s| {
            let (p, w, h) = geometry(s, *fw, *fh, &item.edit);
            (p.into_owned(), w, h)
        }))
    };
    let (w, h) = match &geometry {
        Some((_, w, h)) => (*w, *h),
        None => (*fw, *fh),
    };
    geometry_span.set_detail(format!("{}x{}", w, h));
    drop(geometry_span);
    let color_span = perf::Span::new("export.color");
    let apply_color = |p: &mut [u8], w: usize, h: usize| {
        if !item.edit.raw.is_identity() {
            rawops::apply(p, w, h, &item.edit.raw);
        }
        if item.edit.is_color_touched() {
            if let Some(sel) = &item.edit.selection {
                let mask = ops::selection_mask(sel, w, h);
                color::apply_color_effects_masked(p, &mask, w, h, &item.edit, item.id as u32);
            } else {
                color::apply_color_effects(p, w, h, &item.edit, item.id as u32);
            }
        }
    };
    let edited: Option<Vec<u8>> = match geometry {
        Some((mut v, _, _)) => {
            if !untouched {
                apply_color(&mut v, w, h);
            }
            Some(v)
        }
        None if untouched => None,
        None => {
            let mut v = pix.as_cow().into_owned();
            apply_color(&mut v, w, h);
            Some(v)
        }
    };
    drop(color_span);

    // Composite onto the full geometry-corrected canvas, then crop+scale
    // straight into the export canvas (one resample instead of two).
    let mut encode_span = perf::Span::new("export.encode");
    let base = web::create_canvas(w as u32, h as u32);
    match &edited {
        Some(v) => web::put_pixels(&base, v, w as u32, h as u32),
        None => pix.put_on_canvas(&base, w as u32, h as u32),
    }
    drop(edited);
    composite_layers(&base, &item.layers, w, h);

    let (cx, cy, cw, ch) = crop_px(&item.edit, w, h);
    let (cx, cy, cw, ch) = aspect_fit(cx, cy, cw, ch, item.edit.aspect);
    // Photo exports keep the crop's native pixels; export_dims is the video cap.
    let (ew, eh) = (cw, ch);
    encode_span.set_detail(format!("{}x{}", ew, eh));
    let out = web::create_canvas(ew as u32, eh as u32);
    web::ctx2d(&out)
        .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
            &base, cx as f64, cy as f64, cw as f64, ch as f64,
            0.0, 0.0, ew as f64, eh as f64,
        )
        .unwrap();
    drop(base);
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
    drop(encode_span);
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

enum DriveSave {
    Saved,
    AlreadyThere,
}

/// Resolve (find-or-create) the "edits" subfolder under the granted root —
/// every new file the app writes to Drive lands there. Cached in state.
async fn edits_folder_id(state: AppState, token: &str, root_id: &str) -> Result<String, String> {
    if let Some(id) = state.drive_edits_folder.get_untracked() {
        return Ok(id);
    }
    let id = drive::ensure_child_folder(token, root_id, drive::EDITS_FOLDER_NAME)
        .await
        .map_err(|e| js_err(&e))?;
    state.drive_edits_folder.set(Some(id.clone()));
    Ok(id)
}

/// Upload one photo into the granted Drive folder's "edits" subfolder;
/// overwrites in place when the item came from Drive or was already saved
/// there. No busy indicator or panel refresh — callers own both (single save
/// and the batch loop differ).
async fn save_photo_to_drive_inner(state: AppState, item: MediaItem) -> Result<DriveSave, String> {
    let Some(token) = fresh_drive_token(state).await else {
        return Err("not signed in to Drive".into());
    };
    let Some((folder_id, _)) = state.drive_folder.get_untracked() else {
        return Err("no Drive folder granted".into());
    };
    let item_id = item.id;

    // Unedited JPEG/PNG: upload the source bytes untouched. If the item
    // came from Drive it is already byte-identical there — nothing to do.
    if photo_unedited(&item) {
        if let Some(mime) = passthru_mime(&item.name) {
            if item.drive_file_id.is_some() {
                return Ok(DriveSave::AlreadyThere);
            }
            if let Ok(bytes) = web::object_url_bytes(&item.object_url).await {
                let dest = edits_folder_id(state, &token, &folder_id).await?;
                return match drive::upload_file(&token, &dest, None, &item.name, mime, &bytes).await {
                    Ok(fid) => {
                        state.update_item_by_id(item_id, |m| {
                            m.drive_file_id = Some(fid);
                            m.drive_baked = true;
                        });
                        Ok(DriveSave::Saved)
                    }
                    Err(e) => Err(js_err(&e)),
                };
            }
            // Couldn't read the source back — fall through to re-encode.
        }
    }

    // New uploads land in the "edits" folder; PATCH-in-place (item already
    // in Drive) keeps the file where it is.
    let dest = match item.drive_file_id {
        Some(_) => None,
        None => Some(edits_folder_id(state, &token, &folder_id).await?),
    };
    let Some((blob, mime, filename)) = render_photo_export(state, &item).await else {
        return Err("render failed".into());
    };
    let result = async {
        let bytes = web::blob_to_bytes(&blob).await?;
        let parent = dest.as_deref().unwrap_or(&folder_id);
        let first = drive::upload_file(
            &token,
            parent,
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
            // original rejects PATCH. Fall back to a new copy in edits/.
            Err(e) => match item.drive_parent_id.as_deref() {
                Some(_) => {
                    let dest = match &dest {
                        Some(d) => d.clone(),
                        None => edits_folder_id(state, &token, &folder_id)
                            .await
                            .map_err(|e| JsValue::from_str(&e))?,
                    };
                    drive::upload_file(&token, &dest, None, &filename, mime, &bytes).await
                }
                None => Err(e),
            },
        }
    }
    .await;
    match result {
        Ok(fid) => {
            // PATCH (same id) means the recipe's pixels are now canonical:
            // drop the manifest entry so a reopen doesn't re-apply it.
            // A copy (new id) leaves the original's recipe untouched.
            let patched_in_place = item.drive_file_id.as_deref() == Some(fid.as_str());
            state.update_item_by_id(item_id, |m| {
                m.drive_file_id = Some(fid.clone());
                m.drive_parent_id = None;
                m.drive_baked = true;
            });
            if patched_in_place && state.drive_edits.with(|m| m.contains_key(&fid)) {
                state.drive_edits.update(|m| {
                    m.remove(&fid);
                });
                schedule_manifest_save(state);
            }
            Ok(DriveSave::Saved)
        }
        Err(e) => Err(js_err(&e)),
    }
}

/// Upload the rendered export into the granted Drive folder; overwrites in
/// place when the item came from Drive or was already saved there.
fn save_photo_to_drive(state: AppState, item: MediaItem) {
    spawn_local(async move {
        state.busy.set(Some(format!("Saving {} to Drive…", item.name)));
        state.drive_error.set(None);
        match save_photo_to_drive_inner(state, item.clone()).await {
            Ok(DriveSave::AlreadyThere) => state
                .drive_error
                .set(Some(format!("{} is unchanged — already in Drive", item.name))),
            Ok(DriveSave::Saved) => drive_refresh(state),
            Err(e) => state
                .drive_error
                .set(Some(format!("Drive save failed: {}", e))),
        }
        state.busy.set(None);
    });
}

/// True for photos that differ from what Drive holds: edited params, an
/// unsaved destructive op, or a local file never uploaded. This is the
/// "Save N edited to Drive" set.
fn photo_needs_save(m: &MediaItem) -> bool {
    m.kind == MediaKind::Photo
        && (!photo_unedited(m) || get_photo(m.id).map(|p| p.dirty).unwrap_or(false))
}

/// Sequential batch save of every photo that needs it; one panel refresh at
/// the end. Sequential because each upload re-encodes full-res pixels.
fn save_edited_to_drive(state: AppState) {
    spawn_local(async move {
        if state.drive_token.get_untracked().is_none()
            || state.drive_folder.get_untracked().is_none()
        {
            return;
        }
        let targets: Vec<MediaItem> =
            state.items.with(|v| v.iter().filter(|m| photo_needs_save(m)).cloned().collect());
        if targets.is_empty() {
            return;
        }
        state.drive_error.set(None);
        let n = targets.len();
        let mut failed: Vec<String> = Vec::new();
        for (i, item) in targets.into_iter().enumerate() {
            state.busy.set(Some(format!("Saving edited to Drive… ({}/{})", i + 1, n)));
            if let Err(e) = save_photo_to_drive_inner(state, item.clone()).await {
                failed.push(format!("{}: {}", item.name, e));
            }
        }
        state.busy.set(None);
        if !failed.is_empty() {
            state.drive_error.set(Some(format!(
                "Saved {}/{} — failed: {}",
                n - failed.len(),
                n,
                failed.join("; ")
            )));
        }
        drive_refresh(state);
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
        if let Some((out, filename)) = render_video_export(state, &item).await {
            web::download_blob(&out, &filename);
        }
        state.busy.set(None);
        state.progress.set(0.0);
    });
}

/// Transcode a video through the edit pipeline; returns (blob, filename).
async fn render_video_export(state: AppState, item: &MediaItem) -> Option<(Blob, String)> {
    let (blob, _dur, w, h) = get_video(item.id)?;
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
    let args = build_ffmpeg_args(item, w as usize, h as usize, &overlay_names);
    let on_progress = Closure::new(move |r: f64| state.progress.set(r as f32));
    match web::video_transcode(&item.name, blob, args, overlays, on_progress).await {
        Ok(out) => {
            let base = item.name.rsplitn(2, '.').last().unwrap_or("video");
            Some((out, format!("edited-{}.mp4", base)))
        }
        Err(e) => {
            web::log_err(&format!("transcode failed: {e:?}"));
            None
        }
    }
}

/// Batch-download every edited item packaged into a single zip. Entries are
/// STOREd — JPEG/PNG/MP4 are already compressed, so DEFLATE would buy
/// nothing.
fn download_edited_zip(state: AppState) {
    spawn_local(async move {
        let targets: Vec<MediaItem> =
            state.items.with(|v| v.iter().filter(|m| item_edited(m)).cloned().collect());
        if targets.is_empty() {
            return;
        }
        let n = targets.len();
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        for (i, item) in targets.into_iter().enumerate() {
            state.busy.set(Some(format!("Packaging zip… ({}/{})", i + 1, n)));
            let rendered = match item.kind {
                MediaKind::Photo => render_photo_export(state, &item)
                    .await
                    .map(|(b, _, f)| (b, f)),
                MediaKind::Video => render_video_export(state, &item).await,
            };
            if let Some((blob, filename)) = rendered {
                if let Ok(bytes) = web::blob_to_bytes(&blob).await {
                    entries.push((filename, bytes));
                }
            }
        }
        if !entries.is_empty() {
            state.busy.set(Some("Building zip…".into()));
            let bytes = zip::build_zip(&entries);
            if let Ok(blob) = web::bytes_to_blob(&bytes, "application/zip") {
                web::download_blob(&blob, "photo-edit-edited.zip");
            }
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

    // Restore the previous session from IndexedDB (no-op on first run), then
    // mirror every later state change back — debounced, diffed per item.
    restore_session(state);
    create_effect(move |_| {
        perf::record("session.effect", 0.0);
        state.items.track();
        state.selected.track();
        schedule_session_save(state);
    });

    // Load the selected photo's display pixels on demand: a RAW gets its
    // camera proxy first, then a background sensor develop; unselected photos
    // stay preview-only and their buffers are LRU-evicted.
    create_effect(move |_| {
        let Some(id) = state.selected.get() else { return };
        let is_photo = state
            .items
            .with(|v| v.iter().any(|m| m.id == id && m.kind == MediaKind::Photo));
        if is_photo {
            ensure_proxy(state, id, true);
        }
    });

    // Persist parametric edits of Drive-backed items into the manifest's
    // edits map (debounced). Baked items carry no recipe — their Drive copy
    // has the edits flattened into pixels. Reset to identity removes the
    // entry via the same path.
    create_effect(move |_| {
        let Some(item) = state.current() else { return };
        let Some(fid) = item.drive_file_id.clone() else { return };
        if item.drive_baked {
            return;
        }
        let recipe = state::edit_recipe_json(&item.edit);
        let stored = state.drive_edits.with(|m| m.get(&fid).cloned());
        if stored != recipe {
            state.drive_edits.update(|m| match &recipe {
                Some(r) => {
                    m.insert(fid.clone(), r.clone());
                }
                None => {
                    m.remove(&fid);
                }
            });
            schedule_manifest_save(state);
        }
    });

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

/// Normalized (0..1) cursor position within the thumb under the cursor.
/// Bubbling events are delegated in Leptos, so current_target is not the
/// thumb — resolve via target + closest instead.
/// Cursor position normalized inside the hovered ancestor matching `sel`.
/// (Leptos delegates bubbling events, so current_target is unusable.)
fn norm_pos_in(ev: &web_sys::MouseEvent, sel: &str) -> Option<(f64, f64)> {
    let el: web_sys::Element = ev.target()?.dyn_into().ok()?;
    let tile = el.closest(sel).ok().flatten()?;
    let r = tile.get_bounding_client_rect();
    Some((
        ((ev.client_x() as f64 - r.left()) / r.width()).clamp(0.0, 1.0),
        ((ev.client_y() as f64 - r.top()) / r.height()).clamp(0.0, 1.0),
    ))
}

fn thumb_norm_pos(ev: &web_sys::MouseEvent) -> Option<(f64, f64)> {
    norm_pos_in(ev, ".thumb")
}

/// CSS width of the hovered element matching `sel` — how large the image is
/// already shown at, which sets the magnification the loupe needs.
fn hovered_width(ev: &web_sys::MouseEvent, sel: &str) -> f64 {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|el| el.closest(sel).ok().flatten())
        .map(|el| el.get_bounding_client_rect().width())
        .unwrap_or(0.0)
}

/// Resting the cursor on a thumb opens the focus loupe after a short dwell,
/// so sweeping across the strip doesn't strobe full-res decodes.
fn loupe_soon(state: AppState, id: usize, ev: &web_sys::MouseEvent) {
    let Some((nx, ny)) = thumb_norm_pos(ev) else { return };
    let (cx, cy) = (ev.client_x() as f64, ev.client_y() as f64);
    state.loupe_gen.update(|g| *g += 1);
    let gen = state.loupe_gen.get_untracked();
    let cb = wasm_bindgen::closure::Closure::once(move || {
        if state.loupe_gen.get_untracked() == gen {
            ensure_proxy(state, id, false);
            state.loupe.set(Some((id, nx, ny, cx, cy)));
        }
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        250,
    );
    cb.forget();
}

fn loupe_move(state: AppState, id: usize, ev: &web_sys::MouseEvent) {
    let showing = state.loupe.with(|l| matches!(l, Some((lid, ..)) if *lid == id));
    if showing {
        let Some((nx, ny)) = thumb_norm_pos(ev) else { return };
        state
            .loupe
            .set(Some((id, nx, ny, ev.client_x() as f64, ev.client_y() as f64)));
    }
}

fn loupe_clear(state: AppState) {
    state.loupe_gen.update(|g| *g += 1);
    state.loupe.set(None);
}

// Scratch canvas for the loupe's pre-decode placeholder (native-res paints
// copy the 1:1 crop straight out of the cache instead, so hovering a 25MP
// photo never materializes a third full-size buffer). Keyed by item and a
// source version so a proxy upgrade rebuilds it.
thread_local! {
    static LOUPE_SRC: RefCell<Option<(usize, u64, web_sys::HtmlCanvasElement, u32, u32)>> =
        RefCell::new(None);
}

/// Filmstrip focus loupe: a floating 1:1 pixel window into the hovered thumb,
/// for judging focus/sharpness without selecting each item.
#[component]
fn FocusLoupe(state: AppState) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let tag = create_rw_signal("");

    create_effect(move |_| {
        state.items.track(); // repaint when a full-res buffer lands
        let Some((id, nx, ny, _, _)) = state.loupe.get() else {
            LOUPE_SRC.with(|s| *s.borrow_mut() = None);
            return;
        };
        let Some(canvas) = canvas_ref.get() else { return };
        let dpr = web::window().device_pixel_ratio().max(1.0);
        let bw = (480.0 * dpr) as usize;
        let bh = (360.0 * dpr) as usize;
        if canvas.width() != bw as u32 || canvas.height() != bh as u32 {
            canvas.set_width(bw as u32);
            canvas.set_height(bh as u32);
        }
        let Some(photo) = get_photo(id) else { return };
        let proxy = proxy_data(id);
        // Native-resolution crop copied row-wise out of whichever buffer the
        // item has: sensor pixels when loaded, else the display proxy.
        let copy_crop = |px: &Pixels, w: usize, h: usize| -> bool {
            if w < bw || h < bh {
                return false;
            }
            let sx = ((nx * w as f64 - bw as f64 / 2.0) as isize)
                .clamp(0, (w - bw) as isize) as usize;
            let sy = ((ny * h as f64 - bh as f64 / 2.0) as isize)
                .clamp(0, (h - bh) as isize) as usize;
            let crop = px.crop_rows(w, sy, sx, bw, bh);
            web::put_pixels(&canvas, &crop, bw as u32, bh as u32);
            true
        };
        if let Some((px, w, h)) = &photo.full {
            tag.set("1:1");
            if copy_crop(px, *w, *h) {
                return;
            }
        } else if let Some(p) = &proxy {
            tag.set("1:1 proxy");
            if copy_crop(&p.pixels, p.w, p.h) {
                return;
            }
        } else {
            tag.set("preview…");
        }
        // Placeholder path (preview-res, or image smaller than the loupe):
        // paint from a small scratch canvas with scaling.
        let ver = photo.gen ^ (proxy_gen(id) << 32);
        let rebuild = LOUPE_SRC.with(|s| {
            !matches!(&*s.borrow(), Some((sid, sver, ..)) if *sid == id && *sver == ver)
        });
        if rebuild {
            let scratch = match (&photo.full, &proxy) {
                (Some((px, w, h)), _) => {
                    let c = web::create_canvas(*w as u32, *h as u32);
                    px.put_on_canvas(&c, *w as u32, *h as u32);
                    (c, *w as u32, *h as u32)
                }
                (None, Some(p)) => {
                    let c = web::create_canvas(p.w as u32, p.h as u32);
                    p.pixels.put_on_canvas(&c, p.w as u32, p.h as u32);
                    (c, p.w as u32, p.h as u32)
                }
                (None, None) => {
                    let (px, w, h) = &photo.preview;
                    let c = web::create_canvas(*w as u32, *h as u32);
                    web::put_pixels(&c, px, *w as u32, *h as u32);
                    (c, *w as u32, *h as u32)
                }
            };
            LOUPE_SRC.with(|s| *s.borrow_mut() = Some((id, ver, scratch.0, scratch.1, scratch.2)));
        }
        LOUPE_SRC.with(|s| {
            let b = s.borrow();
            let Some((_, _, scratch, w, h)) = &*b else { return };
            let (w, h) = (*w as f64, *h as f64);
            let sw = (bw as f64).min(w);
            let sh = (bh as f64).min(h);
            let sx = (nx * w - sw / 2.0).clamp(0.0, (w - sw).max(0.0));
            let sy = (ny * h - sh / 2.0).clamp(0.0, (h - sh).max(0.0));
            let _ = web::ctx2d(&canvas)
                .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    scratch, sx, sy, sw, sh, 0.0, 0.0, bw as f64, bh as f64,
                );
        });
    });

    view! {
        <Show when=move || state.loupe.with(|l| l.is_some()) fallback=|| ()>
            <div
                class="loupe-hover"
                style=move || {
                    state
                        .loupe
                        .get()
                        .map(|(_, _, _, cx, cy)| loupe_style(cx, cy))
                        .unwrap_or_default()
                }
            >
                <canvas node_ref=canvas_ref width="480" height="360"></canvas>
                <span class="loupe-tag">{move || tag.get()}</span>
            </div>
        </Show>
    }
}

// --- editor-canvas hover loupe -------------------------------------------------

// Full-res *edited* render feeding the canvas loupe. Separate from HI_CACHE
// (display-sized) — the loupe wants native pixels, so its render targets the
// full working resolution (still HI_MAX_EDGE-capped) and runs in the worker.
thread_local! {
    static CANVAS_LOUPE_HI: RefCell<Option<HiCache>> = RefCell::new(None);
    static CANVAS_LOUPE_REQ: Cell<u32> = const { Cell::new(0) };
    /// Native-resolution camera JPEG for an item whose sensor pixels were not
    /// developed (lazy RAW). One entry — a 26MP frame is ~100MB of RGBA.
    static CAMERA_LOUPE: RefCell<Option<(usize, Rc<(Pixels, usize, usize)>)>> = RefCell::new(None);
    static CAMERA_LOUPE_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    /// Items with no usable camera JPEG (or no stored original) — don't
    /// re-read a 29MB blob on every repaint.
    static CAMERA_LOUPE_FAILED: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

fn camera_loupe_source(id: usize) -> Option<Rc<(Pixels, usize, usize)>> {
    CAMERA_LOUPE.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(cid, _)| *cid == id)
            .map(|(_, px)| px.clone())
    })
}

fn camera_loupe_drop(id: usize) {
    CAMERA_LOUPE.with(|c| {
        let hit = c.borrow().as_ref().map(|(cid, _)| *cid == id).unwrap_or(false);
        if hit {
            *c.borrow_mut() = None;
        }
    });
}

fn camera_loupe_clear(id: usize) {
    camera_loupe_drop(id);
    CAMERA_LOUPE_INFLIGHT.with(|s| {
        s.borrow_mut().remove(&id);
    });
    CAMERA_LOUPE_FAILED.with(|s| {
        s.borrow_mut().remove(&id);
    });
}

/// A destructive op baked new pixels for the item: its embedded JPEG no
/// longer describes what is on screen, so drop the decode and never serve
/// it again (the regenerated proxy covers the evicted-full case).
fn camera_loupe_invalidate(id: usize) {
    camera_loupe_drop(id);
    CAMERA_LOUPE_FAILED.with(|s| {
        s.borrow_mut().insert(id);
    });
}

/// Decode the camera's full-size embedded JPEG for the loupe. Import defers
/// the sensor develop, so without this the loupe would upscale the 3200px
/// display proxy. Reads the original from IndexedDB, keeps only the decoded
/// pixels, and lets the effect repaint when they land.
fn request_camera_loupe(state: AppState, id: usize) {
    if camera_loupe_source(id).is_some() {
        return;
    }
    if CAMERA_LOUPE_FAILED.with(|s| s.borrow().contains(&id)) {
        return;
    }
    // A sensor develop is running; its pixels are about to supersede the
    // camera JPEG, and adding ~100MB alongside it buys nothing.
    if FULL_INFLIGHT.with(|f| f.borrow().contains(&id)) {
        return;
    }
    if !CAMERA_LOUPE_INFLIGHT.with(|s| s.borrow_mut().insert(id)) {
        return;
    }
    spawn_local(async move {
        let decoded = async {
            let blob = session::load_raw_blob(id).await?;
            let meta = raw_meta_async(blob.as_ref(), 0).await?;
            let jpeg = meta.camera_jpeg?;
            // Below ~3k wide the largest JPEG is still a preview, not sensor
            // resolution.
            if meta.camera_w < 3000 {
                return None;
            }
            let (px, w, h) = camera_jpeg_rgba(&jpeg).await?;
            let (px, w, h) = raw::apply_orientation_code(px, w, h, meta.orientation);
            Some(Rc::new((Pixels::owned(px), w, h)))
        }
        .await;
        CAMERA_LOUPE_INFLIGHT.with(|s| {
            s.borrow_mut().remove(&id);
        });
        if let Some(px) = decoded {
            // The sensor develop landed while we were decoding; the loupe
            // reads those pixels now, so don't hold a second full frame.
            if !full_present(id) {
                CAMERA_LOUPE.with(|c| *c.borrow_mut() = Some((id, px)));
                state.items.update(|_| {});
            }
        } else {
            CAMERA_LOUPE_FAILED.with(|s| {
                s.borrow_mut().insert(id);
            });
        }
    });
}

/// Normalized working-image coords of a pointer over the editor canvas. The
/// wrap shows the crop region outside the Crop tab, so map through the crop.
fn canvas_loupe_norm(state: AppState, tab: Tab, ev: &web_sys::PointerEvent) -> Option<(f64, f64)> {
    let el: web_sys::Element = ev.target()?.dyn_into().ok()?;
    let wrap = el.closest(".canvas-wrap").ok().flatten()?;
    let r = wrap.get_bounding_client_rect();
    if r.width() < 1.0 || r.height() < 1.0 {
        return None;
    }
    let nx = ((ev.client_x() as f64 - r.left()) / r.width()).clamp(0.0, 1.0);
    let ny = ((ev.client_y() as f64 - r.top()) / r.height()).clamp(0.0, 1.0);
    let item = state.current()?;
    let crop = item.edit.crop;
    if tab != Tab::Crop && !crop_is_full(&crop) {
        Some((
            crop.x as f64 + nx * crop.w as f64,
            crop.y as f64 + ny * crop.h as f64,
        ))
    } else {
        Some((nx, ny))
    }
}

/// Resting the mouse on the editor canvas opens the 1:1 loupe after a dwell.
/// Re-armed on every move while hidden, so sweeps across the canvas never
/// strobe full-res renders — the loupe only opens once the cursor stops.
fn canvas_loupe_soon(state: AppState, tab: RwSignal<Tab>, ev: &web_sys::PointerEvent) {
    if ev.pointer_type() != "mouse" {
        return;
    }
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some((nx, ny)) = canvas_loupe_norm(state, tab.get_untracked(), ev) else { return };
    let (cx, cy) = (ev.client_x() as f64, ev.client_y() as f64);
    let id = item.id;
    state.canvas_loupe_gen.update(|g| *g += 1);
    let gen = state.canvas_loupe_gen.get_untracked();
    let cb = Closure::once(move || {
        if state.canvas_loupe_gen.get_untracked() == gen {
            ensure_proxy(state, id, false);
            state.canvas_loupe.set(Some((id, nx, ny, cx, cy)));
        }
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        250,
    );
    cb.forget();
}

fn canvas_loupe_move(state: AppState, tab: RwSignal<Tab>, ev: &web_sys::PointerEvent) {
    // Drags (heal/clone/crop/select/layers) never open the loupe.
    if ev.buttons() != 0 {
        canvas_loupe_clear(state);
        return;
    }
    if state.canvas_loupe.with(|l| l.is_some()) {
        let Some(item) = state.current() else { return };
        if let Some((nx, ny)) = canvas_loupe_norm(state, tab.get_untracked(), ev) {
            state.canvas_loupe.set(Some((
                item.id,
                nx,
                ny,
                ev.client_x() as f64,
                ev.client_y() as f64,
            )));
        }
    } else {
        canvas_loupe_soon(state, tab, ev);
    }
}

fn canvas_loupe_clear(state: AppState) {
    state.canvas_loupe_gen.update(|g| *g += 1);
    state.canvas_loupe.set(None);
}

/// Crop a hover window out of the sensor buffer and run the pointwise edit
/// pipeline over it at source density (raw develop → blur → levels → curves →
/// adjust → grain), mirroring render_pipeline. Local ops get a margin so the
/// cropped window's edges do not darken against the border.
fn source_window(
    px: &Pixels,
    fw: usize,
    fh: usize,
    sx: usize,
    sy: usize,
    cw: usize,
    ch: usize,
    edit: &EditParams,
    seed: u32,
) -> Option<Vec<u8>> {
    if cw == 0 || ch == 0 || sx + cw > fw || sy + ch > fh {
        return None;
    }
    let margin = (edit.blur / 100.0 * 8.0 * 3.0).ceil() as usize + 2;
    let x0 = sx.saturating_sub(margin);
    let y0 = sy.saturating_sub(margin);
    let x1 = (sx + cw + margin).min(fw);
    let y1 = (sy + ch + margin).min(fh);
    let (ew, eh) = (x1 - x0, y1 - y0);
    let mut win = px.crop_rows(fw, y0, x0, ew, eh);
    if !edit.raw.is_identity() {
        rawops::apply(&mut win, ew, eh, &edit.raw);
    }
    color::apply_color_effects(&mut win, ew, eh, edit, seed);
    let (ix, iy) = (sx - x0, sy - y0);
    let mut out = Vec::with_capacity(cw * ch * 4);
    for row in 0..ch {
        let start = ((iy + row) * ew + ix) * 4;
        out.extend_from_slice(&win[start..start + cw * 4]);
    }
    Some(out)
}

/// Draw an RGBA buffer into the loupe backing, scaled to fill it.
fn blit_window(canvas: &web_sys::HtmlCanvasElement, px: &[u8], w: u32, h: u32, bw: u32, bh: u32) {
    let scratch = web::create_canvas(w, h);
    web::put_pixels(&scratch, px, w, h);
    let ctx = web::ctx2d(canvas);
    ctx.set_image_smoothing_enabled(true);
    let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
        &scratch,
        0.0,
        0.0,
        w as f64,
        h as f64,
        0.0,
        0.0,
        bw as f64,
        bh as f64,
    );
}

/// Editor-canvas hover loupe: a floating 1:1 window into the *edited* image —
/// a full-res render through the whole pipeline, so it shows what an export
/// would. Untouched photos take the cheap source-pixel path instead.
#[component]
fn CanvasLoupe(
    state: AppState,
    tab: RwSignal<Tab>,
    main_canvas: NodeRef<html::Canvas>,
) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let tag = create_rw_signal(String::new());

    // Clear on any press inside the wrap. Capture phase: overlay children
    // (crop rect, selected text) stop propagation, so the wrap's delegated
    // on:pointerdown never sees those presses.
    let pd = Closure::wrap(Box::new(move |ev: web_sys::PointerEvent| {
        let hit = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest(".canvas-wrap").ok().flatten())
            .is_some();
        if hit {
            canvas_loupe_clear(state);
        }
    }) as Box<dyn FnMut(_)>);
    let pd_opts = web_sys::AddEventListenerOptions::new();
    pd_opts.set_capture(true);
    web::window()
        .add_event_listener_with_callback_and_add_event_listener_options(
            "pointerdown",
            pd.as_ref().unchecked_ref(),
            &pd_opts,
        )
        .ok();
    on_cleanup(move || {
        web::window()
            .remove_event_listener_with_callback_and_bool(
                "pointerdown",
                pd.as_ref().unchecked_ref(),
                true,
            )
            .ok();
    });

    create_effect(move |_| {
        state.items.track(); // edits, full-res decodes, render landings
        state.selected.track();
        let Some((id, nx, ny, _, _)) = state.canvas_loupe.get() else { return };
        let Some(item) = state.current() else {
            canvas_loupe_clear(state);
            return;
        };
        if item.id != id || item.kind != MediaKind::Photo {
            canvas_loupe_clear(state);
            return;
        }
        let Some(photo) = get_photo(id) else { return };
        let Some(canvas) = canvas_ref.get() else { return };
        let dpr = web::window().device_pixel_ratio().max(1.0);
        let bw = (480.0 * dpr) as usize;
        let bh = (360.0 * dpr) as usize;
        if canvas.width() != bw as u32 || canvas.height() != bh as u32 {
            canvas.set_width(bw as u32);
            canvas.set_height(bh as u32);
        }

        // Window size relative to what the screen already shows: magnify ~4x
        // over the main view, never downsampling below 1 device px per source
        // px. A fixed-size window stops being a closeup once the view is a
        // tight crop — it would frame about the same region as the canvas.
        let (ww, wh) = working_dims(photo.full_dims.0, photo.full_dims.1, &item.edit);
        let cropped_view = tab.get() != Tab::Crop && !crop_is_full(&item.edit.crop);
        let crop = item.edit.crop;
        let (vx, vy, vw, vh) = if cropped_view {
            (
                crop.x as f64 * ww as f64,
                crop.y as f64 * wh as f64,
                (crop.w as f64 * ww as f64).max(1.0),
                (crop.h as f64 * wh as f64).max(1.0),
            )
        } else {
            (0.0, 0.0, ww as f64, wh as f64)
        };
        let disp_css_w = main_canvas
            .get()
            .map(|c| c.client_width() as f64)
            .filter(|w| *w > 1.0)
            .unwrap_or(480.0);
        let zoom = (4.0 * disp_css_w / vw).max(1.0 / dpr).min(4.0);
        let cw = ((bw as f64 / (dpr * zoom)).round() as usize).max(1).min(ww);
        let ch = ((bh as f64 / (dpr * zoom)).round() as usize).max(1).min(wh);
        let mag = (dpr * zoom).round().max(1.0) as u32;
        let sx = ((nx * ww as f64 - cw as f64 / 2.0) as isize)
            .clamp(0, (ww - cw) as isize) as usize;
        let sy = ((ny * wh as f64 - ch as f64 / 2.0) as isize)
            .clamp(0, (wh - ch) as isize) as usize;

        // Direct path: crop the window straight out of the sensor buffer and
        // run the pointwise pipeline over it, so the loupe shows true source
        // pixels instead of a slice of a whole-image render. Exact only with
        // identity geometry and no selection or visible layers.
        let plain = item.edit.rot90 % 4 == 0
            && item.edit.fine_angle.abs() <= 0.01
            && item.edit.selection.is_none()
            && !item.layers.iter().any(|l| l.visible && l.opacity > 0.01);
        if plain {
            if let Some((px, fw, fh)) = photo.full.as_ref() {
                if let Some(win) =
                    source_window(px, *fw, *fh, sx, sy, cw, ch, &item.edit, id as u32)
                {
                    blit_window(&canvas, &win, cw as u32, ch as u32, bw as u32, bh as u32);
                    tag.set(format!("{mag}:1"));
                    return;
                }
            }
            if photo.raw {
                // The sensor develop is deferred on import, so without this
                // the loupe would upscale the 3200px display proxy. The
                // camera's own full-size JPEG is one decode away and covers
                // the same frame.
                request_camera_loupe(state, id);
                if let Some(cam) = camera_loupe_source(id) {
                    let (kx, ky) = (cam.1 as f64 / ww as f64, cam.2 as f64 / wh as f64);
                    let px = ((sx as f64 * kx).round() as usize).min(cam.1.saturating_sub(1));
                    let py = ((sy as f64 * ky).round() as usize).min(cam.2.saturating_sub(1));
                    let pw = ((cw as f64 * kx).round() as usize).max(1).min(cam.1 - px);
                    let phh = ((ch as f64 * ky).round() as usize).max(1).min(cam.2 - py);
                    if let Some(win) =
                        source_window(&cam.0, cam.1, cam.2, px, py, pw, phh, &item.edit, id as u32)
                    {
                        blit_window(&canvas, &win, pw as u32, phh as u32, bw as u32, bh as u32);
                        tag.set(format!("{mag}:1 camera"));
                        return;
                    }
                }
            }
            if let Some(p) = proxy_data(id) {
                // The proxy covers the same frame at reduced density: map the
                // window into proxy pixels and let the blit upscale it.
                let (kx, ky) = (p.w as f64 / ww as f64, p.h as f64 / wh as f64);
                let px = ((sx as f64 * kx).round() as usize).min(p.w.saturating_sub(1));
                let py = ((sy as f64 * ky).round() as usize).min(p.h.saturating_sub(1));
                let pw = ((cw as f64 * kx).round() as usize).max(1).min(p.w - px);
                let phh = ((ch as f64 * ky).round() as usize).max(1).min(p.h - py);
                if let Some(win) = source_window(
                    &p.pixels, p.w, p.h, px, py, pw, phh, &item.edit, id as u32,
                ) {
                    blit_window(&canvas, &win, pw as u32, phh as u32, bw as u32, bh as u32);
                    tag.set(format!("{mag}:1 proxy"));
                    return;
                }
            }
        }

        let edit_fp = edit_fingerprint(&item);
        let source_fp = source_fingerprint(photo.gen, proxy_gen(id));
        let hit = CANVAS_LOUPE_HI.with(|c| {
            let b = c.borrow();
            let hi = b.as_ref()?;
            (hi.item_id == id && hi.edit_fp == edit_fp && hi.source_fp == source_fp)
                .then(|| (hi.canvas.clone(), hi.w, hi.h))
        });
        if let Some((hi_canvas, hw, hh)) = hit {
            // The cached render covers the whole working frame; map the
            // window through its scale, then magnify it into the loupe.
            let kx = hw as f64 / ww as f64;
            let ky = hh as f64 / wh as f64;
            let ctx = web::ctx2d(&canvas);
            ctx.set_image_smoothing_enabled(true);
            let _ = ctx
                .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                    &hi_canvas,
                    sx as f64 * kx,
                    sy as f64 * ky,
                    cw as f64 * kx,
                    ch as f64 * ky,
                    0.0,
                    0.0,
                    bw as f64,
                    bh as f64,
                );
            // Whole-image renders are capped, so past that cap this is not
            // sensor 1:1 — say so rather than overstate the detail.
            let full_density = hw >= ww && hh >= wh;
            tag.set(if full_density {
                format!("{mag}:1")
            } else {
                format!("{mag}:1 preview")
            });
            return;
        }

        // Placeholder until the full decode or the rendered window lands:
        // show the same framing, magnified out of the on-screen canvas. Its
        // backing shows the visible region (the crop, outside the Crop tab).
        tag.set(String::from("rendering…"));
        if let Some(main) = main_canvas.get() {
            let (mw, mh) = (main.width() as f64, main.height() as f64);
            if mw > 1.0 && mh > 1.0 {
                let kx = mw / vw;
                let ky = mh / vh;
                let dx = (sx as f64 - vx) * kx;
                let dy = (sy as f64 - vy) * ky;
                let x0 = dx.max(0.0).min(mw);
                let y0 = dy.max(0.0).min(mh);
                let x1 = (dx + cw as f64 * kx).max(0.0).min(mw);
                let y1 = (dy + ch as f64 * ky).max(0.0).min(mh);
                if x1 > x0 && y1 > y0 {
                    let ctx = web::ctx2d(&canvas);
                    ctx.set_image_smoothing_enabled(true);
                    let _ = ctx
                        .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                            &main, x0, y0, x1 - x0, y1 - y0, 0.0, 0.0, bw as f64, bh as f64,
                        );
                }
            }
        }

        // Kick the full-res edited render. Every kick invalidates the one
        // before it; the store nudges items, re-running this effect.
        if !full_present(id) && !proxy_in_mem(id) {
            ensure_proxy(state, id, true);
            return;
        }
        CANVAS_LOUPE_REQ.with(|c| c.set(c.get() + 1));
        let rgen = CANVAS_LOUPE_REQ.with(|c| c.get());
        // Size from the source actually being rendered — a proxy render must
        // not be asked for sensor dimensions it can never produce.
        let (src_id, (sw, sh)) = render_source(id, &photo);
        let (fw, fh) = working_dims(sw, sh, &item.edit);
        let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
        let sel = item.edit.selection.clone();
        let item_cb = item.clone();
        let accepted = pworker::render(
            src_id,
            &recipe,
            fw as f64,
            fh as f64,
            HI_MAX_EDGE as u32,
            sel.as_ref(),
            move |reply| {
                if CANVAS_LOUPE_REQ.with(|c| c.get()) != rgen {
                    return;
                }
                let hi = match reply {
                    Ok(r) => {
                        let c = web::create_canvas(r.w, r.h);
                        web::put_pixels_view(&c, &r.pixels, r.w, r.h);
                        composite_layers(&c, &item_cb.layers, r.w as usize, r.h as usize);
                        Some(HiCache {
                            item_id: id,
                            edit_fp,
                            source_fp,
                            canvas: c,
                            w: r.w as usize,
                            h: r.h as usize,
                        })
                    }
                    Err(_) => render_hi(&item_cb, fw as f64, fh as f64),
                };
                if let Some(hi) = hi {
                    CANVAS_LOUPE_HI.with(|c| *c.borrow_mut() = Some(hi));
                    state.items.update(|_| {});
                }
            },
        );
        if !accepted {
            if let Some(hi) = render_hi(&item, fw as f64, fh as f64) {
                CANVAS_LOUPE_HI.with(|c| *c.borrow_mut() = Some(hi));
                state.items.update(|_| {});
            }
        }
    });

    view! {
        <Show when=move || state.canvas_loupe.with(|l| l.is_some()) fallback=|| ()>
            <div
                class="loupe-hover"
                style=move || {
                    state
                        .canvas_loupe
                        .get()
                        .map(|(_, _, _, cx, cy)| loupe_style(cx, cy))
                        .unwrap_or_default()
                }
            >
                <canvas node_ref=canvas_ref width="480" height="360"></canvas>
                <span class="loupe-tag">{move || tag.get()}</span>
            </div>
        </Show>
    }
}

/// Drive grid hover popup: w800 thumbnail on dwell, upgrading to a panning
/// crop once the original has been downloaded and decoded. RAW uses the
/// embedded preview by default; after "Full res" the crop switches to sensor
/// pixels. Mirrors FocusLoupe: <Show> keeps the canvas mounted, paints happen
/// in the effect, and the crop is row-copied out of the cache buffer.
#[component]
fn DriveHoverLoupe(state: AppState) -> impl IntoView {
    let canvas_ref = create_node_ref::<html::Canvas>();
    let tag = create_rw_signal(String::from("preview…"));
    let canvas_on = create_rw_signal(false);

    create_effect(move |_| {
        let hover = state.drive_hover.get();
        let ready = state.drive_hover_ready.get();
        let full_ready = state.drive_full_ready.get();
        let Some((id, nx, ny, _, _, disp_w)) = hover else { return };
        let Some(canvas) = canvas_ref.get() else { return };
        let sensor = full_ready.as_deref() == Some(id.as_str());
        let full = if sensor {
            DRIVE_FULL_CACHE.with(|c| {
                c.borrow()
                    .as_ref()
                    .and_then(|(fid, d)| (fid == &id).then(|| d.clone()))
            })
        } else {
            None
        };
        let (full, sensor) = match full {
            Some(d) => (Some(d), true),
            None => (
                if ready.as_deref() == Some(id.as_str()) {
                    DRIVE_HOVER_CACHE.with(|c| c.borrow().1.get(&id).cloned())
                } else {
                    None
                },
                false,
            ),
        };
        let Some(full) = full else {
            tag.set(String::from("preview…"));
            canvas_on.set(false);
            return;
        };
        let (px, w, h) = &*full;
        let (w, h) = (*w, *h);
        let dpr = web::window().device_pixel_ratio().max(1.0);
        let bw = (480.0 * dpr) as usize;
        let bh = (360.0 * dpr) as usize;
        if canvas.width() != bw as u32 || canvas.height() != bh as u32 {
            canvas.set_width(bw as u32);
            canvas.set_height(bh as u32);
        }
        // Magnify to ~4x the size the hovered image is already shown at, but
        // never below physical 1:1: a 1:1 crop of a small embedded RAW preview
        // looks no bigger than the popup it sits on, which is useless for
        // judging focus.
        let zoom = (4.0 * disp_w / w as f64).max(1.0 / dpr).min(4.0);
        let cw = ((bw as f64 / (dpr * zoom)).round() as usize).max(1);
        let ch = ((bh as f64 / (dpr * zoom)).round() as usize).max(1);
        if w >= cw && h >= ch {
            let mag = (dpr * zoom).round().max(1.0) as u32;
            // RAW defaults to the embedded preview, which is not sensor
            // resolution — say so, so the tag can't pass for pixel-level
            // sharpness when it is actually a resampled JPEG.
            let preview_src =
                !sensor && DRIVE_HOVER_PREVIEW.with(|s| s.borrow().contains(&id));
            tag.set(if preview_src {
                format!("{mag}:1 preview")
            } else {
                format!("{mag}:1")
            });
            canvas_on.set(true);
            let sx = ((nx * w as f64 - cw as f64 / 2.0) as isize)
                .clamp(0, (w - cw) as isize) as usize;
            let sy = ((ny * h as f64 - ch as f64 / 2.0) as isize)
                .clamp(0, (h - ch) as isize) as usize;
            let crop = px.crop_rows(w, sy, sx, cw, ch);
            let scratch = web::create_canvas(cw as u32, ch as u32);
            web::put_pixels(&scratch, &crop, cw as u32, ch as u32);
            let ctx = web::ctx2d(&canvas);
            ctx.set_image_smoothing_enabled(true);
            let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                &scratch,
                0.0,
                0.0,
                cw as f64,
                ch as f64,
                0.0,
                0.0,
                bw as f64,
                bh as f64,
            );
        } else {
            tag.set(String::from("preview"));
            canvas_on.set(true);
            // Image smaller than the loupe: scale-to-fit via a scratch canvas.
            let scratch = web::create_canvas(w as u32, h as u32);
            px.put_on_canvas(&scratch, w as u32, h as u32);
            let scale = (bw as f64 / w as f64).min(bh as f64 / h as f64);
            let dw = w as f64 * scale;
            let dh = h as f64 * scale;
            let ctx = web::ctx2d(&canvas);
            ctx.set_fill_style_str("#000");
            ctx.fill_rect(0.0, 0.0, bw as f64, bh as f64);
            let _ = ctx.draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                &scratch,
                0.0,
                0.0,
                w as f64,
                h as f64,
                (bw as f64 - dw) / 2.0,
                (bh as f64 - dh) / 2.0,
                dw,
                dh,
            );
        }
    });

    view! {
        <Show when=move || state.drive_hover.with(|h| h.is_some()) fallback=|| ()>
            <div
                class="drive-hover"
                style=move || {
                    state
                        .drive_hover
                        .get()
                        .map(|(_, _, _, cx, cy, _)| drive_hover_style(cx, cy))
                        .unwrap_or_default()
                }
            >
                <img
                    src=move || {
                        state
                            .drive_hover
                            .with(|h| {
                                h.as_ref()
                                    .map(|(id, ..)| drive::thumb_url_sz(id, 800))
                                    .unwrap_or_default()
                            })
                    }
                    decoding="async"
                />
                <canvas
                    node_ref=canvas_ref
                    width="480"
                    height="360"
                    style:display=move || if canvas_on.get() { "block" } else { "none" }
                ></canvas>
                <span class="loupe-tag">{move || tag.get()}</span>
            </div>
        </Show>
    }
}

/// Fixed-position style for the loupe panel, kept inside the viewport.
fn loupe_style(x: f64, y: f64) -> String {
    let w = 496.0_f64;
    let h = 398.0_f64;
    let vw = web::window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1280.0);
    let vh = web::window().inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
    let left = if x + 24.0 + w > vw { (x - w - 14.0).max(8.0) } else { x + 24.0 };
    let top = (y - h / 2.0).max(8.0).min((vh - h - 8.0).max(8.0));
    format!("left:{left:.0}px;top:{top:.0}px")
}

/// True when an item carries any visible edit: adjusted params, crop/rotate,
/// trim, annotation layers, an unsaved destructive op, or a Drive copy that
/// already has edits baked in.
fn item_edited(m: &MediaItem) -> bool {
    let dirty = get_photo(m.id).map(|p| p.dirty).unwrap_or(false);
    m.drive_baked
        || dirty
        || match m.kind {
            MediaKind::Photo => !photo_unedited(m),
            MediaKind::Video => {
                let e = &m.edit;
                e.trim.is_some()
                    || !e.keep_audio
                    || e.crop != state::CropRect::default()
                    || e.rot90 != 0
                    || e.fine_angle != 0.0
                    || e.aspect != Aspect::Original
            }
        }
}

#[component]
fn FilmStrip(state: AppState) -> impl IntoView {
    view! {
        <div class="filmstrip">
            <For
                each=move || {
                    perf::record("filmstrip.each", 0.0);
                    // Membership only: edit ticks touch `items` but must not
                    // rebuild this list.
                    state.list_gen.track();
                    state.items.get_untracked()
                }
                key=|m| m.id
                children=move |m: MediaItem| {
                    let id = m.id;
                    let is_photo = m.kind == MediaKind::Photo;
                    let badge = if m.kind == MediaKind::Video { "▶" } else { "" };
                    view! {
                        <div
                            class="thumb"
                            title=m.name.clone()
                            class:selected=move || state.selected.get() == Some(id)
                            on:click=move |_| state.selected.set(Some(id))
                            on:mouseenter=move |ev| {
                                if is_photo {
                                    loupe_soon(state, id, &ev);
                                }
                            }
                            on:mousemove=move |ev| loupe_move(state, id, &ev)
                            on:mouseleave=move |_| loupe_clear(state)
                        >
                            <img src=m.thumb_url.clone()/>
                            <span class="badge">{badge}</span>
                            {move || {
                                let edited = state.items.with(|v| {
                                    v.iter().find(|m| m.id == id).map(item_edited).unwrap_or(false)
                                });
                                edited.then(|| {
                                    view! { <span class="badge thumb-edited" title="Has edits">"✎"</span> }
                                })
                            }}
                            {move || {
                                // Read live: an item gains its drive id after
                                // the first successful save, and this tile is
                                // not rebuilt on item-content changes.
                                let did = state.items.with(|v| {
                                    v.iter().find(|m| m.id == id).and_then(|m| m.drive_file_id.clone())
                                })?;
                                if !state.drive_shortlist.with(|s| s.contains(&did)) {
                                    return None;
                                }
                                Some(view! {
                                    <button
                                        class="thumb-star"
                                        title="Starred on Drive — click to unstar (removes it here)"
                                        on:click=move |ev| {
                                            ev.stop_propagation();
                                            toggle_star(state, did.clone());
                                        }
                                    >
                                        "★"
                                    </button>
                                })
                            }}
                        </div>
                    }
                }
            />
        </div>
        <FocusLoupe state=state/>
    }
}

/// Folder currently being browsed: deepest breadcrumb, or the granted root.
fn drive_current_folder(state: AppState) -> Option<(String, String)> {
    if let Some(last) = state.drive_path.get_untracked().last() {
        return Some(last.clone());
    }
    state.drive_folder.get_untracked()
}

/// Current token, refreshed silently when it is within a minute of expiry.
/// GIS access tokens last ~1h; without this every Drive call in a long
/// editing session starts failing with HTTP 401.
async fn fresh_drive_token(state: AppState) -> Option<Rc<String>> {
    let token = state.drive_token.get_untracked()?;
    if state.drive_token_expiry.get_untracked() > js_sys::Date::now() + 60_000.0 {
        return Some(token);
    }
    match drive::sign_in().await {
        Ok(auth) => {
            let fresh = Rc::new(auth.token);
            state.drive_token.set(Some(fresh.clone()));
            state.drive_token_expiry.set(auth.expires_at_ms);
            Some(fresh)
        }
        Err(e) => {
            state.drive_error.set(Some(format!(
                "Drive session expired — sign in again ({})",
                js_err(&e)
            )));
            None
        }
    }
}

fn drive_refresh(state: AppState) {
    spawn_local(async move {
        let Some(token) = fresh_drive_token(state).await else {
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
        // The manifest lives at the granted root regardless of which
        // subfolder is being browsed. On the first load of a session the
        // remote manifest is authoritative, but local changes made before the
        // fetch landed (restored session recipes, a quick star) overlay it —
        // union here and let the gated writer converge remote afterwards.
        // After that, while a debounced write is outstanding (or a change
        // landed mid-fetch, moving save_gen) the remote copy is stale —
        // local state wins and the pending writer overwrites remote.
        let gen_at_read = state.drive_save_gen.get_untracked();
        match drive::find_manifest(&token, &root_id).await {
            Ok(Some(mid)) => {
                state.drive_manifest_id.set(Some(mid.clone()));
                if let Ok(bytes) = drive::download_file(&token, &mid).await {
                    let (starred, edits) = drive::parse_manifest(&bytes);
                    if !state.drive_manifest_loaded.get_untracked() {
                        state.drive_shortlist.update(|s| s.extend(starred));
                        state.drive_edits.update(|m| {
                            for (k, v) in edits {
                                m.entry(k).or_insert(v);
                            }
                        });
                        state.drive_manifest_loaded.set(true);
                    } else {
                        let fresh = !state.drive_save_pending.get_untracked()
                            && state.drive_save_gen.get_untracked() == gen_at_read;
                        if fresh {
                            state.drive_shortlist.set(starred);
                            state.drive_edits.set(edits);
                        }
                    }
                }
            }
            Ok(None) => {
                state.drive_manifest_id.set(None);
                if !state.drive_manifest_loaded.get_untracked() {
                    // No manifest on Drive yet — local state (if any) becomes
                    // the first manifest via the pending/next writer.
                    state.drive_manifest_loaded.set(true);
                } else {
                    let fresh = !state.drive_save_pending.get_untracked()
                        && state.drive_save_gen.get_untracked() == gen_at_read;
                    if fresh {
                        state.drive_shortlist.set(Default::default());
                        state.drive_edits.set(Default::default());
                    }
                }
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

/// Debounced manifest write (starred ids + edit recipes): only the latest
/// generation actually saves, so slider drags coalesce into one upload.
/// Reads both maps at fire time.
fn schedule_manifest_save(state: AppState) {
    state.drive_save_gen.update(|g| *g += 1);
    state.drive_save_pending.set(true);
    let gen = state.drive_save_gen.get_untracked();
    manifest_save_attempt(state, gen, 0);
}

fn manifest_save_attempt(state: AppState, gen: u32, attempt: u32) {
    let cb = wasm_bindgen::closure::Closure::once(move || {
        spawn_local(async move {
            if state.drive_save_gen.get_untracked() != gen {
                // Superseded by a newer change; that write owns the flag.
                return;
            }
            let Some(token) = fresh_drive_token(state).await else {
                state.drive_save_pending.set(false);
                return;
            };
            let Some((root_id, _)) = state.drive_folder.get_untracked() else {
                state.drive_save_pending.set(false);
                return;
            };
            // Never write before the manifest's first authoritative load —
            // the shortlist is empty until then, so an early write would
            // clobber every star on Drive. Re-arm while the load is in
            // flight; give up (loudly) if it never lands.
            if !state.drive_manifest_loaded.get_untracked() {
                if attempt >= 40 {
                    state.drive_save_pending.set(false);
                    state
                        .drive_error
                        .set(Some("Shortlist not saved — the Drive manifest never loaded".into()));
                    return;
                }
                manifest_save_attempt(state, gen, attempt + 1);
                return;
            }
            let ids = state.drive_shortlist.get_untracked();
            let edits = state.drive_edits.get_untracked();
            let body = drive::manifest_json(&ids, &edits);
            let existing = state.drive_manifest_id.get_untracked();
            let mut result = drive::upload_file(
                &token,
                &root_id,
                existing.as_deref(),
                drive::MANIFEST_NAME,
                "application/json",
                &body,
            )
            .await;
            // A token revoked or expired mid-request (clock skew, Google
            // dropping it early) still gets one silent re-auth and retry.
            if result
                .as_ref()
                .err()
                .map(|e| js_err(e).contains("HTTP 401"))
                .unwrap_or(false)
            {
                if let Ok(auth) = drive::sign_in().await {
                    let retry = Rc::new(auth.token);
                    state.drive_token.set(Some(retry.clone()));
                    state.drive_token_expiry.set(auth.expires_at_ms);
                    result = drive::upload_file(
                        &retry,
                        &root_id,
                        existing.as_deref(),
                        drive::MANIFEST_NAME,
                        "application/json",
                        &body,
                    )
                    .await;
                }
            }
            // Remote now reflects local; refreshes may apply it again.
            state.drive_save_pending.set(false);
            match result {
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

// --- session persistence (IndexedDB) -------------------------------------------
//
// The whole working set is mirrored into IndexedDB so any reload — including
// the service worker's update reload — restores it: item bytes, parametric
// edits (same recipe JSON as the Drive manifest), layers, order, selection.
// Bytes get their own sub-signature so a slider tick never rewrites a
// multi-MB blob: a plain photo's source is keyed by object URL, a RAW source
// by length, a healed photo's pixels by cache generation (re-encoded to a
// full-res PNG only when the generation moves).

#[derive(Clone, PartialEq)]
enum BytesSig {
    /// Plain photo: (object_url, cache gen). gen > 0 means heal/clone baked
    /// new pixels — the stored bytes are a full-res PNG of the cache.
    Photo(String, u64),
    /// RAW import: (raw byte len, cache gen).
    Raw(u64, u64),
    /// Video: blob byte len.
    Video(u64),
}

#[derive(Clone, PartialEq)]
struct ItemSig {
    bytes: BytesSig,
    name: String,
    w: usize,
    h: usize,
    ord: usize,
    recipe: Option<String>,
    layers: Option<String>,
    /// Raster layer ids whose pixel PNGs are already stored.
    raster_layers: Vec<usize>,
    exif_len: usize,
    drive_file_id: Option<String>,
    drive_parent_id: Option<String>,
    drive_baked: bool,
    next_layer_id: usize,
}

thread_local! {
    static SESSION_SIGS: RefCell<HashMap<usize, ItemSig>> = RefCell::new(HashMap::new());
    static SESSION_GEN: Cell<u64> = const { Cell::new(0) };
    /// Persist stays inert until the restore pass finishes, so a fresh page
    /// holding an empty state can't wipe the stored session before reading it.
    static SESSION_READY: Cell<bool> = const { Cell::new(false) };
}

fn item_sig(item: &MediaItem, ord: usize) -> ItemSig {
    let (gen, is_raw, raw_len) = CACHE.with(|c| {
        let c = c.borrow();
        match c.photos.get(&item.id) {
            Some(p) => (p.gen, p.raw, p.raw_len),
            None => (0, false, 0),
        }
    });
    let bytes = match item.kind {
        MediaKind::Video => BytesSig::Video(
            CACHE.with(|c| c.borrow().video_blobs.get(&item.id).map(|b| b.size() as u64))
                .unwrap_or(0),
        ),
        MediaKind::Photo if is_raw => BytesSig::Raw(raw_len, gen),
        MediaKind::Photo => BytesSig::Photo(item.object_url.clone(), gen),
    };
    let mut raster_layers: Vec<usize> = item
        .layers
        .iter()
        .filter_map(|l| matches!(l.kind, LayerKind::Raster(_)).then_some(l.id))
        .collect();
    raster_layers.sort_unstable();
    ItemSig {
        bytes,
        name: item.name.clone(),
        w: item.width,
        h: item.height,
        ord,
        recipe: state::edit_recipe_json(&item.edit),
        layers: session::layers_json(&item.layers),
        raster_layers,
        exif_len: item.exif.as_ref().map(|e| e.len()).unwrap_or(0),
        drive_file_id: item.drive_file_id.clone(),
        drive_parent_id: item.drive_parent_id.clone(),
        drive_baked: item.drive_baked,
        next_layer_id: item.next_layer_id,
    }
}

fn schedule_session_save(state: AppState) {
    SESSION_GEN.with(|g| g.set(g.get() + 1));
    let gen = SESSION_GEN.with(|g| g.get());
    let cb = wasm_bindgen::closure::Closure::once(move || {
        spawn_local(async move {
            if SESSION_GEN.with(|g| g.get()) != gen {
                return;
            }
            persist_session(state).await;
        });
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        800,
    );
    cb.forget();
}

/// PNG-encode a pixel buffer into a Blob (for healed photos and raster
/// layers).
async fn pixels_to_png_blob(pixels: &[u8], w: usize, h: usize) -> Result<Blob, JsValue> {
    let canvas = web::create_canvas(w as u32, h as u32);
    web::put_pixels(&canvas, pixels, w as u32, h as u32);
    web::canvas_to_blob(&canvas, "image/png").await
}

async fn persist_session(state: AppState) {
    if !SESSION_READY.with(|r| r.get()) {
        return;
    }
    let mut persist_span = perf::Span::new("session.persist");
    let items = state.items.get_untracked();
    persist_span.set_detail(format!("{} items", items.len()));
    let Ok(db) = session::open_db().await else { return };
    let live: std::collections::HashSet<usize> = items.iter().map(|i| i.id).collect();
    let gone: Vec<usize> = SESSION_SIGS.with(|s| {
        s.borrow().keys().filter(|id| !live.contains(id)).cloned().collect()
    });
    for id in &gone {
        let _ = session::delete_layer_pixels(&db, *id).await;
    }
    let _ = session::delete_raw_assets(&db, &gone).await;
    let _ = session::delete_items(&db, &gone).await;
    SESSION_SIGS.with(|s| {
        let mut s = s.borrow_mut();
        for id in &gone {
            s.remove(id);
        }
    });

    for (ord, item) in items.iter().enumerate() {
        let sig = item_sig(item, ord);
        let prev = SESSION_SIGS.with(|s| s.borrow().get(&item.id).cloned());
        if prev.as_ref() == Some(&sig) {
            continue;
        }
        let rec = session::jobj();
        let rec_v: &JsValue = rec.as_ref();
        session::jset(rec_v, "v", 1.0);
        session::jset(rec_v, "name", item.name.as_str());
        session::jset(rec_v, "kind", if item.kind == MediaKind::Video { 1.0 } else { 0.0 });
        session::jset(rec_v, "w", item.width as f64);
        session::jset(rec_v, "h", item.height as f64);
        session::jset(rec_v, "ord", ord as f64);
        session::jset(rec_v, "nlid", item.next_layer_id as f64);
        match &sig.recipe {
            Some(r) => session::jset(rec_v, "recipe", r.as_str()),
            None => session::jset(rec_v, "recipe", JsValue::NULL),
        }
        match &sig.layers {
            Some(l) => session::jset(rec_v, "layers", l.as_str()),
            None => session::jset(rec_v, "layers", JsValue::NULL),
        }
        match (&item.drive_file_id, &item.drive_parent_id) {
            (Some(a), Some(b)) => {
                session::jset(rec_v, "dfid", a.as_str());
                session::jset(rec_v, "dpid", b.as_str());
            }
            (Some(a), None) => {
                session::jset(rec_v, "dfid", a.as_str());
                session::jset(rec_v, "dpid", JsValue::NULL);
            }
            _ => {
                session::jset(rec_v, "dfid", JsValue::NULL);
                session::jset(rec_v, "dpid", JsValue::NULL);
            }
        }
        session::jset(rec_v, "dbaked", item.drive_baked);
        if let Some(exif) = &item.exif {
            let arr = js_sys::Uint8Array::from(exif.as_slice());
            session::jset(rec_v, "exif", arr.buffer());
        } else {
            session::jset(rec_v, "exif", JsValue::NULL);
        }
        match item.taken {
            Some(t) => session::jset(rec_v, "taken", t),
            None => session::jset(rec_v, "taken", JsValue::NULL),
        }

        // Bytes: a lazy RAW's original lives in the raw_src store, so its
        // record carries only the length. Other items embed a source blob.
        // Rewrite only when the bytes signature moved; otherwise carry the
        // stored blob over (put() replaces the whole record).
        let bytes_moved = prev.as_ref().map(|p| p.bytes != sig.bytes).unwrap_or(true);
        let gen = match &sig.bytes {
            BytesSig::Photo(_, g) | BytesSig::Raw(_, g) => *g,
            BytesSig::Video(_) => 0,
        };
        let healed = item.kind == MediaKind::Photo && gen > 0;
        // A RAW whose original never reached raw_src (quota, private mode) has
        // no lazy record to point at — it must embed its bytes like a legacy
        // RAW or it would vanish on reload.
        let raw_persisted = CACHE.with(|c| {
            c.borrow().photos.get(&item.id).map(|p| p.raw_persisted).unwrap_or(true)
        });
        let lazy_raw = matches!(sig.bytes, BytesSig::Raw(_, 0)) && !healed && raw_persisted;
        session::jset(rec_v, "raw", matches!(sig.bytes, BytesSig::Raw(_, _)) && !healed);
        session::jset(rec_v, "raster", healed);
        let mut persisted_png: Option<Blob> = None;
        if lazy_raw {
            let raw_len = match &sig.bytes {
                BytesSig::Raw(l, _) => *l,
                _ => 0,
            };
            session::jset(rec_v, "rawlen", raw_len as f64);
        } else if bytes_moved {
            let blob: Option<Blob> = match &sig.bytes {
                BytesSig::Video(_) => {
                    CACHE.with(|c| c.borrow().video_blobs.get(&item.id).cloned())
                }
                BytesSig::Photo(url, 0) => match web::object_url_bytes(url).await {
                    Ok(b) => web::bytes_to_blob(&b, "application/octet-stream").ok(),
                    Err(_) => None,
                },
                // RAW original that never made it into raw_src: embed the
                // bytes we kept in the cache for exactly this fallback.
                BytesSig::Raw(_, 0) => CACHE.with(|c| {
                    c.borrow().raw_fallback.get(&item.id).cloned()
                })
                .and_then(|b| web::bytes_to_blob(&b, "application/octet-stream").ok()),
                // Healed: the cache's full-res raster is the source of truth.
                // gen moved ⇒ the item is dirty ⇒ full is present (dirty
                // items are eviction-exempt), so the None arm can't fire.
                BytesSig::Photo(_, _) | BytesSig::Raw(_, _) => match get_photo(item.id) {
                    Some(p) => match &p.full {
                        Some((px, w, h)) => px.to_png_blob(*w, *h).await.ok(),
                        None => None,
                    },
                    None => None,
                },
            };
            let Some(blob) = blob else { continue };
            if healed {
                persisted_png = Some(blob.clone());
            }
            session::jset(rec_v, "bytes", blob);
        } else if let Ok(Some(old)) = session::get_item(&db, item.id).await {
            if let Some(b) = state::jget(&old, "bytes") {
                session::jset(rec_v, "bytes", b);
            } else {
                continue;
            }
        } else {
            continue;
        }

        // Raster layer pixel PNGs: store once per layer id (immutable), drop
        // records for layers that no longer exist.
        let prev_rasters: Vec<usize> = prev.map(|p| p.raster_layers).unwrap_or_default();
        for layer in &item.layers {
            let LayerKind::Raster(r) = &layer.kind else { continue };
            if prev_rasters.contains(&layer.id) {
                continue;
            }
            match pixels_to_png_blob(&r.pixels, r.width, r.height).await {
                Ok(b) => {
                    let _ = session::put_layer_pixels(&db, item.id, layer.id, b.as_ref()).await;
                }
                Err(e) => web::log(&format!("layer pixels: {}", js_err(&e))),
            }
        }
        for old in prev_rasters.iter().filter(|id| !sig.raster_layers.contains(id)) {
            let _ = session::delete_layer_pixel(&db, item.id, *old).await;
        }
        let healed_raw = healed && matches!(sig.bytes, BytesSig::Raw(_, _));
        let _ = session::put_item(&db, item.id, rec_v).await;
        SESSION_SIGS.with(|s| {
            s.borrow_mut().insert(item.id, sig);
        });
        // The baked PNG is now the re-decode source; the original's 25MB copy
        // in raw_src would otherwise sit there forever.
        if healed_raw {
            let _ = session::delete_raw_assets(&db, &[item.id]).await;
            CACHE.with(|c| c.borrow_mut().raw_fallback.remove(&item.id));
        }
        // A healed raster just landed in IndexedDB — it becomes the re-decode
        // source and the cache copy is clean (evictable) again.
        if let Some(png) = persisted_png {
            CACHE.with(|c| {
                let mut c = c.borrow_mut();
                c.photo_src.insert(item.id, png);
                if let Some(p) = c.photos.get_mut(&item.id) {
                    Rc::make_mut(p).dirty = false;
                }
            });
        }
    }

    let meta = session::jobj();
    let meta_v: &JsValue = meta.as_ref();
    match state.selected.get_untracked() {
        Some(id) => session::jset(meta_v, "selected", id as f64),
        None => session::jset(meta_v, "selected", JsValue::NULL),
    }
    session::jset(meta_v, "next_id", state.next_id.get_untracked() as f64);
    let _ = session::put_meta(&db, meta_v).await;
}

/// Rebuild the working set from IndexedDB on startup. Mirrors the ingest
/// paths: videos re-probe, RAW re-decodes from the original bytes, healed
/// photos come back as plain photos from their baked PNG.
fn restore_session(state: AppState) {
    spawn_local(async move {
        let mut restore_span = perf::Span::new("restore.session");
        let db = match session::open_db().await {
            Ok(db) => db,
            Err(_) => {
                SESSION_READY.with(|r| r.set(true));
                return;
            }
        };
        let mut records = session::all_items(&db).await.unwrap_or_default();
        records.sort_by_key(|(_, v)| state::jnum(v, "ord").unwrap_or(0.0) as usize);
        if records.is_empty() {
            SESSION_READY.with(|r| r.set(true));
            return;
        }
        restore_span.set_detail(format!("{} items", records.len()));
        state.busy.set(Some("Restoring session…".into()));
        let mut max_id = 0usize;
        for (id, v) in records {
            max_id = max_id.max(id);
            let name = state::jget(&v, "name")
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| "restored".into());
            let _item_span =
                perf::Span::new("restore.item").detail(format!("{name} #{id}"));
            let is_video = state::jnum(&v, "kind").unwrap_or(0.0) == 1.0;
            let is_raw = state::jbool(&v, "raw").unwrap_or(false);
            let recipe = state::jget(&v, "recipe").and_then(|s| s.as_string());
            let mut edit = recipe
                .as_deref()
                .and_then(state::edit_params_from_recipe)
                .unwrap_or_default();
            edit.selection = None;
            let mut layers = state::jget(&v, "layers")
                .and_then(|s| s.as_string())
                .map(|j| session::layers_from_json(&j))
                .unwrap_or_default();
            // Fill raster layer pixels from their PNG blobs.
            for layer in &mut layers {
                let LayerKind::Raster(r) = &mut layer.kind else { continue };
                let Ok(Some(px_v)) = session::get_layer_pixels(&db, id, layer.id).await else {
                    continue;
                };
                let Ok(px_blob) = px_v.dyn_into::<Blob>() else { continue };
                if let Ok((img, _)) = web::load_image(&px_blob).await {
                    let (px, pw, ph) = web::rgba_from_image(&img);
                    r.pixels = Rc::new(px);
                    r.width = pw;
                    r.height = ph;
                }
            }
            let exif = state::jget(&v, "exif").map(|e| {
                let arr = js_sys::Uint8Array::new(&e);
                Rc::new(arr.to_vec())
            });
            let drive_file_id = state::jget(&v, "dfid").and_then(|s| s.as_string());
            let drive_parent_id = state::jget(&v, "dpid").and_then(|s| s.as_string());
            let drive_baked = state::jbool(&v, "dbaked").unwrap_or(false);
            let next_layer_id = state::jnum(&v, "nlid").unwrap_or(0.0) as usize;
            let taken = state::jnum(&v, "taken");

            // A RAW added by the preview-first importer restores from its
            // stored preview — no sensor decode, no 25MB read.
            let lazy_preview = if is_raw && !is_video {
                match session::get_preview(&db, id).await {
                    Ok(Some(pv)) => pv.dyn_into::<Blob>().ok(),
                    _ => None,
                }
            } else {
                None
            };

            if is_video {
                let Some(bytes_v) = state::jget(&v, "bytes") else { continue };
                let Ok(blob) = bytes_v.dyn_into::<Blob>() else { continue };
                let url = Url::create_object_url_with_blob(&blob).unwrap_or_default();
                let Some((dur, w, h)) = probe_video(&url).await else { continue };
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
                    edit,
                    layers,
                    next_layer_id,
                    exif,
                    taken,
                    drive_file_id,
                    drive_parent_id,
                    drive_baked,
                });
            } else if let Some(pv) = lazy_preview {
                let Ok((img, url)) = web::load_image(&pv).await else { continue };
                let preview = downscale(&img, 1024);
                let thumb_url = thumb_object_url(&preview.0, preview.1, preview.2).await;
                let w = state::jnum(&v, "w").unwrap_or(preview.1 as f64) as usize;
                let h = state::jnum(&v, "h").unwrap_or(preview.2 as f64) as usize;
                let raw_len = state::jnum(&v, "rawlen").unwrap_or(0.0) as u64;
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: None,
                            full_dims: (w, h),
                            preview,
                            dirty: false,
                            gen: 0,
                            raw: true,
                            raw_len,
                            raw_persisted: true,
                        }),
                    )
                });
                push_item(state, MediaItem {
                    id,
                    kind: MediaKind::Photo,
                    name,
                    object_url: url,
                    thumb_url,
                    width: w,
                    height: h,
                    edit,
                    layers,
                    next_layer_id,
                    exif,
                    taken,
                    drive_file_id,
                    drive_parent_id,
                    drive_baked,
                });
            } else if is_raw {
                // Legacy RAW record: bytes live in the item record. Decode
                // once, then migrate to the raw_src/previews stores so the
                // next restore is preview-only.
                let Some(bytes_v) = state::jget(&v, "bytes") else { continue };
                let Ok(blob) = bytes_v.dyn_into::<Blob>() else { continue };
                let raw_len = blob.size() as u64;
                let Some((full_rgba, fw, fh)) = raw_develop_async(id, blob.as_ref()).await else {
                    continue;
                };
                let (pp, pw, ph) = full_rgba.downscale(fw, fh, 1024);
                let canvas = web::create_canvas(pw as u32, ph as u32);
                web::put_pixels(&canvas, &pp, pw as u32, ph as u32);
                let Ok(png) = web::canvas_to_blob(&canvas, "image/png").await else {
                    continue;
                };
                let raw_ok = session::put_raw_src(&db, id, blob.as_ref()).await.is_ok();
                let prev_ok = session::put_preview(&db, id, png.as_ref()).await.is_ok();
                let raw_persisted = raw_ok && prev_ok;
                let url = Url::create_object_url_with_blob(&png).unwrap_or_default();
                let thumb_url = thumb_object_url(&pp, pw, ph).await;
                if !raw_persisted {
                    if let Ok(bytes) = web::blob_to_bytes(&blob).await {
                        CACHE.with(|c| c.borrow_mut().raw_fallback.insert(id, Rc::new(bytes)));
                    }
                }
                full_rgba.load_into_worker(id, fw, fh);
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: Some((full_rgba, fw, fh)),
                            full_dims: (fw, fh),
                            preview: (pp, pw, ph),
                            dirty: false,
                            gen: 0,
                            raw: true,
                            raw_len,
                            raw_persisted,
                        }),
                    )
                });
                touch_full(id);
                evict_full_lru();
                push_item(state, MediaItem {
                    id,
                    kind: MediaKind::Photo,
                    name,
                    object_url: url,
                    thumb_url,
                    width: fw,
                    height: fh,
                    edit,
                    layers,
                    next_layer_id,
                    exif,
                    taken,
                    drive_file_id,
                    drive_parent_id,
                    drive_baked,
                });
            } else {
                let Some(bytes_v) = state::jget(&v, "bytes") else { continue };
                let Ok(blob) = bytes_v.dyn_into::<Blob>() else { continue };
                let Ok((img, url)) = web::load_image(&blob).await else { continue };
                let (fw, fh) = (img.natural_width() as usize, img.natural_height() as usize);
                let preview = downscale(&img, 1024);
                let thumb_url = thumb_object_url(&preview.0, preview.1, preview.2).await;
                CACHE.with(|c| {
                    let mut c = c.borrow_mut();
                    c.photo_src.insert(id, blob);
                    c.photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: None,
                            full_dims: (fw, fh),
                            preview,
                            dirty: false,
                            gen: 0,
                            raw: false,
                            raw_len: 0,
                            raw_persisted: true,
                        }),
                    );
                });
                push_item(state, MediaItem {
                    id,
                    kind: MediaKind::Photo,
                    name,
                    object_url: url,
                    thumb_url,
                    width: fw,
                    height: fh,
                    edit,
                    layers,
                    next_layer_id,
                    exif,
                    taken,
                    drive_file_id,
                    drive_parent_id,
                    drive_baked,
                });
            }
            // Seed the signature so the next persist pass doesn't rewrite
            // bytes that are already stored.
            let ord = state.items.with(|v| v.iter().position(|m| m.id == id).unwrap_or(0));
            if let Some(item) = state.items.with(|v| v.iter().find(|m| m.id == id).cloned()) {
                let sig = item_sig(&item, ord);
                SESSION_SIGS.with(|s| {
                    s.borrow_mut().insert(id, sig);
                });
            }
        }
        if let Ok(Some(meta)) = session::get_meta(&db).await {
            let selected = state::jnum(&meta, "selected").map(|s| s as usize);
            let next_id = state::jnum(&meta, "next_id").unwrap_or(0.0) as usize;
            if let Some(sel) = selected {
                if state.items.with(|v| v.iter().any(|m| m.id == sel)) {
                    state.selected.set(Some(sel));
                }
            }
            state.next_id.set(next_id.max(max_id + 1));
        } else {
            state.next_id.set(max_id + 1);
        }
        SESSION_READY.with(|r| r.set(true));
        state.busy.set(None);
        // Record the restored baseline without rewriting bytes.
        schedule_session_save(state);
    });
}

/// Imported Drive files join the star set: the star persists in the manifest,
/// so "Import starred" can treat it as "should be open" instead of a one-shot
/// queue, and unstarring stays the way a Drive-backed item is removed again.
fn star_drive_file(state: AppState, id: &str) {
    let mut added = false;
    state.drive_shortlist.update(|s| added = s.insert(id.to_string()));
    if added {
        schedule_manifest_save(state);
    }
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
    schedule_manifest_save(state);
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

thread_local! {
    static DRIVE_IMPORT_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Import Drive files one download at a time. A starred batch can be dozens
/// of 25MB RAWs; firing every fetch at once buffered the whole set
/// (1.4GB for 53 files) in the tab while the multi-second decodes ran
/// serially, and the renderer died partway down the list. Ingest tasks still
/// run one at a time — each decodes synchronously on the main thread.
fn import_drive_files(state: AppState, files: Vec<drive::DriveFile>, missing: Vec<String>) {
    if files.is_empty() && missing.is_empty() {
        return;
    }
    if DRIVE_IMPORT_ACTIVE.with(|a| a.replace(true)) {
        state
            .drive_error
            .set(Some("A Drive import is already running".into()));
        return;
    }
    // Everything imported from the Drive view is starred, so it can be
    // unstarred (removed) from the filmstrip and survives as part of the set.
    for f in &files {
        star_drive_file(state, &f.id);
    }
    spawn_local(async move {
        let Some(mut token) = fresh_drive_token(state).await else {
            DRIVE_IMPORT_ACTIVE.with(|a| a.set(false));
            return;
        };
        pool_batch_begin();
        let total = files.len() + missing.len();
        let mut done = 0;
        for f in files {
            // A big batch can outlive the token it started with.
            if let Some(t) = fresh_drive_token(state).await {
                token = t;
            }
            done += 1;
            import_drive_one(state, &token, f, done, total).await;
        }
        for id in missing {
            if let Some(t) = fresh_drive_token(state).await {
                token = t;
            }
            match drive::get_file(&token, &id).await {
                Ok(f) if f.importable() => {
                    done += 1;
                    import_drive_one(state, &token, f, done, total).await;
                }
                Ok(_) => {}
                Err(e) => state
                    .drive_error
                    .set(Some(format!("Starred file lookup failed: {}", js_err(&e)))),
            }
        }
        state.busy.set(None);
        pool_batch_end();
        DRIVE_IMPORT_ACTIVE.with(|a| a.set(false));
    });
}

async fn import_drive_one(
    state: AppState,
    token: &str,
    f: drive::DriveFile,
    done: usize,
    total: usize,
) {
    state.busy.set(Some(format!("Importing {done}/{total} — {}", f.name)));
    state.drive_error.set(None);
    match drive::download_file(token, &f.id).await {
        Ok(bytes) => ingest_drive_bytes(state, f.id, f.parent, f.name, f.edited, bytes).await,
        Err(e) => {
            state.busy.set(None);
            state
                .drive_error
                .set(Some(format!("Drive download failed: {}", js_err(&e))));
        }
    }
}

// Full-res RGBA decoded on Drive hover for the 1:1 loupe; FIFO-capped at 2
// files (~192MB for 24MP shots) so flip-flopping between two candidates
// stays instant without inviting the OOM the 512MB working set guards
// against. Crops are row-copied out of these buffers — never a third
// full-res canvas.
thread_local! {
    static DRIVE_HOVER_CACHE: RefCell<(Vec<String>, HashMap<String, Rc<(Pixels, usize, usize)>>)> =
        RefCell::new((Vec::new(), HashMap::new()));
    static DRIVE_HOVER_INFLIGHT: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    /// Ids in DRIVE_HOVER_CACHE whose pixels are an embedded RAW preview, not
    /// the full image — the loupe tag says so.
    static DRIVE_HOVER_PREVIEW: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    /// Sensor-res develop for the currently popped-up RAW, single-slot: a
    /// 24MP demosaic is ~96MB and only the open popup can use it.
    static DRIVE_FULL_CACHE: RefCell<Option<(String, Rc<(Pixels, usize, usize)>)>> =
        RefCell::new(None);
}

fn drive_full_cached(id: &str) -> bool {
    DRIVE_FULL_CACHE.with(|c| {
        c.borrow()
            .as_ref()
            .map(|(fid, _)| fid == id)
            .unwrap_or(false)
    })
}

/// Develop a RAW at sensor resolution on explicit request, then let the hover
/// loupe crop real pixels instead of the embedded preview. The demosaic runs
/// synchronously on the main thread for seconds, so the busy label is given a
/// frame to paint first; closing or stepping the popup drops the result.
fn drive_develop_full(state: AppState, id: String) {
    if state.drive_full_ready.with(|r| r.as_deref() == Some(id.as_str()))
        || state.drive_full_busy.with(|b| b.is_some())
    {
        return;
    }
    if state.drive_token.with(|t| t.is_none()) {
        return;
    }
    state.drive_error.set(None);
    state.drive_full_busy.set(Some(id.clone()));
    spawn_local(async move {
        let alive = || state.drive_full_busy.with(|b| b.as_deref() == Some(id.as_str()));
        sleep_ms(60).await;
        if !alive() {
            return;
        }
        let Some(token) = fresh_drive_token(state).await else {
            state.drive_full_busy.set(None);
            return;
        };
        let bytes = match drive::download_file(token.as_str(), &id).await {
            Ok(bytes) => bytes,
            Err(e) => {
                state.drive_full_busy.set(None);
                state
                    .drive_error
                    .set(Some(format!("Full-res download failed: {}", js_err(&e))));
                return;
            }
        };
        if !alive() {
            return;
        }
        let Ok(blob) = web::bytes_to_blob(&bytes, "application/octet-stream") else {
            state.drive_full_busy.set(None);
            return;
        };
        drop(bytes);
        // Drive files have no numeric item id; 0 never matches a cached frame.
        match raw_develop_async(0, blob.as_ref()).await {
            Some((px, w, h)) => {
                if !alive() {
                    return;
                }
                DRIVE_FULL_CACHE.with(|c| {
                    *c.borrow_mut() = Some((id.clone(), Rc::new((px, w, h))));
                });
                DRIVE_HOVER_CACHE.with(|c| {
                    let mut c = c.borrow_mut();
                    c.0.retain(|k| k != &id);
                    c.1.remove(&id);
                });
                DRIVE_HOVER_PREVIEW.with(|s| {
                    s.borrow_mut().remove(&id);
                });
                state.drive_full_busy.set(None);
                state.drive_full_ready.set(Some(id.clone()));
            }
            None => {
                state.drive_full_busy.set(None);
                state
                    .drive_error
                    .set(Some("Full-res develop failed: unsupported or corrupt RAW".to_string()));
            }
        }
    });
}

/// Download + decode a Drive photo's original for the hover loupe. Stills
/// (JPEG/PNG) decode directly; RAW uses the embedded preview, since a full
/// demosaic is far too slow for a hover. Sony ARWs embed a full-size camera
/// JPEG that rawler never reads — decode that for sensor-resolution crops.
/// HEIC keeps the w800 preview.
fn drive_hover_prefetch(state: AppState, id: String, name: String) {
    let lname = name.to_lowercase();
    let raw = raw::is_raw_name(&lname);
    let arw = lname.ends_with(".arw");
    let still_mime = if lname.ends_with(".png") {
        Some("image/png")
    } else if lname.ends_with(".jpg") || lname.ends_with(".jpeg") {
        Some("image/jpeg")
    } else {
        None
    };
    if !raw && still_mime.is_none() {
        return;
    }
    // The sensor-res buffer serves the loupe directly; don't re-fetch the
    // embedded preview alongside it.
    if drive_full_cached(&id) {
        return;
    }
    if DRIVE_HOVER_CACHE.with(|c| c.borrow().1.contains_key(&id)) {
        state.drive_hover_ready.set(Some(id));
        return;
    }
    if !DRIVE_HOVER_INFLIGHT.with(|s| s.borrow_mut().insert(id.clone())) {
        return;
    }
    if state.drive_token.with(|t| t.is_none()) {
        DRIVE_HOVER_INFLIGHT.with(|s| {
            s.borrow_mut().remove(&id);
        });
        return;
    }
    spawn_local(async move {
        let decoded = async {
            let token = fresh_drive_token(state).await?;
            let bytes = drive::download_file(token.as_str(), &id).await.ok()?;
            if raw {
                let blob = web::bytes_to_blob(&bytes, "application/octet-stream").ok()?;
                drop(bytes);
                let src: JsValue = blob.into();
                if arw {
                    if let Some(meta) = raw_meta_async(&src, 0).await {
                        // Below ~3k wide the largest JPEG is still a preview,
                        // not sensor resolution — keep the preview tag.
                        if meta.camera_w >= 3000 {
                            if let Some(jpeg) = meta.camera_jpeg {
                                if let Some((px, w, h)) = camera_jpeg_rgba(&jpeg).await {
                                    let (px, w, h) =
                                        raw::apply_orientation_code(px, w, h, meta.orientation);
                                    return Some(((Pixels::owned(px), w, h), false));
                                }
                            }
                        }
                    }
                }
                return raw_preview_async(&src)
                    .await
                    .map(|r| ((Pixels::view(r.pixels), r.w, r.h), true));
            }
            let blob = web::bytes_to_blob(&bytes, still_mime?).ok()?;
            let (img, _) = web::load_image(&blob).await.ok()?;
            let (px, w, h) = web::rgba_from_image(&img);
            Some(((Pixels::owned(px), w, h), false))
        }
        .await;
        DRIVE_HOVER_INFLIGHT.with(|s| {
            s.borrow_mut().remove(&id);
        });
        if let Some(((px, w, h), preview)) = decoded {
            DRIVE_HOVER_CACHE.with(|c| {
                let mut c = c.borrow_mut();
                c.0.retain(|k| k != &id);
                c.0.push(id.clone());
                c.1.insert(id.clone(), Rc::new((px, w, h)));
                while c.0.len() > 2 {
                    let old = c.0.remove(0);
                    c.1.remove(&old);
                    DRIVE_HOVER_PREVIEW.with(|s| {
                        s.borrow_mut().remove(&old);
                    });
                }
            });
            DRIVE_HOVER_PREVIEW.with(|s| {
                let mut s = s.borrow_mut();
                s.remove(&id);
                if preview {
                    s.insert(id.clone());
                }
            });
            state.drive_hover_ready.set(Some(id));
        }
    });
}

/// Decode an embedded camera JPEG through the browser's image pipeline.
async fn camera_jpeg_rgba(jpeg: &Uint8Array) -> Option<(Vec<u8>, usize, usize)> {
    let blob = web::view_to_blob(jpeg, "image/jpeg").ok()?;
    let (img, url) = web::load_image(&blob).await.ok()?;
    let out = web::rgba_from_image(&img);
    let _ = web_sys::Url::revoke_object_url(&url);
    Some(out)
}

/// Hover-zoom: open the floating preview after a short rest on a tile, so
/// sweeping the cursor across the grid doesn't strobe previews. `sel` is the
/// element the normalized position is measured against (grid tile or the
/// popped-up loupe image).
fn drive_hover_soon(state: AppState, id: String, name: String, ev: &web_sys::MouseEvent, sel: &str) {
    let Some((nx, ny)) = norm_pos_in(ev, sel) else { return };
    let (cx, cy) = (ev.client_x() as f64, ev.client_y() as f64);
    let disp_w = hovered_width(ev, sel);
    state.drive_hover_gen.update(|g| *g += 1);
    let gen = state.drive_hover_gen.get_untracked();
    let cb = wasm_bindgen::closure::Closure::once(move || {
        if state.drive_hover_gen.get_untracked() == gen {
            drive_hover_prefetch(state, id.clone(), name);
            state.drive_hover.set(Some((id, nx, ny, cx, cy, disp_w)));
        }
    });
    let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
        cb.as_ref().unchecked_ref(),
        280,
    );
    cb.forget();
}

fn drive_hover_move(state: AppState, id: &str, ev: &web_sys::MouseEvent, sel: &str) {
    let showing = state.drive_hover.with(|h| {
        h.as_ref().map(|(hid, ..)| hid == id).unwrap_or(false)
    });
    if showing {
        let Some((nx, ny)) = norm_pos_in(ev, sel) else { return };
        state.drive_hover.set(Some((
            id.to_string(),
            nx,
            ny,
            ev.client_x() as f64,
            ev.client_y() as f64,
            hovered_width(ev, sel),
        )));
    }
}

fn drive_hover_clear(state: AppState) {
    state.drive_hover_gen.update(|g| *g += 1);
    state.drive_hover.set(None);
}

/// Fixed-position style for the hover preview, kept inside the viewport.
fn drive_hover_style(x: f64, y: f64) -> String {
    let w = 496.0_f64;
    let h = 398.0_f64;
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
    let close = move |_| {
        drive_hover_clear(state);
        state.drive_loupe.set(None);
    };
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
        // The sensor-res buffer belongs to the previous popup; a develop still
        // in flight sees the cleared busy marker and drops its result.
        DRIVE_FULL_CACHE.with(|c| *c.borrow_mut() = None);
        state.drive_full_ready.set(None);
        state.drive_full_busy.set(None);
    });
    let step = move |d: i64| {
        let n = visible_drive_files(state).len();
        if n == 0 {
            return;
        }
        drive_hover_clear(state);
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
            "Escape" => {
                drive_hover_clear(state);
                state.drive_loupe.set(None);
            }
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
                let fid_h1 = f.id.clone();
                let fid_h2 = f.id.clone();
                let fname_h = f.name.clone();
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
                                on:mouseenter=move |ev| {
                                    drive_hover_soon(
                                        state,
                                        fid_h1.clone(),
                                        fname_h.clone(),
                                        &ev,
                                        ".loupe-img",
                                    );
                                }
                                on:mousemove=move |ev| {
                                    drive_hover_move(state, &fid_h2, &ev, ".loupe-img")
                                }
                                on:mouseleave=move |_| drive_hover_clear(state)
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
                                {raw::is_raw_name(&f.name)
                                    .then(|| {
                                        let fid_click = f.id.clone();
                                        let fid_label = f.id.clone();
                                        view! {
                                            <button
                                                class="btn loupe-full"
                                                title="Develop this RAW at sensor resolution for the loupe"
                                                on:click=move |_| drive_develop_full(state, fid_click.clone())
                                            >
                                                {move || {
                                                    if state
                                                        .drive_full_ready
                                                        .with(|r| r.as_deref() == Some(fid_label.as_str()))
                                                    {
                                                        "Full res ✓"
                                                    } else if state
                                                        .drive_full_busy
                                                        .with(|b| b.as_deref() == Some(fid_label.as_str()))
                                                    {
                                                        "Developing…"
                                                    } else {
                                                        "Full res"
                                                    }
                                                }}
                                            </button>
                                        }
                                    })}
                                <button
                                    class="btn primary"
                                    disabled=!f_import.importable()
                                    on:click=move |_| {
                                        import_drive_files(
                                            state,
                                            vec![f_import.clone()],
                                            Vec::new(),
                                        );
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
                Ok(auth) => {
                    state.drive_token.set(Some(Rc::new(auth.token)));
                    state.drive_token_expiry.set(auth.expires_at_ms);
                    drive_refresh(state);
                }
                Err(e) => state.drive_error.set(Some(js_err(&e))),
            }
            state.busy.set(None);
        });
    };
    let do_pick_folder = move |_| {
        spawn_local(async move {
            let Some(token) = fresh_drive_token(state).await else {
                return;
            };
            state.drive_error.set(None);
            match drive::pick_folder(&token).await {
                Ok(Some((id, name))) => {
                    drive::save_folder(&id, &name);
                    state.drive_folder.set(Some((id, name)));
                    state.drive_path.set(Vec::new());
                    // The manifest is per-grant: drop the previous folder's
                    // shortlist so its stars/recipes never bleed into (or get
                    // written to) the new root's manifest.
                    state.drive_shortlist.set(Default::default());
                    state.drive_edits.set(Default::default());
                    state.drive_manifest_id.set(None);
                    state.drive_manifest_loaded.set(false);
                    state.drive_edits_folder.set(None);
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
        state.drive_token_expiry.set(0.0);
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
        import_drive_files(state, files, Vec::new());
    };
    // Import the starred files that aren't open yet, including ones outside the
    // current folder (resolved by id). Stars stay on imported files, so
    // re-importing every starred id would duplicate the whole set.
    let import_starred = move |_| {
        let open: HashSet<String> = state.items.with(|v| {
            v.iter().filter_map(|m| m.drive_file_id.clone()).collect()
        });
        let ids: Vec<String> = state.drive_shortlist.with(|s| {
            s.iter()
                .filter(|id| !open.contains(*id))
                .cloned()
                .collect()
        });
        if ids.is_empty() {
            return;
        }
        state.drive_open.set(false);
        let known = state.drive_files.get_untracked();
        let files: Vec<drive::DriveFile> = known
            .iter()
            .filter(|f| ids.contains(&f.id) && f.importable())
            .cloned()
            .collect();
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !known.iter().any(|f| &f.id == *id))
            .cloned()
            .collect();
        import_drive_files(state, files, missing);
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
                                    let open: HashSet<String> = state.items.with(|v| {
                                        v.iter().filter_map(|m| m.drive_file_id.clone()).collect()
                                    });
                                    let n = state.drive_shortlist.with(|s| {
                                        s.iter().filter(|id| !open.contains(*id)).count()
                                    });
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
                                        let fname = f.name.clone();
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
                                                            fname.clone(),
                                                            &ev,
                                                            ".drive-file",
                                                        );
                                                    }
                                                }
                                                on:mousemove=move |ev| {
                                                    drive_hover_move(
                                                        state,
                                                        &fid3_move,
                                                        &ev,
                                                        ".drive-file",
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
                            <DriveHoverLoupe state=state/>
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
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
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
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
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
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
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
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
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
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
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
        let Some((nx, ny)) = state.with_current(|m| layer_norm_pos(&ev, m.edit.crop)).flatten() else { return };
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
        perf::record("preview.effect", 0.0);
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
        tab.track();
        select_drag.track();
        // Paint on the leading edge so the change that opened this frame shows
        // in it; a burst of further changes this frame coalesces into one
        // trailing rAF pass, so there is still at most one extra paint/frame.
        let render: Rc<dyn Fn()> = Rc::new(move || {
            let Some(item) = state.current() else { return };
            if item.kind != MediaKind::Photo {
                return;
            }
            let Some(photo) = get_photo(item.id) else { return };
            let Some(canvas) = canvas_ref.get() else { return };
            let mut render_span = perf::Span::new("preview.render");

        let (pix, w, h) = &photo.preview;
        let geo = preview_geometry_cached(item.id, photo.gen, &photo.preview, &item.edit);
        let (mut p, w, h) = match &geo {
            Some((px, gw, gh)) => (Cow::Borrowed(px.as_slice()), *gw, *gh),
            None => geometry(pix, *w, *h, &item.edit),
        };
        if !item.edit.raw.is_identity() {
            rawops::apply(p.to_mut(), w, h, &item.edit.raw);
        }
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
        // Exact size the render will produce for this request, sized from the
        // source actually loaded (sensor pixels or the display proxy) — the
        // cache is sufficient iff it matches, which keeps rebuilds from looping.
        let (_, (sw, sh)) = render_source(item.id, &photo);
        let (fw, fh) = working_dims(sw, sh, &item.edit);
        let target = hi_target(req_w, req_h, fw, fh, HI_MAX_EDGE as u32);

        // Sharp backing for the canvas. The cropped view prefers a crop-window
        // render (native density up to WINDOW_MAX_EDGE); the whole-frame hi
        // cache still serves the full view and backs the cropped one when a
        // window is not available (worker down → sync fallback).
        let edit_fp = edit_fingerprint(&item);
        let src_fp = source_fingerprint(photo.gen, proxy_gen(item.id));
        let (cx_px, cy_px, cw_p, _) = crop_px(&item.edit, w, h);
        let (cx_p, cy_p) = (cx_px as f64, cy_px as f64);
        let mut sharp: Option<Sharp> = None;
        // A sharp frame from an older source stays on screen while the new
        // one renders, so a RAW proxy upgrade never flashes the soft preview.
        let mut stale_source = false;
        if want_hi {
            if cropped_view {
                let (_, _, fcw, fch) = crop_px(&item.edit, fw, fh);
                let win_target = hi_target(need_w, need_h, fcw, fch, WINDOW_MAX_EDGE as u32);
                let key = crop_key(&crop);
                sharp = HI_WINDOW.with(|c| {
                    let b = c.borrow();
                    let win = b.as_ref()?;
                    if win.item_id != item.id || win.edit_fp != edit_fp || win.crop != key {
                        return None;
                    }
                    if win.w < win_target.0 || win.h < win_target.1 {
                        return None;
                    }
                    canvas.set_width(win.w as u32);
                    canvas.set_height(win.h as u32);
                    web::ctx2d(&canvas)
                        .draw_image_with_html_canvas_element(&win.canvas, 0.0, 0.0)
                        .unwrap();
                    stale_source = win.source_fp != src_fp;
                    Some(Sharp::Window { w: win.w, cw: cw_p })
                });
            }
            if sharp.is_none() {
                sharp = HI_CACHE.with(|c| {
                    let b = c.borrow();
                    let hi = b.as_ref()?;
                    if hi.item_id != item.id || hi.edit_fp != edit_fp {
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
                    stale_source = hi.source_fp != src_fp;
                    Some(Sharp::Whole { w: hi.w })
                });
            }
        }

        if sharp.is_none() {
            if item.edit.is_color_touched() {
                if let Some(sel) = &item.edit.selection {
                    let mask = ops::selection_mask(sel, w, h);
                    color::apply_color_effects_masked(
                        p.to_mut(),
                        &mask,
                        w,
                        h,
                        &item.edit,
                        item.id as u32,
                    );
                } else {
                    color::apply_color_effects(p.to_mut(), w, h, &item.edit, item.id as u32);
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
        }
        if want_hi && (sharp.is_none() || stale_source) {
            // Sharp frame lands 60ms after edits settle; until then the
            // cheap preview render keeps drags responsive. The worker's
            // base cache makes the follow-up render cheap, so a short
            // debounce no longer buys a costly re-decode of the frame.
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
                if !full_present(item.id) && !proxy_in_mem(item.id) {
                    // Pixels not decoded yet — kick the loader; its
                    // completion nudge retriggers this effect and
                    // reschedules the hi-res render.
                    ensure_proxy(state, item.id, true);
                    return;
                }
                let Some(photo) = get_photo(item.id) else { return };
                let (src_id, (sw2, sh2)) = render_source(item.id, &photo);
                let (fw2, fh2) = working_dims(sw2, sh2, &item.edit);
                HI_REQ_GEN.with(|d| d.set(d.get() + 1));
                let rgen = HI_REQ_GEN.with(|d| d.get());
                let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
                let sel = item.edit.selection.clone();
                let edit_fp = edit_fingerprint(&item);
                let source_fp = source_fingerprint(photo.gen, proxy_gen(item.id));
                let item_id = item.id;
                let item_cb = item.clone();
                if cropped_view {
                    let (fcx, fcy, fcw, fch) = crop_px(&item.edit, fw2, fh2);
                    let win_target =
                        hi_target(need_w, need_h, fcw, fch, WINDOW_MAX_EDGE as u32);
                    let request = perf::Span::new("hi.request")
                        .detail(format!("{}x{}", win_target.0, win_target.1));
                    let hi_t0 = perf::now();
                    let accepted = pworker::render_window(
                        src_id,
                        &recipe,
                        need_w,
                        need_h,
                        WINDOW_MAX_EDGE as u32,
                        sel.as_ref(),
                        move |reply| {
                            if HI_REQ_GEN.with(|d| d.get()) != rgen {
                                return;
                            }
                            match reply {
                                Ok(r) => {
                                    perf::record("hi.worker_reply", perf::now() - hi_t0);
                                    let blit = perf::Span::new("hi.blit");
                                    let canvas = web::create_canvas(r.w, r.h);
                                    web::put_pixels_view(&canvas, &r.pixels, r.w, r.h);
                                    // Layers live in working-image coords;
                                    // scale/translate them onto the window.
                                    let ctx = web::ctx2d(&canvas);
                                    let sx = r.w as f64 / fcw as f64;
                                    let sy = r.h as f64 / fch as f64;
                                    let _ = ctx.scale(sx, sy);
                                    let _ = ctx.translate(-(fcx as f64), -(fcy as f64));
                                    composite_layers(&canvas, &item_cb.layers, fw2, fh2);
                                    let _ =
                                        ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
                                    HI_WINDOW.with(|c| {
                                        *c.borrow_mut() = Some(HiWindow {
                                            item_id,
                                            edit_fp,
                                            source_fp,
                                            crop: crop_key(&item_cb.edit.crop),
                                            canvas,
                                            w: r.w as usize,
                                            h: r.h as usize,
                                        })
                                    });
                                    state.hi_built.update(|g| *g += 1);
                                    drop(blit);
                                }
                                _ => {
                                    perf::record("hi.worker_reply", perf::now() - hi_t0);
                                    if let Some(hi) = render_hi(&item_cb, req_w, req_h) {
                                        HI_CACHE.with(|c| *c.borrow_mut() = Some(hi));
                                        state.hi_built.update(|g| *g += 1);
                                    }
                                }
                            }
                        },
                    );
                    drop(request);
                    if !accepted {
                        if let Some(hi) = render_hi(&item, req_w, req_h) {
                            HI_CACHE.with(|c| *c.borrow_mut() = Some(hi));
                            state.hi_built.update(|g| *g += 1);
                        }
                    }
                } else {
                    let request =
                        perf::Span::new("hi.request").detail(format!("{}x{}", req_w, req_h));
                    let hi_t0 = perf::now();
                    let accepted = pworker::render(
                        src_id,
                        &recipe,
                        req_w,
                        req_h,
                        HI_MAX_EDGE as u32,
                        sel.as_ref(),
                        move |reply| {
                            if HI_REQ_GEN.with(|d| d.get()) != rgen {
                                return;
                            }
                            match reply {
                                Ok(r) => {
                                    perf::record("hi.worker_reply", perf::now() - hi_t0);
                                    let blit = perf::Span::new("hi.blit");
                                    let canvas = web::create_canvas(r.w, r.h);
                                    web::put_pixels_view(&canvas, &r.pixels, r.w, r.h);
                                    composite_layers(
                                        &canvas,
                                        &item_cb.layers,
                                        r.w as usize,
                                        r.h as usize,
                                    );
                                    HI_CACHE.with(|c| {
                                        *c.borrow_mut() = Some(HiCache {
                                            item_id,
                                            edit_fp,
                                            source_fp,
                                            canvas,
                                            w: r.w as usize,
                                            h: r.h as usize,
                                        })
                                    });
                                    state.hi_built.update(|g| *g += 1);
                                    drop(blit);
                                }
                                _ => {
                                    perf::record("hi.worker_reply", perf::now() - hi_t0);
                                    if let Some(hi) = render_hi(&item_cb, req_w, req_h) {
                                        HI_CACHE.with(|c| *c.borrow_mut() = Some(hi));
                                        state.hi_built.update(|g| *g += 1);
                                    }
                                }
                            }
                        },
                    );
                    drop(request);
                    if !accepted {
                        if let Some(hi) = render_hi(&item, req_w, req_h) {
                            HI_CACHE.with(|c| *c.borrow_mut() = Some(hi));
                            state.hi_built.update(|g| *g += 1);
                        }
                    }
                }
            });
            let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                60,
            );
            cb.forget();
        }

        // Overlay draws below use full-image preview pixel coords; map them
        // onto the canvas backing (sharp-frame scale and/or the crop offset).
        let ctx = web::ctx2d(&canvas);
        let overlay_open = match (&sharp, cropped_view) {
            (Some(Sharp::Window { w: win_w, cw }), _) => {
                ctx.save();
                let s = *win_w as f64 / *cw as f64;
                let _ = ctx.scale(s, s);
                let _ = ctx.translate(-cx_p, -cy_p);
                true
            }
            (Some(Sharp::Whole { w: hw }), true) => {
                ctx.save();
                let s = *hw as f64 / w as f64;
                let _ = ctx.scale(s, s);
                let _ = ctx.translate(-cx_p, -cy_p);
                true
            }
            (Some(Sharp::Whole { w: hw }), false) => {
                ctx.save();
                let s = *hw as f64 / w as f64;
                let _ = ctx.scale(s, s);
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
            render_span.set_detail(format!("{}x{}", canvas.width(), canvas.height()));
        });
        PREVIEW_DIRTY.with(|d| d.set(true));
        if !PREVIEW_PENDING.with(|p| p.get()) {
            PREVIEW_PENDING.with(|p| p.set(true));
            PREVIEW_DIRTY.with(|d| d.set(false));
            render();
            let trailing = render.clone();
            let cb = Closure::once(move || {
                PREVIEW_PENDING.with(|p| p.set(false));
                if PREVIEW_DIRTY.with(|d| d.replace(false)) {
                    trailing();
                }
            });
            web::request_animation_frame(cb.as_ref().unchecked_ref());
            cb.forget();
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
                            canvas_loupe_clear(state);
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
                        on:pointermove=move |ev: web_sys::PointerEvent| {
                            canvas_loupe_move(state, tab, &ev);
                        }
                        on:pointerleave=move |_| canvas_loupe_clear(state)
                    >
                        <canvas node_ref=canvas_ref></canvas>
                        <Show when=move || tab.get() == Tab::Crop fallback=|| ()>
                            <CropOverlay state=state working=working drag=crop_drag pinch=crop_pinch/>
                        </Show>
                        <SelectedTextOverlay state=state tab=tab layer_drag=layer_drag/>
                    </div>
                }.into_view(),
            }}
            <CanvasLoupe state=state tab=tab main_canvas=canvas_ref/>
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
                let Some(ratio) = state.with_current(|m| m.edit.aspect.ratio().map(|(rw, rh)| rw / rh)) else { return };
                let Some((ww, wh)) = working.try_get() else { return };
                let c = zoom_crop(
                    orig,
                    mid0,
                    (mid.0 - mid0.0, mid.1 - mid0.1),
                    spread / d0,
                    ratio,
                    ww as f32 / wh as f32,
                );
                state.update_current(|e| e.crop = c);
            });
            return;
        }
        // Signals are disposed when the Crop tab unmounts, but this window
        // listener outlives them — bail out instead of touching dead signals.
        let Some(Some((handle, sx, sy, orig))) = drag.try_get() else { return };
        let Some(ratio) = state.with_current(|m| m.edit.aspect.ratio().map(|(rw, rh)| rw / rh)) else { return };
        let Some((ww, wh)) = working.try_get() else { return };
        let Some((nx, ny)) = norm_pos(&ev) else { return };
        let dx = nx - sx;
        let dy = ny - sy;

        let mut c = orig;
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
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    if item.edit.selection.is_none() {
        return;
    }
    if !full_present(item.id) {
        ensure_full(state, item.id, Some(Box::new(move || selection_to_layer(state, cut))));
        return;
    }
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            let Some(sel) = item.edit.selection.clone() else { return false };
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let item_id = item.id;
            pworker::extract(item_id, &recipe, &sel, cut, move |res| {
                match res {
                    Ok(Some(r)) => {
                        state.items.update(|v| {
                            if let Some(m) = v.iter_mut().find(|m| m.id == item_id) {
                                let nid = m.next_layer_id;
                                m.next_layer_id += 1;
                                m.layers.push(Layer::new_raster(nid, r.layer, r.lw, r.lh));
                            }
                        });
                        if let Some(c) = r.cut {
                            destructive_apply(state, item_id, c);
                        }
                    }
                    Ok(None) => {}
                    Err(()) => selection_to_layer_sync(state, cut),
                }
            })
        },
        move || selection_to_layer_sync(state, cut),
    );
}

fn selection_to_layer_sync(state: AppState, cut: bool) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(sel) = item.edit.selection.clone() else { return; };
    let Some(photo) = get_photo(item.id) else { return; };
    let Some((full, fw, fh)) = photo.full.clone() else { return };
    let (base, w, h) = owned_geometry(&full, fw, fh, &item.edit);
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
        pworker::load(item.id, &cleared, w, h);
        CACHE.with(|c| {
            if let Some(pd) = c.borrow_mut().photos.get_mut(&item.id) {
                let pd = Rc::make_mut(pd);
                pd.full = Some((Pixels::owned(cleared), w, h));
                pd.full_dims = (w, h);
                pd.preview = preview;
                pd.dirty = true;
                pd.gen += 1;
            }
        });
        state.update_current(|e| {
            e.rot90 = 0;
            e.fine_angle = 0.0;
            e.selection = None;
        });
    }
}

fn delete_selection(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    if item.edit.selection.is_none() {
        return;
    }
    if !full_present(item.id) {
        ensure_full(state, item.id, Some(Box::new(move || delete_selection(state))));
        return;
    }
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            let Some(sel) = item.edit.selection.clone() else { return false };
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let item_id = item.id;
            pworker::cut(item_id, &recipe, &sel, move |res| {
                match res {
                    Ok(Some(r)) => destructive_apply(state, item_id, r),
                    Ok(None) => {}
                    Err(()) => delete_selection_sync(state),
                }
            })
        },
        move || delete_selection_sync(state),
    );
}

fn delete_selection_sync(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(sel) = item.edit.selection.clone() else { return; };
    let Some(photo) = get_photo(item.id) else { return; };
    let Some((full, fw, fh)) = photo.full.clone() else { return };
    let (mut base, w, h) = owned_geometry(&full, fw, fh, &item.edit);
    let mask = ops::selection_mask(&sel, w, h);
    for (px, m) in base.chunks_exact_mut(4).zip(mask.iter()) {
        let a = *m as f32 / 255.0;
        px[3] = (px[3] as f32 * (1.0 - a)).min(255.0) as u8;
    }
    let preview = downscale_pixels(&base, w, h, 1024);
    pworker::load(item.id, &base, w, h);
    CACHE.with(|c| {
        if let Some(pd) = c.borrow_mut().photos.get_mut(&item.id) {
            let pd = Rc::make_mut(pd);
            pd.full = Some((Pixels::owned(base), w, h));
            pd.full_dims = (w, h);
            pd.preview = preview;
            pd.dirty = true;
            pd.gen += 1;
        }
    });
    state.update_current(|e| {
        e.rot90 = 0;
        e.fine_angle = 0.0;
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
    if item.edit.selection.is_none() {
        return;
    }
    if !full_present(item.id) {
        ensure_full(state, item.id, Some(Box::new(move || content_fill_selection(state))));
        return;
    }
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            let Some(sel) = item.edit.selection.clone() else { return false };
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let item_id = item.id;
            pworker::fill(item_id, &recipe, &sel, move |res| {
                match res {
                    Ok(Some(r)) => destructive_apply(state, item_id, r),
                    Ok(None) => {}
                    Err(()) => content_fill_selection_sync(state),
                }
            })
        },
        move || content_fill_selection_sync(state),
    );
}

fn content_fill_selection_sync(state: AppState) {
    let Some(item) = state.current() else { return; };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(sel) = item.edit.selection.clone() else { return; };
    let Some(photo) = get_photo(item.id) else { return; };
    let Some((full, fw, fh)) = photo.full.clone() else { return };
    let (mut base, w, h) = owned_geometry(&full, fw, fh, &item.edit);
    let mask = ops::selection_mask(&sel, w, h);
    if !fill::content_fill(&mut base, &mask, w, h) {
        return;
    }
    let preview = downscale_pixels(&base, w, h, 1024);
    pworker::load(item.id, &base, w, h);
    CACHE.with(|c| {
        if let Some(pd) = c.borrow_mut().photos.get_mut(&item.id) {
            let pd = Rc::make_mut(pd);
            pd.full = Some((Pixels::owned(base), w, h));
            pd.full_dims = (w, h);
            pd.preview = preview;
            pd.dirty = true;
            pd.gen += 1;
        }
    });
    state.update_current(|e| {
        e.rot90 = 0;
        e.fine_angle = 0.0;
        e.selection = None;
    });
}

/// Swap an item's cached pixels after a destructive op (worker or sync).
/// Rotation is baked into the new pixels, so it resets; the crop rect is a
/// render-time window over the working frame and is deliberately preserved.
fn destructive_apply(state: AppState, item_id: usize, r: pworker::DestructiveResult) {
    camera_loupe_invalidate(item_id);
    CACHE.with(|c| {
        if let Some(pd) = c.borrow_mut().photos.get_mut(&item_id) {
            let pd = Rc::make_mut(pd);
            pd.full = Some((Pixels::owned(r.pixels), r.w, r.h));
            pd.full_dims = (r.w, r.h);
            pd.preview = (r.preview, r.pw, r.ph);
            pd.dirty = true;
            pd.gen += 1;
        }
    });
    state.items.update(|v| {
        if let Some(m) = v.iter_mut().find(|m| m.id == item_id) {
            m.edit.rot90 = 0;
            m.edit.fine_angle = 0.0;
            m.edit.selection = None;
        }
    });
}

fn heal_mode_u8(m: heal::HealMode) -> u8 {
    match m {
        heal::HealMode::ContentAware => 0,
        heal::HealMode::SmoothFill => 1,
        heal::HealMode::ProximityMatch => 2,
    }
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
    if !full_present(item.id) {
        let pts = pts.to_vec();
        ensure_full(state, item.id, Some(Box::new(move || apply_heal(state, &pts))));
        return;
    }
    let pts = pts.to_vec();
    let pts_sync = pts.clone();
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            if item.kind != MediaKind::Photo {
                return false;
            }
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let radius = state.heal_radius.get_untracked();
            let mode = heal_mode_u8(state.heal_mode.get_untracked());
            let item_id = item.id;
            let pts_fb = pts.clone();
            pworker::heal(item_id, &recipe, &pts, radius, mode, move |res| {
                match res {
                    Ok(Some(r)) => destructive_apply(state, item_id, r),
                    Ok(None) => {}
                    Err(()) => apply_heal_sync(state, &pts_fb),
                }
            })
        },
        move || apply_heal_sync(state, &pts_sync),
    );
}

fn apply_heal_sync(state: AppState, pts: &[(f32, f32)]) {
    if pts.is_empty() {
        return;
    }
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(photo) = get_photo(item.id) else { return };
    let Some((full, fw, fh)) = photo.full.clone() else { return };
    let (mut base, w, h) = owned_geometry(&full, fw, fh, &item.edit);
    let diag = ((w * w + h * h) as f32).sqrt();
    let r = (state.heal_radius.get_untracked() * diag).max(1.0);
    let mut coverage = vec![0u8; w * h];
    stamp_stroke(&mut coverage, w, h, pts, r);
    let mode = state.heal_mode.get_untracked();
    if !heal::spot_heal(&mut base, &coverage, w, h, 1.0, mode, item.id as u32) {
        return;
    }
    let preview = downscale_pixels(&base, w, h, 1024);
    pworker::load(item.id, &base, w, h);
    CACHE.with(|c| {
        if let Some(pd) = c.borrow_mut().photos.get_mut(&item.id) {
            let pd = Rc::make_mut(pd);
            pd.full = Some((Pixels::owned(base), w, h));
            pd.full_dims = (w, h);
            pd.preview = preview;
            pd.dirty = true;
            pd.gen += 1;
        }
    });
    state.update_current(|e| {
        e.rot90 = 0;
        e.fine_angle = 0.0;
        e.selection = None;
    });
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
    if state.clone_source.get_untracked().is_none() {
        return;
    }
    if !full_present(item.id) {
        let pts = pts.to_vec();
        ensure_full(state, item.id, Some(Box::new(move || apply_clone(state, &pts))));
        return;
    }
    let pts = pts.to_vec();
    let pts_sync = pts.clone();
    pworker::destructive(
        move || {
            let Some(item) = state.current() else { return false };
            if item.kind != MediaKind::Photo {
                return false;
            }
            let Some(photo) = get_photo(item.id) else { return false };
            let Some(src) = state.clone_source.get_untracked() else { return false };
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
            let (ww, wh) = working_dims(photo.full_dims.0, photo.full_dims.1, &item.edit);
            let dx = (offset.0 * ww as f32).round() as i32;
            let dy = (offset.1 * wh as f32).round() as i32;
            let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
            let radius = state.clone_radius.get_untracked();
            let item_id = item.id;
            let pts_fb = pts.clone();
            pworker::clone_stamp(item_id, &recipe, &pts, radius, dx, dy, move |res| {
                match res {
                    Ok(Some(r)) => destructive_apply(state, item_id, r),
                    Ok(None) => {}
                    Err(()) => apply_clone_sync(state, &pts_fb),
                }
            })
        },
        move || apply_clone_sync(state, &pts_sync),
    );
}

fn apply_clone_sync(state: AppState, pts: &[(f32, f32)]) {
    if pts.is_empty() {
        return;
    }
    let Some(item) = state.current() else { return };
    if item.kind != MediaKind::Photo {
        return;
    }
    let Some(src) = state.clone_source.get_untracked() else { return };
    let Some(photo) = get_photo(item.id) else { return };
    let Some((full, fw, fh)) = photo.full.clone() else { return };
    let (mut base, w, h) = owned_geometry(&full, fw, fh, &item.edit);
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
    pworker::load(item.id, &base, w, h);
    CACHE.with(|c| {
        if let Some(pd) = c.borrow_mut().photos.get_mut(&item.id) {
            let pd = Rc::make_mut(pd);
            pd.full = Some((Pixels::owned(base), w, h));
            pd.full_dims = (w, h);
            pd.preview = preview;
            pd.dirty = true;
            pd.gen += 1;
        }
    });
    state.update_current(|e| {
        e.rot90 = 0;
        e.fine_angle = 0.0;
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
    if !item.edit.raw.is_identity() {
        rawops::apply(p.to_mut(), w, h, &item.edit.raw);
    }
    if item.edit.is_color_touched() {
        blur::blur_apply(p.to_mut(), w, h, item.edit.blur);
        color::apply_color(p.to_mut(), &item.edit);
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
    let channel = create_rw_signal(0usize); // 0=RGB, 1=R, 2=G, 3=B, shared by Levels + Curves
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
        <RawPanel state=state />
        {slider("Brightness", |e| e.brightness, |e, v| e.brightness = v)}
        {slider("Contrast", |e| e.contrast, |e, v| e.contrast = v)}
        {slider("Saturation", |e| e.saturation, |e, v| e.saturation = v)}
        {slider("Warmth", |e| e.warmth, |e, v| e.warmth = v)}
        <BlurPanel state=state />
        <LevelsPanel state=state channel=channel />
        <CurvesPanel state=state channel=channel />
        <GrainPanel state=state />
        <button class="btn dim-btn" on:click=move |_| state.update_current(|e| {
            e.brightness = 0.0; e.contrast = 0.0; e.saturation = 0.0; e.warmth = 0.0;
        })>"Reset color"</button>
    }
}

/// Linear-light RAW develop controls. Shown for RAW sources and for any item
/// carrying RAW edits (a healed RAW becomes a raster but keeps its recipe).
/// JPEGs have no develop stage, so the panel stays hidden and their recipes
/// never grow RAW fields.
#[component]
fn RawPanel(state: AppState) -> impl IntoView {
    let is_raw = move || {
        state
            .current()
            .map(|m| {
                !m.edit.raw.is_identity()
                    || get_photo(m.id).map(|p| p.raw).unwrap_or(false)
            })
            .unwrap_or(false)
    };
    let rslider = move |label: &'static str,
                        min: f64,
                        max: f64,
                        step: f64,
                        get: fn(&RawAdjust) -> f32,
                        set: fn(&mut RawAdjust, f32),
                        fmt: fn(f32) -> String| {
        view! {
            <label class="slider">
                <span>{label} ": " {move || {
                    let Some(m) = state.current() else { return "—".into() };
                    fmt(get(&m.edit.raw))
                }}</span>
                <input
                    type="range" min=min.to_string() max=max.to_string() step=step.to_string()
                    prop:value=move || state.current()
                        .map(|m| get(&m.edit.raw).to_string())
                        .unwrap_or_default()
                    on:input=move |ev| {
                        let v: f32 = event_target_value(&ev).parse().unwrap_or(0.0);
                        state.update_current(|e| set(&mut e.raw, v));
                    }
                />
            </label>
        }
    };
    view! {
        <Show when=is_raw fallback=|| ()>
            <div class="raw-panel">
                <div class="panel-title">"RAW develop"</div>
                {rslider(
                    "Temperature", 2500.0, 9500.0, 50.0,
                    |r| r.temp, |r, v| r.temp = v,
                    |v| format!("{v:.0} K"),
                )}
                {rslider(
                    "Tint", -100.0, 100.0, 1.0,
                    |r| r.tint, |r, v| r.tint = v,
                    |v| format!("{v:+.0}"),
                )}
                {rslider(
                    "Exposure", -5.0, 5.0, 0.05,
                    |r| r.exposure, |r, v| r.exposure = v,
                    |v| format!("{v:+.2} EV"),
                )}
                {rslider(
                    "Highlights", 0.0, 100.0, 1.0,
                    |r| r.highlights, |r, v| r.highlights = v,
                    |v| format!("{v:.0}"),
                )}
                {rslider(
                    "Shadows", 0.0, 100.0, 1.0,
                    |r| r.shadows, |r, v| r.shadows = v,
                    |v| format!("{v:.0}"),
                )}
                {rslider(
                    "Shadow NR", 0.0, 100.0, 1.0,
                    |r| r.noise, |r, v| r.noise = v,
                    |v| format!("{v:.0}"),
                )}
                {rslider(
                    "Smart denoise", 0.0, 100.0, 1.0,
                    |r| r.denoise, |r, v| r.denoise = v,
                    |v| format!("{v:.0}"),
                )}
                {rslider(
                    "Demosaic sharpening", 0.0, 100.0, 1.0,
                    |r| r.sharpen, |r, v| r.sharpen = v,
                    |v| format!("{v:.0}"),
                )}
                <button class="btn dim-btn" on:click=move |_| {
                    state.update_current(|e| e.raw = RawAdjust::default());
                }>"Reset RAW"</button>
            </div>
        </Show>
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

/// Histogram fill for a levels channel: 0=RGB, 1=R, 2=G, 3=B.
fn channel_color(ch: usize) -> &'static str {
    match ch {
        1 => "rgba(255,69,58,0.85)",
        2 => "rgba(48,209,88,0.85)",
        3 => "rgba(10,132,255,0.9)",
        _ => "rgba(255,255,255,0.75)",
    }
}

/// Curve stroke for a curves channel (RGB/R/G/B).
fn curve_color(ch: usize, alpha: f64) -> String {
    let (r, g, b) = match ch {
        1 => (255, 69, 58),
        2 => (48, 209, 88),
        3 => (10, 132, 255),
        _ => (242, 242, 247),
    };
    format!("rgba({r},{g},{b},{alpha})")
}

#[component]
fn LevelsPanel(state: AppState, channel: RwSignal<usize>) -> impl IntoView {
    let hist_ref = create_node_ref::<html::Canvas>();
    // Which Auto button produced the current settings. Cleared as soon as the
    // levels are edited by hand or the selection moves to another item.
    let last_auto = create_rw_signal(None::<levels::AutoMode>);
    create_effect(move |_| {
        state.selected.track();
        last_auto.set(None);
    });

    // Redraw the histogram whenever the photo, its edits, or the channel change.
    create_effect(move |_| {
        state.items.track();
        state.selected.track();
        channel.track();
        let Some(canvas) = hist_ref.get() else { return };
        let Some(item) = state.current() else { return };
        if item.kind != MediaKind::Photo {
            return;
        }
        let Some(photo) = get_photo(item.id) else { return };
        let ch = channel.get_untracked();
        let bins = levels::histogram(&photo.preview.0);
        let scale = levels::histogram_display_scale(&bins[ch]);
        let ctx = web::ctx2d(&canvas);
        ctx.clear_rect(0.0, 0.0, 256.0, 64.0);
        if scale <= 0.0 {
            return;
        }
        ctx.set_fill_style_str(channel_color(ch));
        for (i, &b) in bins[ch].iter().enumerate() {
            let h = (b / scale * 64.0).min(64.0);
            if h >= 0.5 {
                ctx.fill_rect(i as f64, 64.0 - h, 1.0, h);
            }
        }
    });

    let dirty = move |idx: usize| {
        state
            .current()
            .map(|m| {
                let r = &m.edit.levels.ranges;
                if idx == 0 {
                    (1..4).any(|ch| r[ch] != levels::LevelRange::default())
                } else {
                    r[idx] != levels::LevelRange::default()
                }
            })
            .unwrap_or(false)
    };

    let channel_btn = move |idx: usize, label: &'static str| {
        view! {
            <button
                class=move || format!(
                    "btn small chan-{idx}{}",
                    if channel.get() == idx { " active" } else { "" }
                )
                on:click=move |_| channel.set(idx)
            >
                {label}
                <span class=move || if dirty(idx) { "chan-dot on" } else { "chan-dot" }></span>
            </button>
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
                    let Some(m) = state.current() else { return "—".into() };
                    let ch = channel.get();
                    let ranges = &m.edit.levels.ranges;
                    let mut s = fmt(get(&ranges[ch]));
                    // In the RGB view, show the per-channel values once any
                    // channel differs from its default, so Auto color/neutral
                    // visibly move something even when they leave ranges[0] alone.
                    if ch == 0 && (1..4).any(|i| ranges[i] != levels::LevelRange::default()) {
                        s.push_str(&format!(
                            " · R {} G {} B {}",
                            fmt(get(&ranges[1])),
                            fmt(get(&ranges[2])),
                            fmt(get(&ranges[3])),
                        ));
                    }
                    s
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
                        last_auto.set(None);
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
        last_auto.set(Some(mode));
    };

    let auto_btn = move |mode: levels::AutoMode, label: &'static str| {
        view! {
            <button
                class=move || format!(
                    "btn small{}",
                    if last_auto.get() == Some(mode) { " active" } else { "" }
                )
                on:click=move |_| auto(mode)
            >{label}</button>
        }
    };

    let applied = move || {
        let Some(mode) = last_auto.get() else { return ().into_view() };
        let Some(m) = state.current() else { return ().into_view() };
        let r = &m.edit.levels.ranges;
        let span = |i: usize| format!("{:.0}–{:.0}", r[i].black, r[i].white);
        let title = match mode {
            levels::AutoMode::Contrast => "Auto contrast applied",
            levels::AutoMode::Color => "Auto color applied",
            levels::AutoMode::ColorNeutral => "Auto neutral applied",
        };
        let detail = if mode == levels::AutoMode::Contrast {
            format!("black {:.0} · white {:.0}", r[0].black, r[0].white).into_view()
        } else {
            let gamma = if mode == levels::AutoMode::ColorNeutral {
                format!(
                    " · gamma {:.2} / {:.2} / {:.2}",
                    r[1].gamma, r[2].gamma, r[3].gamma
                )
            } else {
                String::new()
            };
            view! {
                <>
                    <span class="las-r">{format!("R {}", span(1))}</span>" · "
                    <span class="las-g">{format!("G {}", span(2))}</span>" · "
                    <span class="las-b">{format!("B {}", span(3))}</span>
                    {gamma}
                </>
            }.into_view()
        };
        view! {
            <div class="levels-auto-status">
                <span class="las-title">{title}</span>" · "{detail}
            </div>
        }.into_view()
    };

    view! {
        <div class="levels-panel">
            <div class="panel-title">"Levels"</div>
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
                {auto_btn(levels::AutoMode::Contrast, "Auto contrast")}
                {auto_btn(levels::AutoMode::Color, "Auto color")}
                {auto_btn(levels::AutoMode::ColorNeutral, "Auto neutral")}
                <button class="btn small dim-btn" on:click=move |_| {
                    state.update_current(|e| e.levels = levels::LevelsSettings::default());
                    last_auto.set(None);
                }>"Reset levels"</button>
            </div>
            {applied}
        </div>
    }
}

#[component]
fn CurvesPanel(state: AppState, channel: RwSignal<usize>) -> impl IntoView {
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
        // Other channels as faint ghosts so an edit on one channel is
        // distinguishable from the untouched ones.
        for other in 0..4 {
            if other == ch {
                continue;
            }
            ctx.set_stroke_style_str(&curve_color(other, 0.22));
            ctx.set_line_width(1.0);
            ctx.begin_path();
            for x in 0..=255 {
                let y = item.edit.curves.value(x as f64, other);
                let px = x as f64 / 255.0 * 256.0;
                let py = (1.0 - y / 255.0) * 256.0;
                if x == 0 {
                    ctx.move_to(px, py);
                } else {
                    ctx.line_to(px, py);
                }
            }
            ctx.stroke();
        }
        ctx.set_stroke_style_str(&curve_color(ch, 1.0));
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
            if sel == Some(i) {
                ctx.set_fill_style_str("#ffffff");
                ctx.fill();
                ctx.set_stroke_style_str(&curve_color(ch, 1.0));
                ctx.set_line_width(2.0);
                ctx.stroke();
            } else {
                ctx.set_fill_style_str(&curve_color(ch, 1.0));
                ctx.fill();
            }
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

    let dirty = move |idx: usize| {
        state
            .current()
            .map(|m| {
                if idx == 0 {
                    (1..4).any(|ch| !m.edit.curves.channel_is_identity(ch))
                } else {
                    !m.edit.curves.channel_is_identity(idx)
                }
            })
            .unwrap_or(false)
    };

    let channel_btn = move |idx: usize, label: &'static str| {
        view! {
            <button
                class=move || format!(
                    "btn small chan-{idx}{}",
                    if channel.get() == idx { " active" } else { "" }
                )
                on:click=move |_| {
                    channel.set(idx);
                    selected.set(None);
                    dragging.set_value(None);
                }
            >
                {label}
                <span class=move || if dirty(idx) { "chan-dot on" } else { "chan-dot" }></span>
            </button>
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
            <div class="panel-title">"Curves"</div>
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
                    let ch = channel.get();
                    let detail = selected.get().and_then(|i| {
                        state.current().and_then(|m| {
                            m.edit.curves.channels[ch]
                                .get(i)
                                .map(|p| format!("Input {} · Output {}", p.x as i32, p.y as i32))
                        })
                    })
                    .unwrap_or_else(|| "Click to add a point. Drag to adjust.".into());
                    format!("{} — {detail}", ["RGB", "R", "G", "B"][ch])
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
    let edited_count = move || state.items.with(|v| v.iter().filter(|m| item_edited(m)).count());
    let save_count = move || state.items.with(|v| v.iter().filter(|m| photo_needs_save(m)).count());
    let drive_ready = move || {
        state.drive_token.with(|t| t.is_some()) && state.drive_folder.with(|f| f.is_some())
    };
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
            <button class="btn" disabled=move || edited_count() == 0 on:click=move |_| {
                download_edited_zip(state);
            }>{move || format!("Download {} edited (zip)", edited_count())}</button>
            <Show when=drive_ready fallback=|| ()>
                <button class="btn" disabled=move || save_count() == 0 on:click=move |_| {
                    save_edited_to_drive(state);
                }>{move || format!("Save {} edited to Drive", save_count())}</button>
            </Show>
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
