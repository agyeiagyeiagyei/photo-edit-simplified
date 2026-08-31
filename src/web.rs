//! Browser interop: canvas pixel access, blob helpers, JS library bridges.

use js_sys::{Array, Function, Promise, Reflect, Uint8ClampedArray};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, CanvasRenderingContext2d, HtmlCanvasElement, HtmlImageElement, ImageData, Url};

pub fn window() -> web_sys::Window {
    web_sys::window().expect("no window")
}

pub fn document() -> web_sys::Document {
    window().document().expect("no document")
}

pub fn create_canvas(w: u32, h: u32) -> HtmlCanvasElement {
    let c: HtmlCanvasElement = document()
        .create_element("canvas")
        .unwrap()
        .unchecked_into();
    c.set_width(w);
    c.set_height(h);
    c
}

pub fn ctx2d(canvas: &HtmlCanvasElement) -> CanvasRenderingContext2d {
    canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .unchecked_into()
}

pub fn image_data_from_ctx(
    ctx: &CanvasRenderingContext2d,
    w: u32,
    h: u32,
) -> (Vec<u8>, usize, usize) {
    let data = ctx.get_image_data(0.0, 0.0, w as f64, h as f64).unwrap();
    (data.data().to_vec(), w as usize, h as usize)
}

pub fn put_pixels(
    canvas: &HtmlCanvasElement,
    pixels: &[u8],
    w: u32,
    h: u32,
) {
    canvas.set_width(w);
    canvas.set_height(h);
    let arr = Uint8ClampedArray::from(pixels);
    let data = ImageData::new_with_js_u8_clamped_array_and_sh(&arr, w, h).unwrap();
    ctx2d(canvas).put_image_data(&data, 0.0, 0.0).unwrap();
}

/// Load a Blob into an HtmlImageElement (object URL lifetime is caller's job).
pub async fn load_image(blob: &Blob) -> Result<(HtmlImageElement, String), JsValue> {
    let url = Url::create_object_url_with_blob(blob)?;
    let img: HtmlImageElement = document()
        .create_element("img")
        .unwrap()
        .unchecked_into();
    let promise = Promise::new(&mut |resolve, reject| {
        img.set_onload(Some(&resolve));
        img.set_onerror(Some(&reject));
    });
    img.set_src(&url);
    JsFuture::from(promise).await?;
    Ok((img, url))
}

/// Draw an image element to a canvas at natural size and read back RGBA.
pub fn rgba_from_image(img: &HtmlImageElement) -> (Vec<u8>, usize, usize) {
    let w = img.natural_width();
    let h = img.natural_height();
    let canvas = create_canvas(w, h);
    let ctx = ctx2d(&canvas);
    ctx.draw_image_with_html_image_element(img, 0.0, 0.0).unwrap();
    image_data_from_ctx(&ctx, w, h)
}

fn call_global(name: &str, args: &Array) -> Result<JsValue, JsValue> {
    let f: Function = Reflect::get(&window(), &JsValue::from_str(name))?.unchecked_into();
    f.apply(&JsValue::NULL, args)
}

/// HEIC blob -> JPEG blob via window.pesHeicToJpeg (heic2any CDN script).
pub async fn heic_to_jpeg(blob: Blob) -> Result<Blob, JsValue> {
    let args = Array::new();
    args.push(&blob);
    let out = call_global("pesHeicToJpeg", &args)?;
    let result = JsFuture::from(Promise::from(out)).await?;
    Ok(result.unchecked_into())
}

/// Transcode a video via window.pesVideoTranscode (ffmpeg.wasm CDN script).
pub async fn video_transcode(
    name: &str,
    blob: Blob,
    args: Vec<String>,
    on_progress: Closure<dyn FnMut(f64)>,
) -> Result<Blob, JsValue> {
    let arr = Array::new();
    for a in &args {
        arr.push(&JsValue::from_str(a));
    }
    let call_args = Array::new();
    call_args.push(&JsValue::from_str(name));
    call_args.push(&blob);
    call_args.push(&arr);
    call_args.push(on_progress.as_ref());
    let out = call_global("pesVideoTranscode", &call_args)?;
    let result = JsFuture::from(Promise::from(out)).await?;
    Ok(result.unchecked_into())
}

/// Trigger a browser download for a Blob.
pub fn download_blob(blob: &Blob, filename: &str) {
    let url = Url::create_object_url_with_blob(blob).unwrap();
    let a: web_sys::HtmlElement = document()
        .create_element("a")
        .unwrap()
        .unchecked_into();
    a.set_attribute("href", &url).unwrap();
    a.set_attribute("download", filename).unwrap();
    a.click();
    let _ = Url::revoke_object_url(&url);
}

pub async fn canvas_to_jpeg_blob(canvas: &HtmlCanvasElement, quality: f64) -> Result<Blob, JsValue> {
    let promise = Promise::new(&mut |resolve, reject| {
        let rej = reject.clone();
        let cb = Closure::once(move |v: JsValue| {
            if v.is_null() || v.is_undefined() {
                let _ = rej.call1(&JsValue::NULL, &JsValue::from_str("to_blob failed"));
            } else {
                let _ = resolve.call1(&JsValue::NULL, &v);
            }
        });
        canvas
            .to_blob_with_type_and_encoder_options(
                cb.as_ref().unchecked_ref(),
                "image/jpeg",
                &JsValue::from_f64(quality),
            )
            .unwrap();
        cb.forget();
    });
    let out = JsFuture::from(promise).await?;
    Ok(out.unchecked_into())
}

pub fn log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}

pub fn log_err(msg: &str) {
    web_sys::console::error_1(&JsValue::from_str(msg));
}
