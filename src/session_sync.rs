//! Session persistence: item removal, change signatures, debounced save and restore from IndexedDB.

use crate::prelude::*;
use crate::*;

/// 1024px JPEG preview for the `previews` store: the lazy preview pipeline
/// decodes this instead of re-decoding a full-size source on restore.
pub(crate) fn store_preview(id: usize, pixels: Vec<u8>, w: usize, h: usize) {
    spawn_local(async move {
        let canvas = web::create_canvas(w as u32, h as u32);
        web::put_pixels(&canvas, &pixels, w as u32, h as u32);
        let Ok(b) = web::canvas_to_jpeg_blob(&canvas, 0.9).await else {
            return;
        };
        if let Ok(db) = session::open_db().await {
            let _ = session::put_preview(&db, id, b.as_ref()).await;
        }
    });
}

/// Drop an item from the filmstrip: revoke its object URLs and free its
/// cached pixels/video blob.
pub(crate) fn remove_item(state: AppState, item_id: usize) {
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
            let _ = session::delete_thumb(&db, item_id).await;
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

#[derive(Clone, PartialEq)]
pub(crate) enum BytesSig {
    /// Plain photo: (object_url, cache gen). gen > 0 means heal/clone baked
    /// new pixels — the stored bytes are a full-res PNG of the cache.
    Photo(String, u64),
    /// RAW import: (raw byte len, cache gen).
    Raw(u64, u64),
    /// Video: blob byte len.
    Video(u64),
}

#[derive(Clone, PartialEq)]
pub(crate) struct ItemSig {
    pub(crate) bytes: BytesSig,
    pub(crate) name: String,
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) ord: usize,
    pub(crate) recipe: Option<String>,
    pub(crate) layers: Option<String>,
    /// Raster layer ids whose pixel PNGs are already stored.
    pub(crate) raster_layers: Vec<usize>,
    pub(crate) exif_len: usize,
    pub(crate) drive_file_id: Option<String>,
    pub(crate) drive_parent_id: Option<String>,
    pub(crate) drive_baked: bool,
    pub(crate) next_layer_id: usize,
}

thread_local! {
    pub(crate) static SESSION_SIGS: RefCell<HashMap<usize, ItemSig>> = RefCell::new(HashMap::new());
    pub(crate) static SESSION_GEN: Cell<u64> = const { Cell::new(0) };
    /// Persist stays inert until the restore pass finishes, so a fresh page
    /// holding an empty state can't wipe the stored session before reading it.
    pub(crate) static SESSION_READY: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn item_sig(item: &MediaItem, ord: usize) -> ItemSig {
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

pub(crate) fn schedule_session_save(state: AppState) {
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
pub(crate) async fn pixels_to_png_blob(pixels: &[u8], w: usize, h: usize) -> Result<Blob, JsValue> {
    let canvas = web::create_canvas(w as u32, h as u32);
    web::put_pixels(&canvas, pixels, w as u32, h as u32);
    web::canvas_to_blob(&canvas, "image/png").await
}

pub(crate) async fn persist_session(state: AppState) {
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
        let _ = session::delete_thumb(&db, *id).await;
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
                // Healed: the worker's full-res raster is the source of
                // truth. gen moved ⇒ the item is dirty ⇒ the buffer is
                // present (dirty items are eviction-exempt); encode in the
                // worker so the app thread never reads back 100 MB.
                BytesSig::Photo(_, _) | BytesSig::Raw(_, _) => {
                    let pid = item.id;
                    match await_worker(move |cb| pworker::encode_png(pid, cb)).await {
                        Some(view) => web::view_to_blob(&view, "image/png").ok(),
                        None => None,
                    }
                }
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
pub(crate) fn restore_session(state: AppState) {
    spawn_local(async move {
        let mut restore_span = perf::Span::new("restore.session");
        let db = match session::open_db().await {
            Ok(db) => db,
            Err(_) => {
                SESSION_READY.with(|r| r.set(true));
                return;
            }
        };
        let (mut records, thumb_records, meta) =
            session::load_all(&db).await.unwrap_or_default();
        records.sort_by_key(|(_, v)| state::jnum(v, "ord").unwrap_or(0.0) as usize);
        let thumbs: HashMap<usize, JsValue> = thumb_records.into_iter().collect();
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
            let mut next_layer_id = state::jnum(&v, "nlid").unwrap_or(0.0) as usize;
            state::migrate_adjust_layers(&mut edit, &mut layers, &mut next_layer_id);
            let taken = state::jnum(&v, "taken");
            // A stored thumbnail lets this item restore without decoding
            // anything; sessions written before the `thumbs` store fall back
            // to the eager preview decode below and migrate on this pass.
            let stored_thumb = thumbs.get(&id).and_then(|v| {
                v.clone().dyn_into::<Blob>().ok().filter(|b| b.size() > 0.0)
            });

            // A RAW added by the preview-first importer restores from its
            // stored preview — no sensor decode, no 25MB read.
            let lazy_preview = if is_raw && !is_video && stored_thumb.is_none() {
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
            } else if is_raw && stored_thumb.is_some() {
                // Fast path: build the filmstrip tile from the stored thumb.
                // The 1024px preview and the display proxy decode lazily once
                // the item is selected.
                let thumb_url = Url::create_object_url_with_blob(stored_thumb.as_ref().unwrap())
                    .unwrap_or_default();
                let w = state::jnum(&v, "w").unwrap_or(0.0) as usize;
                let h = state::jnum(&v, "h").unwrap_or(0.0) as usize;
                let raw_len = state::jnum(&v, "rawlen").unwrap_or(0.0) as u64;
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: false,
                            neutral: false,
                            full_dims: (w, h),
                            preview: (Vec::new(), 0, 0),
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
                    object_url: String::new(),
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
            } else if let Some(pv) = lazy_preview {
                let Ok((img, url)) = web::load_image(&pv).await else { continue };
                let preview = downscale(&img, 1024);
                let thumb_url = make_thumb(id, &preview.0, preview.1, preview.2).await;
                let w = state::jnum(&v, "w").unwrap_or(preview.1 as f64) as usize;
                let h = state::jnum(&v, "h").unwrap_or(preview.2 as f64) as usize;
                let raw_len = state::jnum(&v, "rawlen").unwrap_or(0.0) as u64;
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: false,
                            neutral: false,
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
                let Some(mut full) = raw_develop_async(id, blob.as_ref()).await else {
                    continue;
                };
                let (fw, fh) = (full.w, full.h);
                let (pp, pw, ph) = full.pixels.downscale(fw, fh, 1024);
                let canvas = web::create_canvas(pw as u32, ph as u32);
                web::put_pixels(&canvas, &pp, pw as u32, ph as u32);
                let Ok(png) = web::canvas_to_blob(&canvas, "image/png").await else {
                    continue;
                };
                let raw_ok = session::put_raw_src(&db, id, blob.as_ref()).await.is_ok();
                let prev_ok = session::put_preview(&db, id, png.as_ref()).await.is_ok();
                let raw_persisted = raw_ok && prev_ok;
                let url = Url::create_object_url_with_blob(&png).unwrap_or_default();
                let thumb_url = make_thumb(id, &pp, pw, ph).await;
                if !raw_persisted {
                    if let Ok(bytes) = web::blob_to_bytes(&blob).await {
                        CACHE.with(|c| c.borrow_mut().raw_fallback.insert(id, Rc::new(bytes)));
                    }
                }
                let has_full = full.pixels.transfer_to_worker(id, fw, fh);
                let has_neutral = match full.neutral.take() {
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
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: has_full,
                            neutral: has_neutral,
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
            } else if let Some(thumb) = stored_thumb {
                // Fast path: keep the source blob for on-demand full decode
                // and show the stored thumb. The 1024px preview lands when the
                // item is selected.
                let Some(bytes_v) = state::jget(&v, "bytes") else { continue };
                let Ok(blob) = bytes_v.dyn_into::<Blob>() else { continue };
                let thumb_url = Url::create_object_url_with_blob(&thumb).unwrap_or_default();
                let url = Url::create_object_url_with_blob(&blob).unwrap_or_default();
                let fw = state::jnum(&v, "w").unwrap_or(0.0) as usize;
                let fh = state::jnum(&v, "h").unwrap_or(0.0) as usize;
                CACHE.with(|c| {
                    let mut c = c.borrow_mut();
                    c.photo_src.insert(id, blob);
                    c.photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: false,
                            neutral: false,
                            full_dims: (fw, fh),
                            preview: (Vec::new(), 0, 0),
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
            } else {
                let Some(bytes_v) = state::jget(&v, "bytes") else { continue };
                let Ok(blob) = bytes_v.dyn_into::<Blob>() else { continue };
                let Ok((img, url)) = web::load_image(&blob).await else { continue };
                let (fw, fh) = (img.natural_width() as usize, img.natural_height() as usize);
                let preview = downscale(&img, 1024);
                let thumb_url = make_thumb(id, &preview.0, preview.1, preview.2).await;
                CACHE.with(|c| {
                    let mut c = c.borrow_mut();
                    c.photo_src.insert(id, blob);
                    c.photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full: false,
                            neutral: false,
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
        if let Some(meta) = meta {
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
        // Filmstrip is interactive: warm the rest of the previews in the
        // background so switching items doesn't wait on a decode.
        prefetch_previews(state);
    });
}
