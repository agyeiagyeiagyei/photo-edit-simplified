//! Google Drive browsing: auth refresh, folder listing, manifest star persistence, import and hover/loupe previews.

use crate::prelude::*;
use crate::*;

/// Drive grid hover popup: w800 thumbnail on dwell, upgrading to a panning
/// crop once the original has been downloaded and decoded. RAW uses the
/// embedded preview by default; after "Full res" the crop switches to sensor
/// pixels. Mirrors FocusLoupe: <Show> keeps the canvas mounted, paints happen
/// in the effect, and the crop is row-copied out of the cache buffer.
#[component]
pub(crate) fn DriveHoverLoupe(state: AppState) -> impl IntoView {
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

/// Folder currently being browsed: deepest breadcrumb, or the granted root.
pub(crate) fn drive_current_folder(state: AppState) -> Option<(String, String)> {
    if let Some(last) = state.drive_path.get_untracked().last() {
        return Some(last.clone());
    }
    state.drive_folder.get_untracked()
}

/// Current token, refreshed silently when it is within a minute of expiry.
/// GIS access tokens last ~1h; without this every Drive call in a long
/// editing session starts failing with HTTP 401.
pub(crate) async fn fresh_drive_token(state: AppState) -> Option<Rc<String>> {
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

pub(crate) fn drive_refresh(state: AppState) {
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
pub(crate) fn visible_drive_files(state: AppState) -> Vec<drive::DriveFile> {
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
pub(crate) fn schedule_manifest_save(state: AppState) {
    state.drive_save_gen.update(|g| *g += 1);
    state.drive_save_pending.set(true);
    let gen = state.drive_save_gen.get_untracked();
    manifest_save_attempt(state, gen, 0);
}

pub(crate) fn manifest_save_attempt(state: AppState, gen: u32, attempt: u32) {
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

//
// The whole working set is mirrored into IndexedDB so any reload — including
// the service worker's update reload — restores it: item bytes, parametric
// edits (same recipe JSON as the Drive manifest), layers, order, selection.
// Bytes get their own sub-signature so a slider tick never rewrites a
// multi-MB blob: a plain photo's source is keyed by object URL, a RAW source
// by length, a healed photo's pixels by cache generation (re-encoded to a
// full-res PNG only when the generation moves).

/// Imported Drive files join the star set: the star persists in the manifest,
/// so "Import starred" can treat it as "should be open" instead of a one-shot
/// queue, and unstarring stays the way a Drive-backed item is removed again.
pub(crate) fn star_drive_file(state: AppState, id: &str) {
    let mut added = false;
    state.drive_shortlist.update(|s| added = s.insert(id.to_string()));
    if added {
        schedule_manifest_save(state);
    }
}

pub(crate) fn toggle_star(state: AppState, id: String) {
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

pub(crate) fn drive_navigate(state: AppState, depth: usize) {
    // depth 0 = root; otherwise truncate breadcrumbs to that many levels.
    state.drive_path.update(|p| p.truncate(depth));
    state.drive_selected.set(Default::default());
    state.drive_loupe.set(None);
    drive_hover_clear(state);
    drive_refresh(state);
}

pub(crate) fn drive_enter(state: AppState, folder: drive::SubFolder) {
    state
        .drive_path
        .update(|p| p.push((folder.id, folder.name)));
    state.drive_selected.set(Default::default());
    state.drive_loupe.set(None);
    drive_hover_clear(state);
    drive_refresh(state);
}

thread_local! {
    pub(crate) static DRIVE_IMPORT_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Import Drive files one download at a time. A starred batch can be dozens
/// of 25MB RAWs; firing every fetch at once buffered the whole set
/// (1.4GB for 53 files) in the tab while the multi-second decodes ran
/// serially, and the renderer died partway down the list. Ingest tasks still
/// run one at a time — each decodes synchronously on the main thread.
pub(crate) fn import_drive_files(state: AppState, files: Vec<drive::DriveFile>, missing: Vec<String>) {
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

pub(crate) async fn import_drive_one(
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
    pub(crate) static DRIVE_HOVER_CACHE: RefCell<(Vec<String>, HashMap<String, Rc<(Pixels, usize, usize)>>)> =
        RefCell::new((Vec::new(), HashMap::new()));
    pub(crate) static DRIVE_HOVER_INFLIGHT: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    /// Ids in DRIVE_HOVER_CACHE whose pixels are an embedded RAW preview, not
    /// the full image — the loupe tag says so.
    pub(crate) static DRIVE_HOVER_PREVIEW: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    /// Sensor-res develop for the currently popped-up RAW, single-slot: a
    /// 24MP demosaic is ~96MB and only the open popup can use it.
    pub(crate) static DRIVE_FULL_CACHE: RefCell<Option<(String, Rc<(Pixels, usize, usize)>)>> =
        RefCell::new(None);
}

pub(crate) fn drive_full_cached(id: &str) -> bool {
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
pub(crate) fn drive_develop_full(state: AppState, id: String) {
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
            Some(f) => {
                if !alive() {
                    return;
                }
                DRIVE_FULL_CACHE.with(|c| {
                    *c.borrow_mut() = Some((id.clone(), Rc::new((f.pixels, f.w, f.h))));
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
pub(crate) fn drive_hover_prefetch(state: AppState, id: String, name: String) {
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
    let modified = visible_drive_files(state)
        .into_iter()
        .find(|f| f.id == id)
        .map(|f| f.modified)
        .unwrap_or_default();
    spawn_local(async move {
        let decoded = async {
            if arw {
                if let Some((px, w, h)) = drive_cached_proxy(&id, &modified).await {
                    return Some(((px, w, h), false, None));
                }
            }
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
                                    let rec = web::view_to_blob(&jpeg, "image/jpeg")
                                        .ok()
                                        .map(|blob| (blob, w, h, meta.orientation));
                                    return Some(((Pixels::owned(px), w, h), false, rec));
                                }
                            }
                        }
                    }
                }
                return raw_preview_async(&src)
                    .await
                    .map(|r| ((Pixels::view(r.pixels), r.w, r.h), true, None));
            }
            let blob = web::bytes_to_blob(&bytes, still_mime?).ok()?;
            let (img, _) = web::load_image(&blob).await.ok()?;
            let (px, w, h) = web::rgba_from_image(&img);
            Some(((Pixels::owned(px), w, h), false, None))
        }
        .await;
        DRIVE_HOVER_INFLIGHT.with(|s| {
            s.borrow_mut().remove(&id);
        });
        if let Some(((px, w, h), preview, cache_rec)) = decoded {
            drive_hover_install(state, id.clone(), px, w, h, preview);
            if let Some((blob, cw, ch, orientation)) = cache_rec {
                if !modified.is_empty() {
                    if let Ok(db) = session::open_db().await {
                        let rec = session::jobj();
                        session::jset(&rec, "modified", modified.as_str());
                        session::jset(&rec, "orientation", orientation as f64);
                        session::jset(&rec, "bytes", blob);
                        session::jset(&rec, "w", cw as f64);
                        session::jset(&rec, "h", ch as f64);
                        let _ = session::put_drive_proxy(&db, &id, &rec.into(), 24).await;
                    }
                }
            }
        }
    });
}

/// Install a decoded Drive hover frame in the two-slot cache and mark it ready.
pub(crate) fn drive_hover_install(state: AppState, id: String, px: Pixels, w: usize, h: usize, preview: bool) {
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

/// Decode a Drive RAW's persisted camera-JPEG proxy when its modifiedTime
/// still matches, so revisiting the file skips the original download.
pub(crate) async fn drive_cached_proxy(id: &str, modified: &str) -> Option<(Pixels, usize, usize)> {
    if modified.is_empty() {
        return None;
    }
    let db = session::open_db().await.ok()?;
    let rec = session::get_drive_proxy(&db, id, modified).await.ok()??;
    let blob = state::jget(&rec, "bytes")?.dyn_into::<Blob>().ok()?;
    let orientation = state::jget(&rec, "orientation")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0) as u16;
    let (px, w, h) = web::rgba_from_blob(&blob).await.ok()?;
    let (px, w, h) = raw::apply_orientation_code(px, w, h, orientation);
    Some((Pixels::owned(px), w, h))
}

/// Decode an embedded camera JPEG through the browser's image pipeline.
pub(crate) async fn camera_jpeg_rgba(jpeg: &Uint8Array) -> Option<(Vec<u8>, usize, usize)> {
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
pub(crate) fn drive_hover_soon(state: AppState, id: String, name: String, ev: &web_sys::MouseEvent, sel: &str) {
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

pub(crate) fn drive_hover_move(state: AppState, id: &str, ev: &web_sys::MouseEvent, sel: &str) {
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

pub(crate) fn drive_hover_clear(state: AppState) {
    state.drive_hover_gen.update(|g| *g += 1);
    state.drive_hover.set(None);
}

/// Fixed-position style for the hover preview, kept inside the viewport.
pub(crate) fn drive_hover_style(x: f64, y: f64) -> String {
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
pub(crate) fn drive_grid_thumb_w(state: AppState) -> u32 {
    let px = state.drive_thumb_px.get() as f64 * web::window().device_pixel_ratio();
    ((px / 100.0).ceil() as u32 * 100).clamp(100, 400)
}

#[component]
pub(crate) fn DriveLoupe(state: AppState) -> impl IntoView {
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
                let mut neighbors: Vec<String> = Vec::new();
                for d in [-1i64, 1] {
                    let nb = ((idx as i64 + d).rem_euclid(n as i64)) as usize;
                    if let Some(g) = files.get(nb) {
                        if !warm.contains(&g.id) {
                            warm.push(g.id.clone());
                        }
                        if g.id != f.id && !neighbors.contains(&g.id) {
                            neighbors.push(g.id.clone());
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
                                // The loupe paints w800 before the w1600 copy
                                // arrives, so warm the neighbors' w800 too —
                                // stepping to the next file then starts sharp.
                                {neighbors
                                    .into_iter()
                                    .map(|id| {
                                        view! { <img src=drive::thumb_url_sz(&id, 800) loading="eager" decoding="async"/> }
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
pub(crate) fn DrivePanel(state: AppState) -> impl IntoView {
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
