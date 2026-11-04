//! Display proxies for RAW items: camera-JPEG-first decode, proxy cache/pump/persist, the RAW worker pool, and preview prefetching.

use crate::prelude::*;
use crate::*;

/// Long edge of a display proxy — the same cap as the hi-res cache, so the
/// settled canvas renders from a proxy at full display density.
pub(crate) const PROXY_EDGE: u32 = HI_MAX_EDGE as u32;

/// Worker id namespace for proxy buffers (item ids stay small).
pub(crate) const PROXY_WID: usize = 0x8000_0000;

/// Main-side proxy budget. Each live proxy is mirrored in the worker, so the
/// real footprint is ~2x this (three 3200×2133 proxies).
pub(crate) const PROXY_BUDGET: usize = 160 << 20;

/// The half-size sensor proxy develops in ~1 s; wait only briefly so the
/// selection settles without making a wasted develop expensive.
pub(crate) const PROXY_DEVELOP_DELAY_MS: i32 = 750;

pub(crate) struct ProxyData {
    pub(crate) pixels: Pixels,
    pub(crate) w: usize,
    pub(crate) h: usize,
    /// True sensor-frame dims (the proxy pixels are capped below this).
    pub(crate) frame: (usize, usize),
    /// False while this is the camera JPEG, true once sensor pixels landed.
    pub(crate) developed: bool,
    /// Whether the render worker holds this proxy's un-toned 16-bit develop.
    /// A stored (reloaded) JPEG proxy has none until it re-develops.
    pub(crate) neutral: bool,
}

thread_local! {
    pub(crate) static PROXY_CACHE: RefCell<HashMap<usize, Rc<ProxyData>>> = RefCell::new(HashMap::new());
    pub(crate) static PROXY_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    pub(crate) static PROXY_LAST_USE: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    /// Proxy-content version, mixed into render fingerprints. Never use
    /// photo.gen for this — for RAW items gen > 0 means "healed".
    pub(crate) static PROXY_GEN: RefCell<HashMap<usize, u64>> = RefCell::new(HashMap::new());
    /// Item whose sensor develop the UI is waiting on.
    pub(crate) static PROXY_UPGRADE_WANT: Cell<Option<usize>> = const { Cell::new(None) };
    pub(crate) static PROXY_UPGRADE_BUSY: Cell<bool> = const { Cell::new(false) };
    pub(crate) static PROXY_UPGRADE_TIMER: Cell<u32> = const { Cell::new(0) };
    /// Newest hover decode. A request superseded while another decode runs is
    /// dropped; selected-item requests are never gated.
    pub(crate) static PROXY_DECODE_WANT: Cell<Option<usize>> = const { Cell::new(None) };
    /// Tone curve fitted when an item's display proxy developed, keyed by item.
    /// The export develop passes it explicitly so pixels match the screen even
    /// when the develop runs in a fresh worker (pool develop, reload).
    pub(crate) static RAW_TONES: RefCell<HashMap<usize, [u8; 256]>> = RefCell::new(HashMap::new());
    /// Items whose display proxy a batch pool worker is developing. The
    /// interactive pump must not start a duplicate develop for these.
    pub(crate) static POOL_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    /// Queued + in-flight pool develops for the current import batch.
    pub(crate) static POOL_JOBS: Cell<usize> = const { Cell::new(0) };
    /// Open ingest scopes (overlapping drops share one batch mark).
    pub(crate) static POOL_SCOPES: Cell<usize> = const { Cell::new(0) };
    pub(crate) static POOL_BATCH_START: Cell<f64> = const { Cell::new(0.0) };
    pub(crate) static POOL_BATCH_FILES: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn proxy_wid(id: usize) -> usize {
    PROXY_WID | id
}

pub(crate) fn proxy_gen(id: usize) -> u64 {
    PROXY_GEN.with(|g| g.borrow().get(&id).copied().unwrap_or(0))
}

pub(crate) fn bump_proxy_gen(id: usize) {
    PROXY_GEN.with(|g| {
        let mut g = g.borrow_mut();
        *g.entry(id).or_insert(0) += 1;
    });
}

pub(crate) fn proxy_in_mem(id: usize) -> bool {
    PROXY_CACHE.with(|c| c.borrow().contains_key(&id))
}

pub(crate) fn proxy_data(id: usize) -> Option<Rc<ProxyData>> {
    PROXY_CACHE.with(|c| c.borrow().get(&id).cloned())
}

pub(crate) fn proxy_developed(id: usize) -> bool {
    PROXY_CACHE.with(|c| c.borrow().get(&id).map(|p| p.developed).unwrap_or(false))
}

pub(crate) fn proxy_neutral(id: usize) -> bool {
    PROXY_CACHE.with(|c| c.borrow().get(&id).map(|p| p.neutral).unwrap_or(false))
}

/// A restored RAW item can carry a develop its stored proxy never saw: only a
/// sensor re-develop can feed the 16-bit neutral pipeline, so the proxy is
/// upgraded again before the edit renders.
pub(crate) fn proxy_wants_neutral(state: AppState, id: usize) -> bool {
    !proxy_neutral(id)
        && state.items.with(|v| {
            v.iter()
                .find(|m| m.id == id)
                .map(|m| pipeline::neutral_eligible(&m.edit))
                .unwrap_or(false)
        })
}

pub(crate) fn touch_proxy(id: usize) {
    let t = USE_TICK.with(|t| {
        t.set(t.get() + 1);
        t.get()
    });
    PROXY_LAST_USE.with(|l| l.borrow_mut().insert(id, t));
}

/// Forget an item's proxy: worker copy, cache entry, and — so any render the
/// superseded pixels could feed is invalidated — a version bump.
pub(crate) fn drop_proxy(id: usize) {
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
pub(crate) fn purge_proxy(id: usize) {
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
pub(crate) fn evict_proxy_lru(state: AppState, keep: usize) {
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

pub(crate) struct DecodedProxy {
    pub(crate) pixels: Pixels,
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) frame: (usize, usize),
    pub(crate) developed: bool,
    /// Un-toned 16-bit develop at the proxy's dims, when the decode
    /// demosaiced (not for camera JPEGs and stored proxies).
    pub(crate) neutral: Option<NeutralData>,
    /// The pixel worker decoded the record itself and already holds these
    /// pixels; `land_proxy` must not ship them back.
    pub(crate) in_worker: bool,
}

/// Paint a selected, unedited RAW's display proxy straight into the hi cache.
/// An identity edit renders from the proxy to a resample of these same pixels,
/// so seeding the cache skips the worker round trip that would otherwise keep
/// a reopened session on the soft preview for half a second.
pub(crate) fn seed_hi_from_proxy(state: AppState, id: usize) {
    if state.selected.get_untracked() != Some(id) {
        return;
    }
    let Some(photo) = get_photo(id) else { return };
    if !photo.raw || photo.full || photo.dirty {
        return;
    }
    let Some(item) = state.items.with(|v| v.iter().find(|m| m.id == id).cloned()) else {
        return;
    };
    if !photo_unedited_but_detail(&item) {
        return;
    }
    let stand_in = !photo_unedited(&item);
    let Some(p) = proxy_data(id) else { return };
    if p.w == 0 || p.h == 0 {
        return;
    }
    // A frame already on screen for this item wins unless the proxy is
    // strictly larger (a develop upgrade); never downgrade the sharp frame.
    let replace = HI_CACHE.with(|c| {
        let b = c.borrow();
        match b.as_ref() {
            None => true,
            Some(hi) => hi.item_id != id || hi.w < p.w,
        }
    });
    if !replace {
        return;
    }
    let canvas = web::create_canvas(p.w as u32, p.h as u32);
    p.pixels.put_on_canvas(&canvas, p.w as u32, p.h as u32);
    let hi = HiCache {
        item_id: id,
        edit_fp: edit_fingerprint(&item),
        source_fp: source_fingerprint(photo.gen, proxy_gen(id)),
        canvas: Some(canvas),
        bitmap: None,
        w: p.w,
        h: p.h,
        prefetched: false,
        stand_in,
    };
    HI_CACHE.with(|c| *c.borrow_mut() = Some(hi));
}

/// Take the reload-prefetched proxy bitmap (see index.html) if it has arrived
/// and install it as the hi frame. Its decode overlapped the wasm boot, so the
/// first restored frame is a `drawImage`; the record decode that lands later
/// only backs editing. Only a pristine RAW can show the stored display proxy
/// as-is — anything edited renders from the proxy plus its params.
pub(crate) fn seed_hi_from_prefetch(item: &MediaItem, photo: &PhotoData, edit_fp: u64, src_fp: u64) {
    if !photo.raw || photo.full || photo.dirty || !photo_unedited_but_detail(item) {
        return;
    }
    let stand_in = !photo_unedited(item);
    let Some(bmp) = web::take_prefetched_proxy(item.id) else { return };
    let (w, h) = (bmp.width(), bmp.height());
    let replace = w > 0
        && h > 0
        && HI_CACHE.with(|c| {
            let b = c.borrow();
            match b.as_ref() {
                None => true,
                Some(hi) => hi.item_id != item.id || hi.w < w as usize,
            }
        });
    if !replace {
        bmp.close();
        return;
    }
    HI_CACHE.with(|c| {
        *c.borrow_mut() = Some(HiCache {
            item_id: item.id,
            edit_fp,
            source_fp: src_fp,
            canvas: None,
            bitmap: Some(bmp),
            w: w as usize,
            h: h as usize,
            prefetched: true,
            stand_in,
        })
    });
}

/// Seed the prefetched frame and paint it onto the visible canvas if it is the
/// current hi frame. Used both when the prefetch bitmap event fires and on a
/// render pass that has no preview yet, so a restored session reaches its
/// sharp frame without waiting for the low-res preview decode to drive a pass.
pub(crate) fn paint_prefetched_hi(
    item: &MediaItem,
    photo: &PhotoData,
    canvas: Option<&web_sys::HtmlCanvasElement>,
) -> bool {
    let edit_fp = edit_fingerprint(item);
    let src_fp = source_fingerprint(photo.gen, proxy_gen(item.id));
    seed_hi_from_prefetch(item, photo, edit_fp, src_fp);
    let Some(canvas) = canvas else { return false };
    let mut painted = false;
    HI_CACHE.with(|c| {
        let b = c.borrow();
        if let Some(hi) = b.as_ref() {
            if hi.prefetched && hi.item_id == item.id && hi.edit_fp == edit_fp {
                blit_hi_frame(hi, canvas, None);
                painted = true;
            }
        }
    });
    painted
}

/// Install a decoded proxy: cache, worker, dims, version bump. False when the
/// item stopped being lazy (removed, healed, or full pixels landed) meanwhile.
pub(crate) fn land_proxy(state: AppState, id: usize, decoded: DecodedProxy) -> bool {
    let _land = perf::Span::new("proxy.land").detail(id.to_string());
    let lazy = CACHE.with(|c| {
        c.borrow()
            .photos
            .get(&id)
            .map(|p| p.raw && !p.full)
            .unwrap_or(false)
    });
    if !lazy {
        if decoded.in_worker {
            pworker::unload(proxy_wid(id));
        }
        return false;
    }
    // Never downgrade a sensor develop that landed while this was decoding.
    if !decoded.developed && proxy_developed(id) {
        if decoded.in_worker {
            pworker::unload(proxy_wid(id));
        }
        return false;
    }
    if !decoded.in_worker {
        decoded.pixels.load_into_worker(proxy_wid(id), decoded.w, decoded.h);
    }
    if let Some(n) = &decoded.neutral {
        pworker::load_neutral_view(
            proxy_wid(id),
            &n.pixels,
            n.w,
            n.h,
            n.white,
            n.tone.as_ref(),
        );
    }
    let first_land = !proxy_in_mem(id);
    let (dw, dh) = (decoded.w, decoded.h);
    CACHE.with(|c| {
        if let Some(p) = c.borrow_mut().photos.get_mut(&id) {
            let p = Rc::make_mut(p);
            if !p.full && p.raw && decoded.frame.0 > 0 && decoded.frame.1 > 0 {
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
                neutral: decoded.neutral.is_some(),
            }),
        );
    });
    touch_proxy(id);
    bump_proxy_gen(id);
    // A first land of the record the prefetch bitmap came from: the seeded
    // frame is the same pixels, so stamp it with the new proxy generation
    // rather than schedule a redundant worker render.
    if first_land {
        let photo_gen = CACHE.with(|c| c.borrow().photos.get(&id).map(|p| p.gen).unwrap_or(0));
        HI_CACHE.with(|c| {
            if let Some(hi) = c.borrow_mut().as_mut() {
                if hi.prefetched && hi.item_id == id && hi.w == dw && hi.h == dh {
                    hi.source_fp = source_fingerprint(photo_gen, proxy_gen(id));
                    hi.prefetched = false;
                }
            }
        });
    }
    evict_proxy_lru(state, id);
    seed_hi_from_proxy(state, id);
    state.items.update(|_| {});
    // Same spawned-task caveat as `materialize_full`: an `items` update from
    // an async decode does not flush the render effect, so a restored RAW
    // would stay on its 256px preview instead of re-rendering from the proxy.
    state.hi_built.update(|g| *g += 1);
    true
}

pub(crate) struct StoredProxy {
    pub(crate) kind: String,
    pub(crate) orientation: u16,
    /// Blob for records written since the zero-copy change; an ArrayBuffer for
    /// records persisted before it. Both are handed to the worker as-is.
    pub(crate) bytes: JsValue,
    pub(crate) frame: (usize, usize),
    /// Tone curve the pixels were developed with; absent for camera JPEGs and
    /// records written before the curve was persisted.
    pub(crate) tone: Option<[u8; 256]>,
}

pub(crate) fn camera_proxy_record(jpeg: &Uint8Array, orientation: u16, frame: (usize, usize)) -> JsValue {
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

pub(crate) fn developed_proxy_record(
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

pub(crate) fn display_proxy_record(blob: &Blob, frame: (usize, usize)) -> JsValue {
    let o = js_sys::Object::new();
    session::jset(&o, "kind", "display");
    session::jset(&o, "orientation", 1.0);
    session::jset(&o, "bytes", blob);
    session::jset(&o, "full_w", frame.0 as f64);
    session::jset(&o, "full_h", frame.1 as f64);
    o.into()
}

/// Encode a display proxy through the worker's WS5 JPEG encoder (4:4:4 +
/// sRGB ICC): canvas.toBlob's default 4:2:0 chroma smears colour on the
/// pixels edits are judged against, and the encode stays off the main
/// thread. Falls back to a q0.9 canvas encode when the worker is down.
pub(crate) async fn encode_proxy_jpeg(px: &Pixels, w: usize, h: usize) -> Option<Blob> {
    let view = match px {
        Pixels::View(v) => Some(v.clone()),
        Pixels::Owned(v) => Some(Uint8Array::from(v.as_slice())),
    };
    if let Some(view) = view {
        let bytes = await_worker(move |cb| {
            pworker::encode_jpeg_copy(&view, w as u32, h as u32, 95, cb)
        })
        .await;
        if let Some(bytes) = bytes {
            if let Ok(blob) = web::bytes_to_blob(&bytes, "image/jpeg") {
                return Some(blob);
            }
        }
    }
    let canvas = web::create_canvas(w as u32, h as u32);
    px.put_on_canvas(&canvas, w as u32, h as u32);
    web::canvas_to_jpeg_blob(&canvas, 0.9).await.ok()
}

/// Background proxy persistence shares the pixel worker's FIFO with the
/// display. Encoding a proxy takes ~200 ms (q95 4:4:4, 3200 px), and queued
/// straight after a land it delayed the render that shows the new proxy: the
/// camera JPEG stayed on screen 0.4-0.6 s after a develop instead of ~0.2 s
/// (#16). Let the debounced display request be posted first, then wait until
/// nothing is in flight and the worker has been quiet for a moment. A long
/// drag cannot starve persistence: after `PERSIST_MAX_WAIT_MS` it goes ahead.
const PERSIST_SETTLE_MS: i32 = 250;
const PERSIST_QUIET_MS: f64 = 300.0;
const PERSIST_MAX_WAIT_MS: f64 = 10_000.0;

async fn wait_pixel_worker_quiet() {
    sleep_ms(PERSIST_SETTLE_MS).await;
    let t0 = perf::now();
    loop {
        let now = perf::now();
        let quiet = pworker::pixel_in_flight() == 0
            && now - pworker::pixel_last_post_ms() >= PERSIST_QUIET_MS;
        if quiet || now - t0 >= PERSIST_MAX_WAIT_MS {
            return;
        }
        sleep_ms(100).await;
    }
}

/// Persist camera-JPEG pixels already decoded and downscaled to display size.
/// A reload then decodes one ≤3200px JPEG on the main thread instead of
/// re-running the full camera decode in the RAW worker — the difference
/// between a ~0.5 s and a ~2.5 s restore for a 26 MP file.
pub(crate) fn persist_display_proxy(id: usize, px: Pixels, w: usize, h: usize, frame: (usize, usize)) {
    spawn_local(async move {
        wait_pixel_worker_quiet().await;
        // A sensor develop that landed while this waited supersedes it.
        if proxy_developed(id) {
            return;
        }
        let Some(blob) = encode_proxy_jpeg(&px, w, h).await else { return };
        let still_camera = CACHE.with(|c| {
            c.borrow()
                .photos
                .get(&id)
                .map(|p| p.raw && !p.full)
                .unwrap_or(false)
        });
        // A sensor develop landing first makes this record superseded.
        if !still_camera || proxy_developed(id) {
            return;
        }
        let Ok(db) = session::open_db().await else { return };
        let _ = session::put_proxy(&db, id, &display_proxy_record(&blob, frame)).await;
    });
}

pub(crate) fn parse_proxy_record(v: &JsValue) -> Option<StoredProxy> {
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

pub(crate) async fn read_proxy_record(id: usize) -> Option<StoredProxy> {
    let db = session::open_db().await.ok()?;
    let v = session::get_proxy(&db, id).await.ok()??;
    parse_proxy_record(&v)
}

/// Orientation-corrected pixel dims of an embedded JPEG whose own dims are not.
pub(crate) fn oriented_dims(w: usize, h: usize, orientation: u16) -> (usize, usize) {
    if (5..=8).contains(&orientation) {
        (h, w)
    } else {
        (w, h)
    }
}

/// Poll a worker callback until it fires (the raw worker has no awaitable
/// API). 60 s covers a slow develop on a large sensor.
pub(crate) async fn await_worker<T: 'static>(
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

pub(crate) async fn camera_proxy_async(jpeg: &JsValue, orientation: u16) -> Option<pworker::ProxyResult> {
    let jpeg = jpeg.clone();
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::camera_proxy(&jpeg, orientation, PROXY_EDGE, cb)).await;
    perf::record("proxy.camera_decode", perf::now() - t0);
    r
}

pub(crate) async fn raw_proxy_async(id: usize, src: &JsValue) -> Option<pworker::ProxyResult> {
    let src = src.clone();
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::raw_proxy(id, &src, PROXY_EDGE, cb)).await;
    perf::record("proxy.raw_develop", perf::now() - t0);
    store_tone(id, r.as_ref().and_then(|p| p.tone));
    r
}

/// The neutral develop a sensor proxy decode carried, at the proxy's dims.
pub(crate) fn proxy_neutral_data(r: &pworker::ProxyResult) -> Option<NeutralData> {
    (r.neutral.length() > 0).then(|| NeutralData {
        pixels: r.neutral.clone(),
        w: r.w,
        h: r.h,
        white: r.white,
        tone: r.tone,
    })
}

pub(crate) fn store_tone(id: usize, tone: Option<[u8; 256]>) {
    if let Some(t) = tone {
        RAW_TONES.with(|m| {
            m.borrow_mut().insert(id, t);
        });
    }
}

pub(crate) fn tone_of(id: usize) -> Option<[u8; 256]> {
    RAW_TONES.with(|m| m.borrow().get(&id).copied())
}

/// RAW header metadata + preview from the raw worker.
pub(crate) async fn raw_meta_async(src: &JsValue, preview_edge: u32) -> Option<pworker::RawMeta> {
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

/// Full sensor develop in the raw worker. The pixels and the neutral develop
/// come back as views over transferred buffers — no Rust copy on the app
/// thread.
pub(crate) async fn raw_develop_async(id: usize, src: &JsValue) -> Option<FullDevelop> {
    let src = src.clone();
    let tone = tone_of(id);
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::raw_develop(id, &src, tone.as_ref(), cb)).await;
    perf::record("proxy.raw_develop_full", perf::now() - t0);
    r.map(|f| FullDevelop {
        neutral: (f.neutral.length() > 0).then(|| NeutralData {
            pixels: f.neutral,
            w: f.w,
            h: f.h,
            white: f.white,
            tone: f.tone,
        }),
        pixels: Pixels::view(f.pixels),
        w: f.w,
        h: f.h,
    })
}

/// Embedded preview (no demosaic) at display resolution.
pub(crate) async fn raw_preview_async(src: &JsValue) -> Option<pworker::ProxyResult> {
    let src = src.clone();
    let t0 = perf::now();
    let r = await_worker(move |cb| pworker::raw_preview(&src, PROXY_EDGE, cb)).await;
    perf::record("proxy.raw_preview", perf::now() - t0);
    r
}

/// A worker-callable source for an item's original: the IndexedDB Blob
/// (structured-cloned by reference), or the in-memory fallback when the bytes
/// never reached storage.
pub(crate) async fn raw_source_src(id: usize) -> Option<JsValue> {
    if let Some(b) = CACHE.with(|c| c.borrow().raw_fallback.get(&id).cloned()) {
        return Some(Uint8Array::from(&b[..]).into());
    }
    session::load_raw_blob(id).await.map(JsValue::from)
}

/// Decode a stored proxy record. "raw" (sensor develop) and "display"
/// (downscaled camera JPEG) records are already at proxy size — only a JPEG
/// decode remains; a legacy "camera" record holds the full-size embedded JPEG
/// and still needs the WASM decode.
pub(crate) async fn decode_stored_proxy(id: usize, rec: StoredProxy) -> Option<DecodedProxy> {
    if rec.kind == "raw" || rec.kind == "display" {
        let developed = rec.kind == "raw";
        if developed {
            store_tone(id, rec.tone);
        }
        let t = perf::now();
        let blob = match rec.bytes.dyn_into::<Blob>() {
            Ok(b) => b,
            Err(v) => web::view_to_blob(&js_sys::Uint8Array::new(&v), "image/jpeg").ok()?,
        };
        // The worker decodes the record and keeps its own copy; the reply is a
        // transferred buffer, so the app thread does neither the JPEG decode
        // nor the canvas readback. The reload prefetch has already painted
        // this record's bitmap — this decode only backs editing.
        let for_worker = blob.clone();
        let decoded = await_worker(move |cb| {
            pworker::load_photo_view(proxy_wid(id), &for_worker, cb)
        })
        .await;
        let (pixels, w, h, in_worker) = match decoded {
            Some((p, w, h)) => (Pixels::view(p), w, h, true),
            None => {
                let (px, w, h) = web::rgba_from_blob(&blob).await.ok()?;
                (Pixels::owned(px), w, h, false)
            }
        };
        perf::record("proxy.restore_decode", perf::now() - t);
        Some(DecodedProxy {
            pixels,
            w,
            h,
            frame: rec.frame,
            developed,
            neutral: None,
            in_worker,
        })
    } else {
        let r = camera_proxy_async(&rec.bytes, rec.orientation).await?;
        let frame = if rec.frame.0 > 0 { rec.frame } else { (r.full_w, r.full_h) };
        // Rewrite the record at display size so the next reload skips the
        // worker camera decode.
        persist_display_proxy(id, Pixels::view(r.pixels.clone()), r.w, r.h, frame);
        Some(DecodedProxy {
            pixels: Pixels::view(r.pixels),
            w: r.w,
            h: r.h,
            frame,
            developed: false,
            neutral: None,
            in_worker: false,
        })
    }
}

/// First decode for an item with no proxy record: prefer the camera JPEG (and
/// persist a record for next time), else develop fully in the worker.
pub(crate) async fn decode_camera_from_raw(id: usize, src: &JsValue) -> Option<DecodedProxy> {
    if let Some(meta) = raw_meta_async(src, 0).await {
        if let Some(jpeg) = meta.camera_jpeg {
            let orientation = meta.orientation;
            let frame = oriented_dims(meta.camera_w, meta.camera_h, orientation);
            if let Some(r) = camera_proxy_async(&jpeg.clone().into(), orientation).await {
                let frame = if frame.0 > 0 { frame } else { (r.full_w, r.full_h) };
                persist_display_proxy(id, Pixels::view(r.pixels.clone()), r.w, r.h, frame);
                return Some(DecodedProxy {
                    pixels: Pixels::view(r.pixels),
                    w: r.w,
                    h: r.h,
                    frame,
                    developed: false,
                    neutral: None,
                    in_worker: false,
                });
            }
        }
    }
    let r = raw_proxy_async(id, src).await?;
    Some(DecodedProxy {
        neutral: proxy_neutral_data(&r),
        pixels: Pixels::view(r.pixels),
        w: r.w,
        h: r.h,
        frame: (r.full_w, r.full_h),
        developed: true,
        in_worker: false,
    })
}

pub(crate) async fn decode_proxy_source(id: usize, priority: bool) -> Option<DecodedProxy> {
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
pub(crate) fn request_upgrade(state: AppState, id: usize) {
    PROXY_UPGRADE_WANT.with(|w| w.set(Some(id)));
    schedule_pump(state, PROXY_DEVELOP_DELAY_MS);
}

pub(crate) fn schedule_pump(state: AppState, ms: i32) {
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
pub(crate) fn pump_upgrade(state: AppState) {
    if PROXY_UPGRADE_BUSY.with(|b| b.get()) {
        return;
    }
    let Some(id) = PROXY_UPGRADE_WANT.with(|w| w.get()) else { return };
    let Some(photo) = get_photo(id) else { return };
    if !photo.raw || photo.full {
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
    if proxy_developed(id) && !proxy_wants_neutral(state, id) {
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
                DecodedProxy {
                    neutral: proxy_neutral_data(&r),
                    pixels: Pixels::view(r.pixels),
                    w: r.w,
                    h: r.h,
                    frame,
                    developed: true,
                    in_worker: false,
                },
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
pub(crate) fn persist_developed_proxy(
    id: usize,
    px: Pixels,
    w: usize,
    h: usize,
    frame: (usize, usize),
    tone: Option<[u8; 256]>,
) {
    spawn_local(async move {
        wait_pixel_worker_quiet().await;
        let Some(blob) = encode_proxy_jpeg(&px, w, h).await else { return };
        let still_lazy = CACHE.with(|c| {
            c.borrow()
                .photos
                .get(&id)
                .map(|p| p.raw && !p.full)
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
pub(crate) fn develop_proxy_pool(state: AppState, id: usize, name: String) {
    let eligible = CACHE.with(|c| {
        c.borrow()
            .photos
            .get(&id)
            .map(|p| p.raw && !p.full)
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
                    neutral: proxy_neutral_data(&p),
                    pixels: Pixels::view(p.pixels.clone()),
                    w: p.w,
                    h: p.h,
                    frame,
                    developed: true,
                    in_worker: false,
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

pub(crate) fn pool_batch_begin() {
    if POOL_SCOPES.with(|s| s.get()) == 0 {
        POOL_BATCH_START.with(|s| s.set(perf::now()));
        POOL_BATCH_FILES.with(|f| f.set(0));
    }
    POOL_SCOPES.with(|s| s.set(s.get() + 1));
}

pub(crate) fn pool_batch_add() {
    POOL_BATCH_FILES.with(|f| f.set(f.get() + 1));
    POOL_JOBS.with(|j| j.set(j.get() + 1));
}

pub(crate) fn pool_batch_done() {
    let left = POOL_JOBS.with(|j| j.get().saturating_sub(1));
    POOL_JOBS.with(|j| j.set(left));
    pool_batch_finish_if_idle();
}

pub(crate) fn pool_batch_end() {
    let n = POOL_SCOPES.with(|s| s.get()).saturating_sub(1);
    POOL_SCOPES.with(|s| s.set(n));
    pool_batch_finish_if_idle();
}

/// Once the import has enqueued every RAW and the pool has drained, record the
/// batch mark and terminate the pool workers — their wasm memory never shrinks
/// and a batch develop measures ~450 MB per worker.
pub(crate) fn pool_batch_finish_if_idle() {
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
pub(crate) fn ensure_proxy(state: AppState, id: usize, develop: bool) {
    let Some(photo) = get_photo(id) else { return };
    if !photo.raw {
        ensure_full(state, id, None);
        return;
    }
    if photo.full {
        return;
    }
    touch_proxy(id);
    if proxy_in_mem(id) {
        if develop && (!proxy_developed(id) || proxy_wants_neutral(state, id)) {
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

/// How many preview decodes may run at once. Decoding is off the main thread
/// (createImageBitmap), so this bounds memory, not jank.
pub(crate) const PREVIEW_CONCURRENCY: usize = 3;

thread_local! {
    /// Items with a preview decode queued or in flight (dedupes requests).
    pub(crate) static PREVIEW_INFLIGHT: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    pub(crate) static PREVIEW_QUEUE: RefCell<VecDeque<usize>> = RefCell::new(VecDeque::new());
    pub(crate) static PREVIEW_ACTIVE: Cell<usize> = const { Cell::new(0) };
    pub(crate) static PREVIEW_STATE: Cell<Option<AppState>> = const { Cell::new(None) };
    /// Filmstrip visibility observer; tiles ask for their preview when they
    /// scroll into view.
    pub(crate) static THUMB_OBSERVER: RefCell<Option<web_sys::IntersectionObserver>> =
        RefCell::new(None);
    /// Tiles that registered before the strip root mounted.
    pub(crate) static THUMB_PENDING: RefCell<Vec<web_sys::Element>> = RefCell::new(Vec::new());
    pub(crate) static THUMB_STATE: Cell<Option<AppState>> = const { Cell::new(None) };
}

pub(crate) fn preview_ready(id: usize) -> bool {
    get_photo(id).map(|p| p.preview.1 > 0 && p.preview.2 > 0).unwrap_or(false)
}

/// Ask for an item's 1024 px working preview. Selection is urgent (front of
/// the queue); the filmstrip observer and the post-restore idle pass are not.
pub(crate) fn ensure_preview(state: AppState, id: usize) {
    request_preview(state, id, true);
}

pub(crate) fn request_preview(state: AppState, id: usize, urgent: bool) {
    if preview_ready(id) {
        return;
    }
    if !PREVIEW_INFLIGHT.with(|f| f.borrow_mut().insert(id)) {
        if urgent {
            PREVIEW_QUEUE.with(|q| {
                let mut q = q.borrow_mut();
                if let Some(pos) = q.iter().position(|&x| x == id) {
                    let id = q.remove(pos).unwrap();
                    q.push_front(id);
                }
            });
        }
        return;
    }
    PREVIEW_STATE.with(|s| s.set(Some(state)));
    PREVIEW_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if urgent {
            q.push_front(id);
        } else {
            q.push_back(id);
        }
    });
    pump_previews();
}

pub(crate) fn pump_previews() {
    loop {
        if PREVIEW_ACTIVE.with(|a| a.get()) >= PREVIEW_CONCURRENCY {
            return;
        }
        let Some(id) = PREVIEW_QUEUE.with(|q| q.borrow_mut().pop_front()) else {
            return;
        };
        if preview_ready(id) {
            PREVIEW_INFLIGHT.with(|f| {
                f.borrow_mut().remove(&id);
            });
            continue;
        }
        PREVIEW_ACTIVE.with(|a| a.set(a.get() + 1));
        let state = PREVIEW_STATE.with(|s| s.get());
        spawn_local(async move {
            let preview = decode_preview_source(id).await;
            PREVIEW_ACTIVE.with(|a| a.set(a.get().saturating_sub(1)));
            PREVIEW_INFLIGHT.with(|f| {
                f.borrow_mut().remove(&id);
            });
            if let (Some(preview), Some(state)) = (preview, state) {
                let tracked = CACHE.with(|c| c.borrow().photos.contains_key(&id));
                if tracked {
                    CACHE.with(|c| {
                        if let Some(pd) = c.borrow_mut().photos.get_mut(&id) {
                            let pd = Rc::make_mut(pd);
                            pd.preview = preview;
                            pd.gen += 1;
                        }
                    });
                    state.items.update(|_| {});
                }
            }
            pump_previews();
        });
    }
}

pub(crate) async fn decode_preview_source(id: usize) -> Option<(Vec<u8>, usize, usize)> {
    let stored = match session::open_db().await {
        Ok(db) => session::get_preview(&db, id).await.ok().flatten(),
        Err(_) => None,
    }
    .and_then(|v| v.dyn_into::<Blob>().ok())
    .filter(|b| b.size() > 0.0);
    let raw = CACHE.with(|c| c.borrow().photos.get(&id).map(|p| p.raw).unwrap_or(false));
    let blob = match stored {
        Some(b) => b,
        None if raw => return None,
        None => CACHE.with(|c| c.borrow().photo_src.get(&id).cloned())?,
    };
    match web::downscale_blob(&blob, 1024).await {
        Ok(p) => Some(p),
        Err(_) => {
            // Browser without ImageBitmap/OffscreenCanvas: decode through an
            // <img>, which still decodes off the main thread.
            let (img, url) = web::load_image(&blob).await.ok()?;
            let _ = Url::revoke_object_url(&url);
            Some(downscale(&img, 1024))
        }
    }
}

/// Start observing filmstrip tiles against the strip as scroll root.
pub(crate) fn ensure_thumb_observer(state: AppState, root: &web_sys::Element) {
    if THUMB_OBSERVER.with(|o| o.borrow().is_some()) {
        return;
    }
    THUMB_STATE.with(|s| s.set(Some(state)));
    let cb =
        Closure::<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>::new(
            move |entries: js_sys::Array, _| {
                let Some(state) = THUMB_STATE.with(|s| s.get()) else {
                    return;
                };
                for e in entries.iter() {
                    let Ok(entry) =
                        e.dyn_into::<web_sys::IntersectionObserverEntry>()
                    else {
                        continue;
                    };
                    if !entry.is_intersecting() {
                        continue;
                    }
                    let Some(id) = entry
                        .target()
                        .get_attribute("data-id")
                        .and_then(|s| s.parse::<usize>().ok())
                    else {
                        continue;
                    };
                    request_preview(state, id, false);
                }
            },
        );
    let init = web_sys::IntersectionObserverInit::new();
    init.set_root(Some(root));
    init.set_root_margin("96px");
    if let Ok(obs) = web_sys::IntersectionObserver::new_with_options(
        cb.as_ref().unchecked_ref(),
        &init,
    ) {
        THUMB_OBSERVER.with(|o| *o.borrow_mut() = Some(obs.clone()));
        THUMB_PENDING.with(|p| {
            for el in p.borrow_mut().drain(..) {
                obs.observe(&el);
            }
        });
    }
    cb.forget();
}

pub(crate) fn observe_thumb(el: &web_sys::Element) {
    if let Some(obs) = THUMB_OBSERVER.with(|o| o.borrow().clone()) {
        obs.observe(el);
    } else {
        THUMB_PENDING.with(|p| p.borrow_mut().push(el.clone()));
    }
}

/// Warm the remaining previews once the strip is interactive, so switching
/// items after a restore doesn't wait on a decode.
pub(crate) fn prefetch_previews(state: AppState) {
    let ids: Vec<usize> = state
        .items
        .get_untracked()
        .iter()
        .filter(|m| m.kind == MediaKind::Photo)
        .map(|m| m.id)
        .collect();
    if ids.is_empty() {
        return;
    }
    let cb = Closure::once(move || {
        for id in ids {
            request_preview(state, id, false);
        }
    });
    let win = web::window();
    if win
        .request_idle_callback(cb.as_ref().unchecked_ref())
        .is_err()
    {
        let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            250,
        );
    }
    cb.forget();
}
