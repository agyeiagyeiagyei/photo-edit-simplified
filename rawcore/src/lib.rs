//! RAW decode core for the pixel worker: demosaic via rawler, embedded-JPEG
//! previews, EXIF orientation, and the metadata payload the app requests.
//! No browser APIs outside the wasm-only perf clock, so it compiles for both
//! wasm32 targets unchanged.

use rawler::decoders::{RawDecodeParams, RawMetadata};
use rawler::imgop::develop::{Intermediate, RawDevelop};
use rawler::rawsource::RawSource;

use rawmeta::tiff as tw;

pub use rawmeta::{is_raw_name, largest_embedded_jpeg, RAW_EXTENSIONS};

/// Milliseconds for perf marks. wasm32 has no std clock; host builds only run
/// this in tests, where the value is never asserted.
fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}

/// Decode + develop a RAW file into orientation-corrected RGBA pixels, then
/// tone-match to the camera's embedded JPEG preview so the result sits on the
/// camera's exposure and contrast instead of rawler's flat baseline.
pub fn decode_raw(bytes: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let rawfile = RawSource::new_from_slice(bytes);
    let decoder = rawler::get_decoder(&rawfile).map_err(|e| format!("unsupported RAW: {e}"))?;
    let params = RawDecodeParams { image_index: 0 };
    let rawimage = decoder
        .raw_image(&rawfile, &params, false)
        .map_err(|e| format!("RAW decode failed: {e}"))?;
    let orientation = rawimage.orientation.to_u16();
    let develop = RawDevelop::default();
    let intermediate = develop
        .develop_intermediate(&rawimage)
        .map_err(|e| format!("RAW develop failed: {e}"))?;

    let (rgba, w, h) = match intermediate {
        Intermediate::ThreeColor(rgb) => {
            let mut out = Vec::with_capacity(rgb.data.len() * 4);
            for px in &rgb.data {
                out.push((px[0] * 255.0).round().clamp(0.0, 255.0) as u8);
                out.push((px[1] * 255.0).round().clamp(0.0, 255.0) as u8);
                out.push((px[2] * 255.0).round().clamp(0.0, 255.0) as u8);
                out.push(255);
            }
            (out, rgb.width, rgb.height)
        }
        Intermediate::Monochrome(p) => {
            let mut out = Vec::with_capacity(p.data.len() * 4);
            for &v in &p.data {
                let g = (v * 255.0).round().clamp(0.0, 255.0) as u8;
                out.extend_from_slice(&[g, g, g, 255]);
            }
            (out, p.width, p.height)
        }
        Intermediate::FourColor(p) => {
            let mut out = Vec::with_capacity(p.data.len() * 4);
            for px in &p.data {
                out.push((px[0] * 255.0).round().clamp(0.0, 255.0) as u8);
                out.push((px[1] * 255.0).round().clamp(0.0, 255.0) as u8);
                out.push((px[2] * 255.0).round().clamp(0.0, 255.0) as u8);
                out.push(255);
            }
            (out, p.width, p.height)
        }
    };
    let (mut rgba, w, h) = rawmeta::apply_orientation_code(rgba, w, h, orientation);
    let preview = decoder
        .full_image(&rawfile, &params)
        .ok()
        .flatten()
        .or_else(|| decoder.preview_image(&rawfile, &params).ok().flatten())
        .or_else(|| decoder.thumbnail_image(&rawfile, &params).ok().flatten());
    if let Some(img) = preview {
        apply_camera_tone(&mut rgba, &img.into_rgb8().into_raw());
    }
    Ok((rgba, w, h))
}

/// Match the developed image's luminance to the embedded camera preview by
/// quantile, applied as a per-channel curve — rawler develops neutrally with no
/// exposure compensation or camera tone curve, which leaves RAW frames ~1 EV
/// darker and flatter than the camera's own JPEG.
fn apply_camera_tone(rgba: &mut [u8], preview_rgb: &[u8]) {
    let (dev_h, dev_n) = luma_hist(rgba.chunks_exact(4).map(|p| (p[0], p[1], p[2])));
    let (prev_h, prev_n) = luma_hist(preview_rgb.chunks_exact(3).map(|p| (p[0], p[1], p[2])));
    if dev_n == 0 || prev_n == 0 {
        return;
    }
    let dev_c = cdf(&dev_h, dev_n);
    let prev_c = cdf(&prev_h, prev_n);
    // Refuse to stretch toward a preview with almost no tonal spread (a flat
    // frame or a broken thumbnail) — neutral develop is the safer fallback.
    if spread(&dev_c) < 8 || spread(&prev_c) < 8 {
        return;
    }
    let mut lut = [0u8; 256];
    let mut p = 0usize;
    for (v, lut_v) in lut.iter_mut().enumerate() {
        while p < 255 && prev_c[p] < dev_c[v] {
            p += 1;
        }
        *lut_v = p as u8;
    }
    for px in rgba.chunks_exact_mut(4) {
        px[0] = lut[px[0] as usize];
        px[1] = lut[px[1] as usize];
        px[2] = lut[px[2] as usize];
    }
}

fn luma_hist(px: impl Iterator<Item = (u8, u8, u8)>) -> ([u64; 256], u64) {
    let mut h = [0u64; 256];
    let mut n = 0u64;
    for (r, g, b) in px {
        let y = (2126 * r as u32 + 7152 * g as u32 + 722 * b as u32) / 10000;
        h[y as usize] += 1;
        n += 1;
    }
    (h, n)
}

fn cdf(hist: &[u64; 256], n: u64) -> [f32; 256] {
    let mut c = [0f32; 256];
    let mut acc = 0u64;
    for (i, &count) in hist.iter().enumerate() {
        acc += count;
        c[i] = acc as f32 / n as f32;
    }
    c
}

fn spread(c: &[f32; 256]) -> usize {
    let low = c.iter().position(|&v| v >= 0.01).unwrap_or(0);
    let high = c.iter().position(|&v| v >= 0.99).unwrap_or(255);
    high.saturating_sub(low)
}

/// Decode the largest embedded JPEG preview (no demosaic) — fast enough to run
/// on hover, unlike `decode_raw`. Returns orientation-corrected RGBA, or None
/// when the file carries no usable preview.
pub fn preview_rgba(bytes: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let rawfile = RawSource::new_from_slice(bytes);
    let decoder = rawler::get_decoder(&rawfile).ok()?;
    let params = RawDecodeParams { image_index: 0 };
    let img = decoder
        .full_image(&rawfile, &params)
        .ok()
        .flatten()
        .or_else(|| decoder.preview_image(&rawfile, &params).ok().flatten())
        .or_else(|| decoder.thumbnail_image(&rawfile, &params).ok().flatten())?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Some(rawmeta::apply_orientation_code(
        rgba.into_raw(),
        w,
        h,
        raw_orientation(bytes),
    ))
}

/// Everything the app needs from a RAW header in one worker round-trip:
/// EXIF payload, orientation, capture time, the camera JPEG, and a preview.
pub struct RawMeta {
    pub exif_tiff: Option<Vec<u8>>,
    pub orientation: u16,
    pub taken: Option<f64>,
    pub camera_jpeg: Option<Vec<u8>>,
    pub camera_w: usize,
    pub camera_h: usize,
    pub preview: Option<(Vec<u8>, usize, usize)>,
    /// `rawler::get_decoder` alone. The first call in a worker also builds
    /// rawler's camera database; that is the number the ≤50 ms gate measures.
    pub get_decoder_ms: f64,
}

pub fn raw_meta(bytes: &[u8]) -> Option<RawMeta> {
    let rawfile = RawSource::new_from_slice(bytes);
    let t0 = now_ms();
    let decoder = rawler::get_decoder(&rawfile).ok()?;
    let get_decoder_ms = now_ms() - t0;
    let params = RawDecodeParams { image_index: 0 };
    let meta = decoder.raw_metadata(&rawfile, &params).ok()?;
    let orientation = meta.exif.orientation.unwrap_or(1);
    let exif_tiff = Some(tiff_from_raw_metadata(&meta));
    let taken = exif_tiff.as_deref().and_then(rawmeta::taken_from_tiff);
    let (camera_jpeg, camera_w, camera_h) = match largest_embedded_jpeg(bytes) {
        Some((jpeg, w, h)) => (Some(jpeg), w, h),
        None => (None, 0, 0),
    };
    let preview = preview_rgba(bytes);
    Some(RawMeta {
        exif_tiff,
        orientation,
        taken,
        camera_jpeg,
        camera_w,
        camera_h,
        preview,
        get_decoder_ms,
    })
}

fn raw_orientation(bytes: &[u8]) -> u16 {
    let rawfile = RawSource::new_from_slice(bytes);
    rawler::get_decoder(&rawfile)
        .ok()
        .and_then(|d| {
            d.raw_metadata(&rawfile, &RawDecodeParams { image_index: 0 })
                .ok()
        })
        .and_then(|m| m.exif.orientation)
        .unwrap_or(1)
}

/// Decode an embedded JPEG (camera preview) into orientation-corrected RGBA.
pub fn camera_jpeg_rgba(jpeg: &[u8], orientation: u16) -> Option<(Vec<u8>, usize, usize)> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Some(rawmeta::apply_orientation_code(rgba.into_raw(), w, h, orientation))
}

// ---------- EXIF carry-through ----------

fn rat(tag: u16, r: rawler::formats::tiff::Rational) -> tw::Entry {
    tw::e_rat(tag, tw::Rational { n: r.n, d: r.d })
}

fn rats(tag: u16, rs: &[rawler::formats::tiff::Rational]) -> tw::Entry {
    let v: Vec<tw::Rational> = rs.iter().map(|r| tw::Rational { n: r.n, d: r.d }).collect();
    tw::e_rats(tag, &v)
}

fn srat(tag: u16, r: rawler::formats::tiff::SRational) -> tw::Entry {
    tw::e_srat(tag, tw::SRational { n: r.n, d: r.d })
}

/// Build a TIFF payload from a RAW file's metadata.
pub fn tiff_from_raw_metadata(meta: &RawMetadata) -> Vec<u8> {
    let x = &meta.exif;
    let mut ifd0 = vec![
        tw::e_ascii(0x010F, &meta.make),
        tw::e_ascii(0x0110, &meta.model),
        tw::e_short(0x0112, 1),
        tw::e_ascii(0x0131, "photo-edit-simplified"),
    ];
    if let Some(s) = &x.modify_date {
        ifd0.push(tw::e_ascii(0x0132, s));
    }
    if let Some(s) = &x.artist {
        ifd0.push(tw::e_ascii(0x013B, s));
    }
    if let Some(s) = &x.copyright {
        ifd0.push(tw::e_ascii(0x8298, s));
    }

    let mut ex: Vec<tw::Entry> = Vec::new();
    if let Some(r) = x.exposure_time {
        ex.push(rat(0x829A, r));
    }
    if let Some(r) = x.fnumber {
        ex.push(rat(0x829D, r));
    }
    if let Some(v) = x.exposure_program {
        ex.push(tw::e_short(0x8822, v));
    }
    let iso = x.iso_speed_ratings.map(|v| v as u32).or(x.iso_speed);
    if let Some(v) = iso {
        ex.push(tw::e_short(0x8827, v.min(u16::MAX as u32) as u16));
    }
    if let Some(v) = x.sensitivity_type {
        ex.push(tw::e_short(0x8830, v));
    }
    if let Some(v) = x.recommended_exposure_index {
        ex.push(tw::e_long(0x8832, v));
    }
    if let Some(s) = &x.date_time_original {
        ex.push(tw::e_ascii(0x9003, s));
    }
    if let Some(s) = &x.create_date {
        ex.push(tw::e_ascii(0x9004, s));
    }
    if let Some(s) = &x.offset_time {
        ex.push(tw::e_ascii(0x9010, s));
    }
    if let Some(s) = &x.offset_time_original {
        ex.push(tw::e_ascii(0x9011, s));
    }
    if let Some(r) = x.shutter_speed_value {
        ex.push(srat(0x9201, r));
    }
    if let Some(r) = x.aperture_value {
        ex.push(rat(0x9202, r));
    }
    if let Some(r) = x.brightness_value {
        ex.push(srat(0x9203, r));
    }
    if let Some(r) = x.exposure_bias {
        ex.push(srat(0x9204, r));
    }
    if let Some(r) = x.max_aperture_value {
        ex.push(rat(0x9205, r));
    }
    if let Some(r) = x.subject_distance {
        ex.push(rat(0x9206, r));
    }
    if let Some(v) = x.metering_mode {
        ex.push(tw::e_short(0x9207, v));
    }
    if let Some(v) = x.light_source {
        ex.push(tw::e_short(0x9208, v));
    }
    if let Some(v) = x.flash {
        ex.push(tw::e_short(0x9209, v));
    }
    if let Some(r) = x.focal_length {
        ex.push(rat(0x920A, r));
    }
    if let Some(r) = x.flash_energy {
        ex.push(rat(0xA20B, r));
    }
    // Exports are always sRGB.
    ex.push(tw::e_short(0xA001, x.color_space.unwrap_or(1)));
    if let Some(v) = x.exposure_mode {
        ex.push(tw::e_short(0xA402, v));
    }
    if let Some(v) = x.white_balance {
        ex.push(tw::e_short(0xA403, v));
    }
    if let Some(v) = x.scene_capture_type {
        ex.push(tw::e_short(0xA406, v));
    }
    if let Some(v) = x.subject_distance_range {
        ex.push(tw::e_short(0xA40C, v));
    }
    if let Some(s) = &x.owner_name {
        ex.push(tw::e_ascii(0xA430, s));
    }
    if let Some(s) = &x.serial_number {
        ex.push(tw::e_ascii(0xA431, s));
    }
    if let Some(rs) = &x.lens_spec {
        ex.push(rats(0xA432, rs));
    }
    if let Some(s) = &x.lens_make {
        ex.push(tw::e_ascii(0xA433, s));
    }
    if let Some(s) = &x.lens_model {
        ex.push(tw::e_ascii(0xA434, s));
    }
    if let Some(s) = &x.lens_serial_number {
        ex.push(tw::e_ascii(0xA435, s));
    }

    let mut gps_entries: Vec<tw::Entry> = Vec::new();
    if let Some(g) = &x.gps {
        if let Some(v) = g.gps_version_id {
            gps_entries.push(tw::Entry { tag: 0, typ: tw::BYTE, count: 4, data: v.to_vec() });
        }
        if let Some(s) = &g.gps_latitude_ref {
            gps_entries.push(tw::e_ascii(1, s));
        }
        if let Some(rs) = &g.gps_latitude {
            gps_entries.push(rats(2, rs));
        }
        if let Some(s) = &g.gps_longitude_ref {
            gps_entries.push(tw::e_ascii(3, s));
        }
        if let Some(rs) = &g.gps_longitude {
            gps_entries.push(rats(4, rs));
        }
        if let Some(v) = g.gps_altitude_ref {
            gps_entries.push(tw::e_byte(5, v));
        }
        if let Some(r) = g.gps_altitude {
            gps_entries.push(rat(6, r));
        }
        if let Some(rs) = &g.gps_timestamp {
            gps_entries.push(rats(7, rs));
        }
        if let Some(s) = &g.gps_map_datum {
            gps_entries.push(tw::e_ascii(18, s));
        }
        if let Some(s) = &g.gps_date_stamp {
            gps_entries.push(tw::e_ascii(29, s));
        }
    }

    tw::build_tiff(ifd0, ex, gps_entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_bytes_error_not_panic() {
        assert!(decode_raw(b"not a raw file").is_err());
    }

    #[test]
    fn empty_bytes_error_not_panic() {
        assert!(decode_raw(&[]).is_err());
    }
}
