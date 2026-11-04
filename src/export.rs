//! Export paths: photo/video render, Google Drive save, ZIP download and ffmpeg argument construction.

use crate::prelude::*;
use crate::*;

/// Render a photo through the full edit pipeline into an encoded blob.
/// Returns (blob, mime, filename).
pub(crate) async fn render_photo_export(
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
    if !photo.full {
        return None;
    }
    let (fw, fh) = photo.full_dims;
    let (ww, wh) = working_dims(fw, fh, &item.edit);
    let (cx, cy, cw, ch) = crop_px(&item.edit, ww, wh);
    let (cx, cy, cw, ch) = aspect_fit(cx, cy, cw, ch, item.edit.aspect);
    // Photo exports keep the crop's native pixels; export_dims is the video
    // cap, so the target size is the crop itself.
    let (ew, eh) = (cw, ch);
    let format = state.photo_format.get_untracked();
    let recipe = pes_pixel::recipe::edit_params_full_json(&item.edit);
    let sel = item.edit.selection.clone();
    let id = item.id;
    let mut encode_span = perf::Span::new("export.encode");
    encode_span.set_detail(format!("{}x{}", ew, eh));

    let encoded = if item.layers.is_empty() {
        // The worker renders exactly the export rect and encodes there, so a
        // full-resolution RGBA frame never crosses to this thread.
        let png = format == PhotoFormat::Png;
        let quality = if format == PhotoFormat::JpegMax { 97 } else { 92 };
        await_worker(move |cb| {
            pworker::export_photo(
                id,
                &recipe,
                (cx, cy, cw, ch),
                ew,
                eh,
                png,
                quality,
                sel.as_ref(),
                cb,
            )
        })
        .await
        .and_then(|bytes| {
            web::view_to_blob(&bytes, if png { "image/png" } else { "image/jpeg" }).ok()
        })
    } else {
        // Layers composite on a canvas, so the edited base has to land here at
        // working resolution: one worker render, then crop+scale into the
        // export canvas.
        let frame = await_worker(move |cb| {
            let max_edge = ww.max(wh) as u32;
            pworker::render(
                id,
                &recipe,
                ww as f64,
                wh as f64,
                max_edge,
                sel.as_ref(),
                move |r| cb(r.ok()),
            )
        })
        .await?;
        let base = web::create_canvas(frame.w, frame.h);
        web::put_pixels_view(&base, &frame.pixels, frame.w, frame.h);
        composite_layers(&base, &item.layers, frame.w as usize, frame.h as usize);
        let out = web::create_canvas(ew as u32, eh as u32);
        web::ctx2d(&out)
            .draw_image_with_html_canvas_element_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
                &base, cx as f64, cy as f64, cw as f64, ch as f64,
                0.0, 0.0, ew as f64, eh as f64,
            )
            .unwrap();
        drop(base);
        match format {
            PhotoFormat::Jpeg | PhotoFormat::JpegMax => {
                let quality = if format == PhotoFormat::JpegMax { 97 } else { 92 };
                let fallback = if format == PhotoFormat::JpegMax { 1.0 } else { 0.92 };
                let jpeg = match web::image_data_view(&out, ew as u32, eh as u32) {
                    Some(view) => {
                        await_worker(move |cb| {
                            pworker::encode_jpeg(&view, ew as u32, eh as u32, quality, cb)
                        })
                        .await
                    }
                    None => None,
                };
                match jpeg {
                    Some(bytes) => web::bytes_to_blob(&bytes, "image/jpeg").ok(),
                    None => web::canvas_to_jpeg_blob(&out, fallback).await.ok(),
                }
            }
            PhotoFormat::Png => web::canvas_to_blob(&out, "image/png").await.ok(),
        }
    };
    let blob = encoded?;
    let (mime, ext) = match format {
        PhotoFormat::Png => ("image/png", "png"),
        _ => ("image/jpeg", "jpg"),
    };
    // Carry source EXIF into the export when we have it; PNGs always get an
    // sRGB tag because the canvas encoder writes none (the worker JPEG
    // already carries its ICC profile).
    let needs_bytes = format == PhotoFormat::Png || item.exif.is_some();
    let blob = if needs_bytes {
        match web::blob_to_bytes(&blob).await {
            Ok(mut bytes) => {
                if format == PhotoFormat::Png {
                    bytes = exif::tag_png_srgb(&bytes);
                }
                if let Some(tiff) = &item.exif {
                    bytes = match format {
                        PhotoFormat::Png => exif::inject_png(&bytes, tiff),
                        _ => exif::inject_jpeg(&bytes, tiff),
                    };
                }
                web::bytes_to_blob(&bytes, mime).unwrap_or(blob)
            }
            Err(_) => blob,
        }
    } else {
        blob
    };
    drop(encode_span);
    let base = item.name.rsplitn(2, '.').last().unwrap_or("photo");
    let filename = format!("edited-{}-{}.{}", item.edit.aspect.slug(), base, ext);
    Some((blob, mime, filename))
}

pub(crate) fn export_photo(state: AppState, item: MediaItem) {
    spawn_local(async move {
        state.busy.set(Some(format!("Exporting {}…", item.name)));
        if let Some((blob, _mime, filename)) = render_photo_export(state, &item).await {
            web::download_blob(&blob, &filename);
        }
        state.busy.set(None);
    });
}

pub(crate) enum DriveSave {
    Saved,
    AlreadyThere,
}

/// Resolve (find-or-create) the "edits" subfolder under the granted root —
/// every new file the app writes to Drive lands there. Cached in state.
pub(crate) async fn edits_folder_id(state: AppState, token: &str, root_id: &str) -> Result<String, String> {
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
pub(crate) async fn save_photo_to_drive_inner(state: AppState, item: MediaItem) -> Result<DriveSave, String> {
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
pub(crate) fn save_photo_to_drive(state: AppState, item: MediaItem) {
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
pub(crate) fn photo_needs_save(m: &MediaItem) -> bool {
    m.kind == MediaKind::Photo
        && (!photo_unedited(m) || get_photo(m.id).map(|p| p.dirty).unwrap_or(false))
}

/// Sequential batch save of every photo that needs it; one panel refresh at
/// the end. Sequential because each upload re-encodes full-res pixels.
pub(crate) fn save_edited_to_drive(state: AppState) {
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

pub(crate) fn js_err(e: &JsValue) -> String {
    e.as_string()
        .unwrap_or_else(|| format!("{e:?}").chars().take(200).collect())
}

/// True when nothing about the photo differs from the source file — saving
/// can pass the original bytes through instead of re-encoding (a JPEG would
/// otherwise lose a generation on every save).
pub(crate) fn photo_unedited(item: &MediaItem) -> bool {
    edit_unedited(&item.edit, item)
}

/// Unedited apart from RAW detail (Smart denoise, Demosaic sharpening). Those
/// stages are tone-neutral (#15), so an unedited frame is a faithful stand-in
/// on screen until the detailed render lands. The ISO-based denoise start
/// makes this the common case for a restored high-ISO RAW.
pub(crate) fn photo_unedited_but_detail(item: &MediaItem) -> bool {
    let mut e = item.edit.clone();
    e.raw.denoise = 0.0;
    e.raw.sharpen = 0.0;
    edit_unedited(&e, item)
}

fn edit_unedited(e: &EditParams, item: &MediaItem) -> bool {
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
pub(crate) fn passthru_mime(name: &str) -> Option<&'static str> {
    let n = name.to_lowercase();
    if n.ends_with(".png") {
        Some("image/png")
    } else if n.ends_with(".jpg") || n.ends_with(".jpeg") {
        Some("image/jpeg")
    } else {
        None
    }
}

pub(crate) fn export_video(state: AppState, item: MediaItem) {
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
pub(crate) async fn render_video_export(state: AppState, item: &MediaItem) -> Option<(Blob, String)> {
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
pub(crate) fn download_edited_zip(state: AppState) {
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

pub(crate) fn build_ffmpeg_args(item: &MediaItem, w: usize, h: usize, overlays: &[String]) -> Vec<String> {
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
