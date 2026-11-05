//! Import path: file/picker/Drive ingest, RAW preparation, video probing, downscaling and thumbnails.

use crate::prelude::*;
use crate::*;

pub(crate) fn get_video(id: usize) -> Option<(Blob, f64, u32, u32)> {
    CACHE.with(|c| {
        let c = c.borrow();
        match (c.video_blobs.get(&id), c.video_meta.get(&id)) {
            (Some(b), Some(m)) => Some((b.clone(), m.0, m.1, m.2)),
            _ => None,
        }
    })
}

pub(crate) fn is_heic(file: &File) -> bool {
    let t = file.type_().to_lowercase();
    let n = file.name().to_lowercase();
    t.contains("heic") || t.contains("heif") || n.ends_with(".heic") || n.ends_with(".heif")
}

pub(crate) struct RawIngest {
    pub(crate) full: Option<FullDevelop>,
    pub(crate) preview: (Vec<u8>, usize, usize),
    pub(crate) dims: (usize, usize),
    pub(crate) object_url: String,
    pub(crate) thumb_url: String,
    pub(crate) exif: Option<Rc<Vec<u8>>>,
    pub(crate) taken: Option<f64>,
    pub(crate) stored: bool,
    /// Only populated when `stored` is false: the file bytes kept on the app
    /// thread because the worker can no longer reach them through IndexedDB.
    pub(crate) fallback: Option<Vec<u8>>,
}

/// Materialise a RAW source on the app thread. Used only on the fallback path
/// (the original could not be persisted), so a copy here is acceptable.
pub(crate) async fn raw_bytes_from_src(src: &JsValue) -> Option<Vec<u8>> {
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
pub(crate) async fn prepare_raw(id: usize, src: &JsValue, mtime: Option<f64>) -> Option<RawIngest> {
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
        Some(f) => ((f.w, f.h), f.pixels.downscale(f.w, f.h, 1024)),
        // Camera frame dims keep the hi-res target sized to the sensor, not
        // the small embedded preview rawler exposes.
        None => (
            camera.as_ref().map(|(_, _, f)| *f).unwrap_or((pw2, ph2)),
            (pp, pw2, ph2),
        ),
    };
    let object_url = Url::create_object_url_with_blob(&png).ok()?;
    let thumb_url = make_thumb(id, &preview.0, preview.1, preview.2).await;
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

pub(crate) fn ingest_files(state: AppState, files: Vec<File>) {
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
                let (full, neutral) = install_full_develop(id, ing.full.take());
                CACHE.with(|c| {
                    c.borrow_mut().photos.insert(
                        id,
                        Rc::new(PhotoData {
                            full,
                            neutral,
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
                    edit: raw_import_edit(ing.exif.as_deref().map(|v| v.as_slice())),
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
                let thumb_url = make_thumb(id, &preview.0, preview.1, preview.2).await;
                drop(thumb_span);
                store_preview(id, preview.0.clone(), preview.1, preview.2);
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
pub(crate) fn push_item(state: AppState, item: MediaItem) {
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
pub(crate) async fn ingest_drive_bytes(
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
        let (mut edit, drive_baked) = if drive_edited {
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
        let stored_edit = edit != EditParams::default();
        let mut layers = Vec::new();
        let mut next_layer_id = 0usize;
        state::migrate_adjust_layers(&mut edit, &mut layers, &mut next_layer_id);

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
            let (full, neutral) = install_full_develop(id, ing.full.take());
            CACHE.with(|c| {
                c.borrow_mut().photos.insert(
                    id,
                    Rc::new(PhotoData {
                        full,
                        neutral,
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
            // Stored Drive edits win; only a RAW without any gets the ISO start.
            let edit = if !stored_edit && !drive_baked {
                raw_import_edit(ing.exif.as_deref().map(|v| v.as_slice()))
            } else {
                edit
            };
            push_item(state, MediaItem {
                id,
                kind: MediaKind::Photo,
                name,
                object_url: ing.object_url,
                thumb_url: ing.thumb_url,
                width: ing.dims.0,
                height: ing.dims.1,
                edit,
                layers,
                next_layer_id,
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
            let thumb_url = make_thumb(id, &preview.0, preview.1, preview.2).await;
            store_preview(id, preview.0.clone(), preview.1, preview.2);
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
                drive_file_id: Some(drive_file_id),
                drive_parent_id: Some(drive_parent_id),
                drive_baked,
            });
        }
    }
}

pub(crate) async fn probe_video(url: &str) -> Option<(f64, u32, u32)> {
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

/// `imageSmoothingQuality = "high"` — this web-sys version has no binding.
pub(crate) fn high_quality(ctx: &web_sys::CanvasRenderingContext2d) {
    let _ = js_sys::Reflect::set(
        ctx.as_ref(),
        &JsValue::from_str("imageSmoothingQuality"),
        &JsValue::from_str("high"),
    );
}

pub(crate) fn downscale(img: &web_sys::HtmlImageElement, long_edge: u32) -> (Vec<u8>, usize, usize) {
    let (w, h) = (img.natural_width(), img.natural_height());
    let scale = (long_edge as f32 / w.max(h) as f32).min(1.0);
    let (pw, ph) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
    let canvas = web::create_canvas(pw, ph);
    let ctx = web::ctx2d(&canvas);
    // The source pixels are not in memory here, so this decode still uses the
    // browser scaler; ask it for its best filter until the pixels are staged.
    high_quality(&ctx);
    ctx.draw_image_with_html_image_element_and_dw_and_dh(img, 0.0, 0.0, pw as f64, ph as f64)
        .unwrap();
    web::image_data_from_ctx(&ctx, pw, ph)
}

pub(crate) fn default_crop(w: usize, h: usize, aspect: Aspect) -> state::CropRect {
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

/// 256px JPEG thumbnail for a filmstrip tile: object URL for this session,
/// plus a write to the `thumbs` store so a reload needs no decode at all.
pub(crate) async fn make_thumb(id: usize, pixels: &[u8], w: usize, h: usize) -> String {
    let (tp, tw, th) = downscale_pixels(pixels, w, h, 256);
    let canvas = web::create_canvas(tw as u32, th as u32);
    web::put_pixels(&canvas, &tp, tw as u32, th as u32);
    match web::canvas_to_jpeg_blob(&canvas, 0.85).await {
        Ok(b) => {
            let url = Url::create_object_url_with_blob(&b).unwrap_or_default();
            spawn_local(async move {
                if let Ok(db) = session::open_db().await {
                    let _ = session::put_thumb(&db, id, b.as_ref()).await;
                }
            });
            url
        }
        Err(_) => String::new(),
    }
}

pub(crate) fn downscale_pixels(pixels: &[u8], w: usize, h: usize, long_edge: u32) -> (Vec<u8>, usize, usize) {
    resample::fit_long_edge(pixels, w, h, long_edge)
}


/// A new RAW import's starting edit: the camera rendering, plus a Smart
/// denoise start from the photo's ISO (#15). Only new imports get it, as a
/// visible slider value; restored sessions and stored Drive edits keep theirs.
fn raw_import_edit(exif: Option<&[u8]>) -> EditParams {
    let mut edit = EditParams::default();
    if let Some(iso) = exif.and_then(rawmeta::iso_from_tiff) {
        edit.raw.denoise = pes_pixel::rawops::denoise_start_for_iso(iso);
    }
    edit
}
