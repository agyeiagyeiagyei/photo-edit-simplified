//! Non-reactive media cache: decoded photo state, the worker-owned full-resolution budget and its LRU.

use crate::prelude::*;
use crate::*;

/// RGBA pixels the app holds without copying them into Rust when they came
/// back from a worker: a view over the transferred ArrayBuffer, or an owned
/// buffer for main-thread decodes and destructive-op results.
#[derive(Clone)]
pub(crate) enum Pixels {
    View(Uint8Array),
    Owned(Rc<Vec<u8>>),
}

impl Pixels {
    pub(crate) fn view(v: Uint8Array) -> Pixels {
        Pixels::View(v)
    }

    pub(crate) fn owned(v: Vec<u8>) -> Pixels {
        Pixels::Owned(Rc::new(v))
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Pixels::View(v) => v.length() as usize,
            Pixels::Owned(v) => v.len(),
        }
    }

    /// Borrow owned pixels; a view has to be materialised first.
    pub(crate) fn as_cow(&self) -> Cow<'_, [u8]> {
        match self {
            Pixels::Owned(v) => Cow::Borrowed(v.as_slice()),
            Pixels::View(v) => Cow::Owned(v.to_vec()),
        }
    }

    pub(crate) fn put_on_canvas(&self, canvas: &web_sys::HtmlCanvasElement, w: u32, h: u32) {
        match self {
            Pixels::View(v) => web::put_pixels_view(canvas, v, w, h),
            Pixels::Owned(v) => web::put_pixels(canvas, v, w, h),
        }
    }

    pub(crate) fn load_into_worker(&self, id: usize, w: usize, h: usize) {
        match self {
            Pixels::View(v) => pworker::load_view(id, v, w, h),
            Pixels::Owned(v) => pworker::load(id, v, w, h),
        }
    }

    /// Hand the buffer to the worker without keeping a main-thread copy. The
    /// view is detached; only call this when the pixels are not used again.
    pub(crate) fn transfer_to_worker(&self, id: usize, w: usize, h: usize) -> bool {
        match self {
            Pixels::View(v) => pworker::load_view_transfer(id, v, w, h),
            Pixels::Owned(v) => {
                pworker::load(id, v, w, h);
                true
            }
        }
    }

    /// Copy a window of rows out of the buffer without materialising the whole
    /// image (the focus loupe runs this on every pointer move).
    pub(crate) fn crop_rows(&self, w: usize, sy: usize, sx: usize, cw: usize, ch: usize) -> Vec<u8> {
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

    pub(crate) fn downscale(&self, w: usize, h: usize, long_edge: u32) -> (Vec<u8>, usize, usize) {
        resample::fit_long_edge(&self.as_cow(), w, h, long_edge)
    }
}

/// A RAW item's un-toned 16-bit develop: the source the neutral pipeline
/// stretches highlights on. Kept beside the camera-toned u8 `full` so a RAW
/// slider tick and the export both start from unclipped linear data.
#[derive(Clone)]
pub(crate) struct NeutralData {
    /// JS view over the develop transferred back from the RAW worker.
    pub(crate) pixels: Uint16Array,
    pub(crate) w: usize,
    pub(crate) h: usize,
    /// Un-toned white level, so highlight thresholds scale to the sensor.
    pub(crate) white: f32,
    /// Camera-tone curve applied after the develop stages.
    pub(crate) tone: Option<[u8; 256]>,
}

/// The result of a full sensel develop: the camera-toned u8 frame plus the
/// neutral develop the render worker needs for the neutral path.
pub(crate) struct FullDevelop {
    pub(crate) pixels: Pixels,
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) neutral: Option<NeutralData>,
}

#[derive(Clone)]
pub(crate) struct PhotoData {
    /// True when the pixel worker holds this item's full-res RGBA under the
    /// item id. The app thread never keeps a copy; eviction unloads it.
    /// False until materialized (imports decode preview-only).
    pub(crate) full: bool,
    /// True when the worker also holds the un-toned 16-bit develop (RAW
    /// only), which the neutral render path starts from.
    pub(crate) neutral: bool,
    /// Source dimensions, known without holding the pixels.
    pub(crate) full_dims: (usize, usize),
    pub(crate) preview: (Vec<u8>, usize, usize),
    /// A destructive op landed since the last session save persisted the
    /// raster — until then this cache copy is the only full-res source of
    /// truth, so dirty items are exempt from eviction.
    pub(crate) dirty: bool,
    /// Bumped whenever full/preview are rebuilt in place (heal, cut, …) so the
    /// hi-res render cache knows its source pixels changed.
    pub(crate) gen: u64,
    /// RAW import: full pixels are developed lazily from the original bytes,
    /// which live in IndexedDB rather than in memory.
    pub(crate) raw: bool,
    /// Original RAW byte length — keeps the session signature stable once the
    /// bytes are no longer held in memory.
    pub(crate) raw_len: u64,
    /// The original made it into the raw_src store. False means the fallback
    /// is in play: full pixels were developed eagerly and the bytes stay in
    /// `raw_fallback` so the session record can embed them.
    pub(crate) raw_persisted: bool,
}

pub(crate) struct Cache {
    pub(crate) photos: HashMap<usize, Rc<PhotoData>>,
    /// Compressed source blobs (JPEG/PNG/HEIC-converted) for on-demand
    /// full-res re-decode. File-backed for local imports, so cheap to hold.
    pub(crate) photo_src: HashMap<usize, Blob>,
    pub(crate) video_blobs: HashMap<usize, Blob>,
    pub(crate) video_meta: HashMap<usize, (f64, u32, u32)>, // duration, w, h
    /// RAW originals that could not be written to IndexedDB (quota, private
    /// mode) — only kept so persistence can fall back to the old embedded-blob
    /// record. Empty on the normal path.
    pub(crate) raw_fallback: HashMap<usize, Rc<Vec<u8>>>,
}

thread_local! {
    pub(crate) static CACHE: RefCell<Cache> = RefCell::new(Cache {
        photos: HashMap::new(),
        photo_src: HashMap::new(),
        video_blobs: HashMap::new(),
        video_meta: HashMap::new(),
        raw_fallback: HashMap::new(),
    });
}

pub(crate) fn get_photo(id: usize) -> Option<Rc<PhotoData>> {
    CACHE.with(|c| c.borrow().photos.get(&id).cloned())
}

/// Decoded full-res budget for the worker's buffers. The worker is the only
/// owner now, so there is no mirror to budget for: 1 GB on devices reporting
/// at least 8 GB of RAM, else 512 MB. Dirty items (a destructive op landed
/// since the last session save) are exempt — the worker copy is the only
/// source of truth until persisted.
pub(crate) fn full_budget() -> usize {
    FULL_BUDGET.with(|b| {
        if b.get() == 0 {
            let gb = js_sys::Reflect::get(&web::window(), &JsValue::from_str("deviceMemory"))
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(4.0);
            b.set(if gb >= 8.0 { 1 << 30 } else { 512 << 20 });
        }
        b.get()
    })
}

thread_local! {
    pub(crate) static FULL_BUDGET: Cell<usize> = const { Cell::new(0) };
    pub(crate) static FULL_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    pub(crate) static FULL_WAITERS: RefCell<HashMap<usize, Vec<Box<dyn FnOnce()>>>> =
        RefCell::new(HashMap::new());
    pub(crate) static LAST_USE: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    pub(crate) static USE_TICK: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn full_present(id: usize) -> bool {
    CACHE.with(|c| {
        c.borrow()
            .photos
            .get(&id)
            .map(|p| p.full)
            .unwrap_or(false)
    })
}

pub(crate) fn touch_full(id: usize) {
    let t = USE_TICK.with(|t| {
        t.set(t.get() + 1);
        t.get()
    });
    LAST_USE.with(|l| l.borrow_mut().insert(id, t));
}

/// Worker bytes a decoded item keeps alive (u8 frame + 16-bit develop).
pub(crate) fn photo_bytes(p: &PhotoData) -> usize {
    if !p.full {
        return 0;
    }
    let px = p.full_dims.0 * p.full_dims.1;
    px * 4 + if p.neutral { px * 2 } else { 0 }
}

/// Drop least-recently-used clean full-res buffers until the decoded total
/// fits the budget; the worker's copies go with each eviction.
pub(crate) fn evict_full_lru() {
    let evicted: Vec<usize> = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let budget = full_budget();
        let mut total: usize = c.photos.values().map(|p| photo_bytes(p)).sum();
        if total <= budget {
            return Vec::new();
        }
        let mut candidates: Vec<(u64, usize, usize)> = c
            .photos
            .iter()
            .filter(|(_, p)| !p.dirty && p.full)
            .map(|(id, p)| {
                (
                    LAST_USE.with(|l| l.borrow().get(id).copied()).unwrap_or(0),
                    *id,
                    photo_bytes(p),
                )
            })
            .collect();
        candidates.sort_by_key(|c| c.0);
        let mut evicted = Vec::new();
        for (_, id, len) in candidates {
            if total <= budget {
                break;
            }
            if let Some(p) = c.photos.get_mut(&id) {
                let p = Rc::make_mut(p);
                p.full = false;
                p.neutral = false;
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

/// Record that the worker now holds a freshly decoded full-res frame. The
/// pixels themselves never come to this thread; only the flags land here.
/// Runs waiters and nudges items so pending hi-res renders reschedule.
pub(crate) fn materialize_full(state: AppState, id: usize, dims: (usize, usize), neutral: bool) {
    // Sensor pixels supersede the display proxy (and any render it fed).
    if proxy_in_mem(id) {
        drop_proxy(id);
    }
    CACHE.with(|c| {
        if let Some(p) = c.borrow_mut().photos.get_mut(&id) {
            let p = Rc::make_mut(p);
            p.full_dims = dims;
            p.full = true;
            p.neutral = neutral;
        }
    });
    touch_full(id);
    evict_full_lru();
    let waiters = FULL_WAITERS.with(|w| w.borrow_mut().remove(&id)).unwrap_or_default();
    for w in waiters {
        w();
    }
    state.items.update(|_| {});
    // An `items` update raised from this spawned task does not flush the
    // render effect (the restored selection stayed on its 1024px preview);
    // `hi_built` is the signal that effect listens to for exactly this.
    state.hi_built.update(|g| *g += 1);
}

/// Install an eagerly developed RAW frame (the import fallback when the
/// original could not be stored) into the worker. Returns the cache flags.
pub(crate) fn install_full_develop(id: usize, full: Option<FullDevelop>) -> (bool, bool) {
    let Some(mut f) = full else { return (false, false) };
    let neutral = f.neutral.take();
    if !f.pixels.transfer_to_worker(id, f.w, f.h) {
        return (false, false);
    }
    let has_neutral = match &neutral {
        Some(n) => pworker::load_neutral_transfer(
            id,
            &n.pixels,
            n.w,
            n.h,
            n.white,
            n.tone.as_ref(),
        ),
        None => false,
    };
    touch_full(id);
    (true, has_neutral)
}

/// Decode an item's full-res pixels on demand from its source blob (healed
/// PNG or imported JPEG/PNG) or RAW bytes. No-op when present or in flight;
/// `waiter` runs once the buffer lands.
pub(crate) fn ensure_full(state: AppState, id: usize, waiter: Option<Box<dyn FnOnce()>>) {
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
        // Whereever the pixels come from, they are installed in the worker
        // and never reach this thread: a non-RAW source is decoded inside the
        // worker straight from its Blob, and a RAW develop is transferred
        // from the RAW worker into the interactive one.
        let mut installed: Option<((usize, usize), bool)> = None;
        if is_raw {
            // Lazy develop: hand the stored Blob to the RAW worker, then move
            // its transferred views into the interactive worker.
            let decoded = match session::load_raw_blob(id).await {
                Some(blob) => raw_develop_async(id, blob.as_ref()).await,
                None => None,
            };
            if decoded.is_some() && !pworker::raw_busy() {
                // The develop is in the interactive worker now; the RAW
                // worker's wasm (hundreds of MB after a 24 MP develop, and
                // never shrinking) would otherwise sit idle through composite
                // + encode — the peak of the whole export. Recreated lazily.
                pworker::raw_shutdown();
            }
            if let Some(mut full) = decoded {
                let neutral = full.neutral.take();
                let dims = (full.w, full.h);
                let ok = full.pixels.transfer_to_worker(id, full.w, full.h);
                let neutral_ok = match &neutral {
                    Some(n) => pworker::load_neutral_transfer(
                        id,
                        &n.pixels,
                        n.w,
                        n.h,
                        n.white,
                        n.tone.as_ref(),
                    ),
                    None => false,
                };
                if ok {
                    installed = Some((dims, neutral_ok));
                }
            }
        } else if let Some(blob) = src {
            installed = await_worker(move |cb| pworker::load_photo(id, blob.as_ref(), cb))
                .await
                .map(|dims| (dims, false));
        }
        FULL_INFLIGHT.with(|f| f.borrow_mut().remove(&id));
        let still_tracked = CACHE.with(|c| c.borrow().photos.contains_key(&id));
        match (installed, still_tracked) {
            (Some((dims, neutral)), true) => materialize_full(state, id, dims, neutral),
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

pub(crate) async fn sleep_ms(ms: i32) {
    let p = js_sys::Promise::new(&mut |res, _| {
        let _ = web::window().set_timeout_with_callback_and_timeout_and_arguments_0(&res, ms);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(p).await;
}

/// Awaitable ensure_full for the export path. False when undecodable.
pub(crate) async fn ensure_full_async(state: AppState, id: usize) -> bool {
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

// A RAW import shows the camera's embedded JPEG immediately, then develops a
// display-resolution proxy in a second worker. Proxies live outside PhotoData:
// their lifetime is independent of preview rebuilds, and their worker ids sit
// in a separate namespace so a proxy and the sensor buffer can coexist for the
// same item while the develop lands.
