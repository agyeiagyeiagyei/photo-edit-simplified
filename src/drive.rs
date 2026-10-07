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
    /// RFC3339 modified time — sorts lexicographically, shown as a date.
    pub modified: String,
    /// Id of the containing folder — used to place save-as-copy fallbacks.
    pub parent: String,
    /// True when the file was written by this app (appProperties marker set
    /// on upload) — shown as an "Edited" badge in the folder view.
    pub edited: bool,
}

#[derive(Clone, PartialEq)]
pub struct SubFolder {
    pub id: String,
    pub name: String,
}

/// Thumbnail URL usable directly in an <img> tag at a given pixel width.
/// Drive's thumbnailLink is CORS-blocked for browser apps; this endpoint
/// renders via the user's Google session instead.
pub fn thumb_url_sz(file_id: &str, width: u32) -> String {
    format!("https://drive.google.com/thumbnail?id={file_id}&sz=w{width}")
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
/// Runaway guard for one folder's listing.
const MAX_FILES: usize = 2000;

/// Name of the app-owned manifest stored at the root of the granted folder:
/// starred file ids plus parametric edit recipes. Filtered out of the
/// browsing grid.
pub const MANIFEST_NAME: &str = "photo-edit-shortlist.json";

/// List a single folder (not recursive): importable files plus subfolders.
/// `order` is a Drive orderBy clause, e.g. "modifiedTime desc" or "name".
pub async fn list_folder(
    token: &str,
    folder_id: &str,
    order: &str,
) -> Result<(Vec<DriveFile>, Vec<SubFolder>), JsValue> {
    let q = format!(
        "'{}' in parents and trashed = false",
        folder_id.replace('\'', "\\'")
    );
    let mut files = Vec::new();
    let mut subfolders = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let mut url = format!(
            "https://www.googleapis.com/drive/v3/files?q={}\
             &fields=nextPageToken,files(id,name,mimeType,size,modifiedTime,appProperties)\
             &orderBy={}&pageSize=200\
             &supportsAllDrives=true&includeItemsFromAllDrives=true",
            js_sys::encode_uri_component(&q),
            js_sys::encode_uri_component(order),
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
                subfolders.push(SubFolder { id, name });
                continue;
            }
            if name == MANIFEST_NAME || files.len() >= MAX_FILES {
                continue;
            }
            let size_kb = js_str(&f, "size")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0)
                / 1024;
            let modified = js_str(&f, "modifiedTime").unwrap_or_default();
            let edited = Reflect::get(&f, &JsValue::from_str("appProperties"))
                .ok()
                .and_then(|p| js_str(&p, "pes_edited"))
                .as_deref()
                == Some("1");
            files.push(DriveFile {
                id,
                name,
                mime,
                size_kb,
                modified,
                parent: folder_id.to_string(),
                edited,
            });
        }
        page_token = js_str(&json, "nextPageToken");
        if page_token.is_none() {
            break;
        }
    }
    Ok((files, subfolders))
}

/// Find the shortlist manifest at the root of the granted folder.
pub async fn find_manifest(token: &str, folder_id: &str) -> Result<Option<String>, JsValue> {
    let q = format!(
        "name = '{}' and '{}' in parents and trashed = false",
        MANIFEST_NAME,
        folder_id.replace('\'', "\\'")
    );
    let url = format!(
        "https://www.googleapis.com/drive/v3/files?q={}&fields=files(id)\
         &supportsAllDrives=true&includeItemsFromAllDrives=true",
        js_sys::encode_uri_component(&q)
    );
    let resp = drive_fetch(&url, token, "GET", None, None).await?;
    let json = JsFuture::from(resp.json()?).await?;
    let files_v = Reflect::get(&json, &JsValue::from_str("files"))?;
    let arr: Array = files_v.unchecked_into();
    Ok(arr.iter().next().and_then(|f| js_str(&f, "id")))
}

/// Parse the manifest into (starred ids, edits map of file id → raw recipe
/// JSON string). Version 1 manifests (no edits key) parse as empty edits.
pub fn parse_manifest(
    bytes: &[u8],
) -> (std::collections::HashSet<String>, std::collections::HashMap<String, String>) {
    let mut starred = std::collections::HashSet::new();
    let mut edits = std::collections::HashMap::new();
    let Ok(text) = std::str::from_utf8(bytes) else { return (starred, edits) };
    let Ok(v) = js_sys::JSON::parse(text) else { return (starred, edits) };
    if let Ok(arr_v) = Reflect::get(&v, &JsValue::from_str("starred")) {
        if Array::is_array(&arr_v) {
            let arr: Array = arr_v.unchecked_into();
            for id in arr.iter() {
                if let Some(s) = id.as_string() {
                    starred.insert(s);
                }
            }
        }
    }
    if let Ok(edits_v) = Reflect::get(&v, &JsValue::from_str("edits")) {
        if edits_v.is_object() {
            let obj: js_sys::Object = edits_v.clone().unchecked_into();
            let keys = js_sys::Object::keys(&obj);
            for k in keys.iter() {
                let Some(key) = k.as_string() else { continue };
                let Ok(recipe) = Reflect::get(&edits_v, &k) else { continue };
                // Round-trip through stringify to keep the recipe as a raw JSON
                // string — the map stores recipes unparsed until applied.
                if let Ok(s) = js_sys::JSON::stringify(&recipe) {
                    if let Some(s) = s.as_string() {
                        edits.insert(key, s);
                    }
                }
            }
        }
    }
    (starred, edits)
}

/// Serialize the manifest (version 2). Recipes are raw JSON strings produced
/// by state::edit_recipe_json and spliced in verbatim; keys sorted for stable
/// diffs in Drive's revision history.
pub fn manifest_json(
    starred: &std::collections::HashSet<String>,
    edits: &std::collections::HashMap<String, String>,
) -> Vec<u8> {
    let mut ids: Vec<String> = starred.iter().map(|i| format!("\"{}\"", json_escape(i))).collect();
    ids.sort();
    let mut entries: Vec<String> = edits
        .iter()
        .map(|(k, r)| format!("\"{}\":{}", json_escape(k), r))
        .collect();
    entries.sort();
    format!(
        "{{\"version\":2,\"starred\":[{}],\"edits\":{{{}}}}}",
        ids.join(","),
        entries.join(",")
    )
    .into_bytes()
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

/// Fetch one file's metadata by id — resolves starred files that live outside
/// the folder currently being browsed.
pub async fn get_file(token: &str, file_id: &str) -> Result<DriveFile, JsValue> {
    let url = format!(
        "https://www.googleapis.com/drive/v3/files/{}\
         ?fields=id,name,mimeType,size,modifiedTime,appProperties,parents\
         &supportsAllDrives=true",
        js_sys::encode_uri_component(file_id)
    );
    let resp = drive_fetch(&url, token, "GET", None, None).await?;
    let json = JsFuture::from(resp.json()?).await?;
    let id = js_str(&json, "id").ok_or_else(|| JsValue::from_str("bad file metadata"))?;
    let name = js_str(&json, "name").unwrap_or_default();
    let mime = js_str(&json, "mimeType").unwrap_or_default();
    let size_kb = js_str(&json, "size")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
        / 1024;
    let modified = js_str(&json, "modifiedTime").unwrap_or_default();
    let parent = Reflect::get(&json, &JsValue::from_str("parents"))
        .ok()
        .map(|p| Array::from(&p))
        .and_then(|a| a.get(0).as_string())
        .unwrap_or_default();
    let edited = Reflect::get(&json, &JsValue::from_str("appProperties"))
        .ok()
        .and_then(|p| js_str(&p, "pes_edited"))
        .as_deref()
        == Some("1");
    Ok(DriveFile { id, name, mime, size_kb, modified, parent, edited })
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
