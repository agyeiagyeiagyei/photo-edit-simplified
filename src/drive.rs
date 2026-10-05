//! Google Drive integration: sign-in (GIS token client), folder grant via the
//! Drive Picker, and Drive API v3 file list/download/upload over fetch.
//! The OAuth/Picker dance lives in JS shims (index.html window.pesDrive*);
//! this module owns config, state and the REST calls.

use js_sys::{Array, Promise, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

// --- Config -------------------------------------------------------------------

pub const CLIENT_ID: &str =
    "520957027706-qmgdrkuo9gps38e4uqaevlrnp945nmil.apps.googleusercontent.com";
pub const API_KEY: &str = "AIzaSyBK8OUW6AML5pcSQ6UQSRqj0UM0-IgjQL8";
/// Google Cloud project number (Picker setAppId).
pub const APP_ID: &str = "520957027706";
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly \
                         https://www.googleapis.com/auth/drive.file";

pub fn configured() -> bool {
    !CLIENT_ID.starts_with("REPLACE_ME")
}

const FOLDER_KEY: &str = "pes-drive-folder";

// --- Types -------------------------------------------------------------------

#[derive(Clone, PartialEq)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size_kb: u64,
    /// Subfolder path relative to the granted folder, e.g. "trip/iceland/"
    /// ("" for files at the root of the granted folder).
    pub path: String,
    /// Id of the containing folder — used to place save-as-copy fallbacks.
    pub parent: String,
    /// True when the file was written by this app (appProperties marker set
    /// on upload) — shown as an "Edited" badge in the folder view.
    pub edited: bool,
}

/// Thumbnail URL usable directly in an <img> tag. Drive's thumbnailLink is
/// CORS-blocked for browser apps; this endpoint renders via the user's
/// Google session instead.
pub fn thumb_url(file_id: &str) -> String {
    format!("https://drive.google.com/thumbnail?id={file_id}&sz=w400")
}

/// Result of a recursive folder listing.
pub struct Listing {
    pub files: Vec<DriveFile>,
    /// Subfolders that could not be opened (e.g. the Drive grant doesn't
    /// cover them) — surfaced so the user knows the listing is partial.
    pub skipped: Vec<String>,
}

impl DriveFile {
    pub fn importable(&self) -> bool {
        self.mime.starts_with("image/") || crate::raw::is_raw_name(&self.name)
    }
}

// --- localStorage persistence -------------------------------------------------

pub fn saved_folder() -> Option<(String, String)> {
    let storage = crate::web::window().local_storage().ok()??;
    let v = storage.get_item(FOLDER_KEY).ok()??;
    let (id, name) = v.split_once('\t')?;
    Some((id.to_string(), name.to_string()))
}

pub fn save_folder(id: &str, name: &str) {
    if let Ok(Some(s)) = crate::web::window().local_storage() {
        let _ = s.set_item(FOLDER_KEY, &format!("{id}\t{name}"));
    }
}

pub fn clear_folder() {
    if let Ok(Some(s)) = crate::web::window().local_storage() {
        let _ = s.remove_item(FOLDER_KEY);
    }
}

// --- JS shims ------------------------------------------------------------------

async fn call_shim(name: &str, args: &Array) -> Result<JsValue, JsValue> {
    let f: js_sys::Function =
        Reflect::get(&crate::web::window(), &JsValue::from_str(name))?.unchecked_into();
    let out = f.apply(&JsValue::NULL, args)?;
    JsFuture::from(Promise::from(out)).await
}

/// Popup the GIS token flow; resolves to an access token string.
pub async fn sign_in() -> Result<String, JsValue> {
    let args = Array::new();
    args.push(&JsValue::from_str(CLIENT_ID));
    args.push(&JsValue::from_str(SCOPE));
    let v = call_shim("pesDriveSignIn", &args).await?;
    v.as_string()
        .ok_or_else(|| JsValue::from_str("sign-in returned no token"))
}

/// Drive Picker folder grant; resolves to Some((id, name)) or None on cancel.
pub async fn pick_folder(token: &str) -> Result<Option<(String, String)>, JsValue> {
    let args = Array::new();
    args.push(&JsValue::from_str(API_KEY));
    args.push(&JsValue::from_str(APP_ID));
    args.push(&JsValue::from_str(token));
    let v = call_shim("pesDrivePickFolder", &args).await?;
    if v.is_null() || v.is_undefined() {
        return Ok(None);
    }
    let id = Reflect::get(&v, &JsValue::from_str("id"))?
        .as_string()
        .unwrap_or_default();
    let name = Reflect::get(&v, &JsValue::from_str("name"))?
        .as_string()
        .unwrap_or_else(|| "Drive folder".into());
    Ok(Some((id, name)))
}

// --- Drive API v3 over fetch ---------------------------------------------------

async fn drive_fetch(
    url: &str,
    token: &str,
    method: &str,
    body: Option<&web_sys::Blob>,
    content_type: Option<&str>,
) -> Result<web_sys::Response, JsValue> {
    let headers = web_sys::Headers::new()?;
    headers.set("Authorization", &format!("Bearer {token}"))?;
    if let Some(ct) = content_type {
        headers.set("Content-Type", ct)?;
    }
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    init.set_headers(&headers);
    if let Some(b) = body {
        init.set_body(b.unchecked_ref());
    }
    let req = web_sys::Request::new_with_str_and_init(url, &init)?;
    let v = JsFuture::from(crate::web::window().fetch_with_request(&req)).await?;
    let resp: web_sys::Response = v.unchecked_into();
    if !resp.ok() {
        return Err(JsValue::from_str(&format!(
            "Drive {method} {url} -> HTTP {}",
            resp.status()
        )));
    }
    Ok(resp)
}

fn js_str(v: &JsValue, key: &str) -> Option<String> {
    Reflect::get(v, &JsValue::from_str(key))
        .ok()
        .and_then(|s| s.as_string())
}

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// Runaway guards for the recursive walk.
const MAX_DEPTH: u32 = 10;
const MAX_FILES: usize = 2000;

/// List the granted folder recursively (subfolders included), depth-first.
/// Files arrive grouped by folder, each level newest-first. Subfolders that
/// fail to open are collected in `Listing::skipped` instead of aborting.
pub async fn list_files(token: &str, folder_id: &str) -> Result<Listing, JsValue> {
    let mut out = Listing {
        files: Vec::new(),
        skipped: Vec::new(),
    };
    walk_folder(token, folder_id, String::new(), 0, &mut out).await?;
    Ok(out)
}

fn walk_folder<'a>(
    token: &'a str,
    folder_id: &'a str,
    path: String,
    depth: u32,
    out: &'a mut Listing,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JsValue>> + 'a>> {
    Box::pin(async move {
        let q = format!(
            "'{}' in parents and trashed = false",
            folder_id.replace('\'', "\\'")
        );
        let mut page_token: Option<String> = None;
        let mut subfolders: Vec<(String, String)> = Vec::new();
        loop {
            let mut url = format!(
                "https://www.googleapis.com/drive/v3/files?q={}\
                 &fields=nextPageToken,files(id,name,mimeType,size,appProperties)\
                 &orderBy=modifiedTime%20desc&pageSize=200\
                 &supportsAllDrives=true&includeItemsFromAllDrives=true",
                js_sys::encode_uri_component(&q)
            );
            if let Some(pt) = &page_token {
                url.push_str("&pageToken=");
                url.push_str(&String::from(js_sys::encode_uri_component(pt)));
            }
            let resp = drive_fetch(&url, token, "GET", None, None).await?;
            let json = JsFuture::from(resp.json()?).await?;
            let files_v = Reflect::get(&json, &JsValue::from_str("files"))?;
            let arr: Array = files_v.unchecked_into();
            for f in arr.iter() {
                let Some(id) = js_str(&f, "id") else { continue };
                let Some(name) = js_str(&f, "name") else { continue };
                let mime = js_str(&f, "mimeType").unwrap_or_default();
                if mime == FOLDER_MIME {
                    subfolders.push((id, name));
                    continue;
                }
                if out.files.len() >= MAX_FILES {
                    continue;
                }
                let size_kb = js_str(&f, "size")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0)
                    / 1024;
                let edited = Reflect::get(&f, &JsValue::from_str("appProperties"))
                    .ok()
                    .and_then(|p| js_str(&p, "pes_edited"))
                    .as_deref()
                    == Some("1");
                out.files.push(DriveFile {
                    id,
                    name,
                    mime,
                    size_kb,
                    path: path.clone(),
                    parent: folder_id.to_string(),
                    edited,
                });
            }
            page_token = js_str(&json, "nextPageToken");
            if page_token.is_none() {
                break;
            }
        }
        if depth >= MAX_DEPTH {
            out.skipped
                .extend(subfolders.into_iter().map(|(_, n)| format!("{path}{n}/")));
            return Ok(());
        }
        for (id, name) in subfolders {
            let subpath = format!("{path}{name}/");
            if walk_folder(token, &id, subpath.clone(), depth + 1, out)
                .await
                .is_err()
            {
                out.skipped.push(subpath);
            }
        }
        Ok(())
    })
}

/// Download a file's bytes.
pub async fn download_file(token: &str, file_id: &str) -> Result<Vec<u8>, JsValue> {
    let url = format!(
        "https://www.googleapis.com/drive/v3/files/{file_id}?alt=media&supportsAllDrives=true"
    );
    let resp = drive_fetch(&url, token, "GET", None, None).await?;
    let buf = JsFuture::from(resp.array_buffer()?).await?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Upload bytes to Drive. With `existing_id` the file's content is overwritten
/// in place (PATCH); otherwise a new file is created inside `folder_id` (POST).
/// Returns the file id.
pub async fn upload_file(
    token: &str,
    folder_id: &str,
    existing_id: Option<&str>,
    name: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<String, JsValue> {
    let boundary = "pes_drive_boundary";
    let meta = match existing_id {
        Some(_) => format!(
            "{{\"name\":\"{}\",\"appProperties\":{{\"pes_edited\":\"1\"}}}}",
            json_escape(name)
        ),
        None => format!(
            "{{\"name\":\"{}\",\"parents\":[\"{}\"],\"appProperties\":{{\"pes_edited\":\"1\"}}}}",
            json_escape(name),
            json_escape(folder_id)
        ),
    };
    let mut body: Vec<u8> = Vec::with_capacity(bytes.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n\
             --{boundary}\r\nContent-Type: {mime}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let blob = crate::web::bytes_to_blob(&body, "multipart/related")?;

    let url = match existing_id {
        Some(id) => format!(
            "https://www.googleapis.com/upload/drive/v3/files/{id}\
             ?uploadType=multipart&supportsAllDrives=true"
        ),
        None => "https://www.googleapis.com/upload/drive/v3/files\
                 ?uploadType=multipart&supportsAllDrives=true"
            .into(),
    };
    let method = if existing_id.is_some() { "PATCH" } else { "POST" };
    let ct = format!("multipart/related; boundary={boundary}");
    let resp = drive_fetch(&url, token, method, Some(&blob), Some(&ct)).await?;
    let json = JsFuture::from(resp.json()?).await?;
    js_str(&json, "id").ok_or_else(|| JsValue::from_str("upload returned no file id"))
}
