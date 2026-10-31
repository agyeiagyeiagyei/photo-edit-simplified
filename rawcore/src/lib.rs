//! RAW decode core for the pixel worker: demosaic via rawler, embedded-JPEG
//! previews, EXIF orientation, and the metadata payload the app requests.
//! No browser APIs outside the wasm-only perf clock, so it compiles for both
//! wasm32 targets unchanged.

use rawler::decoders::{Decoder, RawDecodeParams, RawMetadata};
use rawler::imgop::develop::{Intermediate, RawDevelop};
use rawler::imgop::sensor::SensorType;
use rawler::rawimage::{RawImage, RawPhotometricInterpretation};
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
    develop_rgba(bytes, None)
}

/// Full develop reusing a tone curve computed by an earlier proxy develop, so
/// the export renders on the same curve as the proxy.
pub fn decode_raw_with_tone(
    bytes: &[u8],
    tone: Option<&[u8; 256]>,
) -> Result<(Vec<u8>, usize, usize), String> {
    develop_rgba(bytes, tone)
}

fn develop_rgba(
    bytes: &[u8],
    tone: Option<&[u8; 256]>,
) -> Result<(Vec<u8>, usize, usize), String> {
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
    let (mut rgba, w, h) = intermediate_rgba(intermediate, orientation);
    match tone {
        Some(lut) => apply_tone_lut(&mut rgba, lut),
        None => {
            if let Some(preview) = camera_preview_rgb(decoder.as_ref(), &rawfile, &params) {
                if let Some(lut) = camera_tone_lut(&rgba, &preview) {
                    apply_tone_lut(&mut rgba, &lut);
                }
            }
        }
    }
    Ok((rgba, w, h))
}

/// A display proxy develop: pixels plus the tone curve used, the true sensor
/// dims, and whether the 2×2 superpixel binning produced half-size output.
pub struct RawProxy {
    pub rgba: Vec<u8>,
    pub w: usize,
    pub h: usize,
    pub full_w: usize,
    pub full_h: usize,
    pub tone: Option<[u8; 256]>,
    pub half: bool,
}

/// Half-size develop when the sensor's binned frame still covers the display
/// cap; otherwise the normal full develop. The tone curve is computed here and
/// returned so a later full develop for export reuses it.
pub fn decode_raw_proxy(bytes: &[u8], max_edge: u32) -> Result<RawProxy, String> {
    let rawfile = RawSource::new_from_slice(bytes);
    let decoder = rawler::get_decoder(&rawfile).map_err(|e| format!("unsupported RAW: {e}"))?;
    let params = RawDecodeParams { image_index: 0 };
    let rawimage = decoder
        .raw_image(&rawfile, &params, false)
        .map_err(|e| format!("RAW decode failed: {e}"))?;
    let orientation = rawimage.orientation.to_u16();
    // Binning halves the long edge. A 26 MP body lands at 3 096 px for a
    // 3 200 px cap — close enough to keep display density — while a 10 MP
    // body would drop to ~1 900 px and visibly soften the proxy.
    let half = max_edge > 0
        && is_bayer_rgb(&rawimage)
        && (rawimage.width.max(rawimage.height) as f64) * 0.5
            >= max_edge as f64 * SUPERPIXEL_MIN_COVER;
    let develop = if half { RawDevelop::superpixel() } else { RawDevelop::default() };
    let intermediate = develop
        .develop_intermediate(&rawimage)
        .map_err(|e| format!("RAW develop failed: {e}"))?;
    let (mut rgba, w, h) = intermediate_rgba(intermediate, orientation);
    let tone = camera_preview_rgb(decoder.as_ref(), &rawfile, &params)
        .and_then(|preview| camera_tone_lut(&rgba, &preview));
    if let Some(lut) = &tone {
        apply_tone_lut(&mut rgba, lut);
    }
    let (full_w, full_h) = if half { (w * 2, h * 2) } else { (w, h) };
    Ok(RawProxy { rgba, w, h, full_w, full_h, tone, half })
}

/// Minimum fraction of the display cap a half-size proxy must cover.
const SUPERPIXEL_MIN_COVER: f64 = 0.9;

fn is_bayer_rgb(rawimage: &RawImage) -> bool {
    match &rawimage.photometric {
        RawPhotometricInterpretation::Cfa(config) => {
            config.cfa.is_rgb() && config.sensor == SensorType::Bayer
        }
        _ => false,
    }
}

/// 0..1 float → 0..255 byte table (12-bit quantised). Beats a per-channel
/// round().clamp() in the develop's innermost loop.
fn px_table() -> &'static [u8; 4097] {
    use std::sync::OnceLock;
    static LUT: OnceLock<[u8; 4097]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u8; 4097];
        for (i, v) in t.iter_mut().enumerate() {
            *v = ((i as f32 / 4096.0) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        t
    })
}

#[inline]
fn to_u8(v: f32) -> u8 {
    px_table()[(v.clamp(0.0, 1.0) * 4096.0) as usize]
}

/// Float intermediate → orientation-corrected RGBA in one write. Folding the
/// EXIF orientation into the destination index avoids a second full-frame
/// copy (and the transpose allocation) after the develop.
fn intermediate_rgba(intermediate: Intermediate, code: u16) -> (Vec<u8>, usize, usize) {
    let (transpose, flip_x, flip_y) = rawmeta::orientation_flips(code);
    match intermediate {
        Intermediate::ThreeColor(rgb) => {
            let out = write_pixels(&rgb.data, rgb.width, rgb.height, transpose, flip_x, flip_y, |px| {
                (px[0], px[1], px[2])
            });
            let (ow, oh) = rawmeta::oriented_dims(rgb.width, rgb.height, code);
            (out, ow, oh)
        }
        Intermediate::Monochrome(p) => {
            let out = write_pixels(&p.data, p.width, p.height, transpose, flip_x, flip_y, |&v| (v, v, v));
            let (ow, oh) = rawmeta::oriented_dims(p.width, p.height, code);
            (out, ow, oh)
        }
        Intermediate::FourColor(p) => {
            let out = write_pixels(&p.data, p.width, p.height, transpose, flip_x, flip_y, |px| {
                (px[0], px[1], px[2])
            });
            let (ow, oh) = rawmeta::oriented_dims(p.width, p.height, code);
            (out, ow, oh)
        }
    }
}

fn write_pixels<T: Copy>(
    data: &[T],
    w: usize,
    h: usize,
    transpose: bool,
    flip_x: bool,
    flip_y: bool,
    rgb: impl Fn(&T) -> (f32, f32, f32),
) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    let ow = if transpose { h } else { w };
    if !transpose && !flip_x && !flip_y {
        for (dst, px) in out.chunks_exact_mut(4).zip(data.iter()) {
            let (r, g, b) = rgb(px);
            dst[0] = to_u8(r);
            dst[1] = to_u8(g);
            dst[2] = to_u8(b);
            dst[3] = 255;
        }
        return out;
    }
    for (i, px) in data.iter().enumerate() {
        let x = i % w;
        let y = i / w;
        let xf = if flip_x { w - 1 - x } else { x };
        let yf = if flip_y { h - 1 - y } else { y };
        let (dx, dy) = if transpose { (yf, xf) } else { (xf, yf) };
        let d = (dy * ow + dx) * 4;
        let (r, g, b) = rgb(px);
        out[d] = to_u8(r);
        out[d + 1] = to_u8(g);
        out[d + 2] = to_u8(b);
        out[d + 3] = 255;
    }
    out
}

fn camera_preview_rgb(
    decoder: &dyn Decoder,
    rawfile: &RawSource,
    params: &RawDecodeParams,
) -> Option<Vec<u8>> {
    let img = decoder
        .full_image(rawfile, params)
        .ok()
        .flatten()
        .or_else(|| decoder.preview_image(rawfile, params).ok().flatten())
        .or_else(|| decoder.thumbnail_image(rawfile, params).ok().flatten())?;
    Some(img.into_rgb8().into_raw())
}

/// Match the developed image's luminance to the embedded camera preview by
/// quantile, applied as a per-channel curve — rawler develops neutrally with no
/// exposure compensation or camera tone curve, which leaves RAW frames ~1 EV
/// darker and flatter than the camera's own JPEG.
fn camera_tone_lut(rgba: &[u8], preview_rgb: &[u8]) -> Option<[u8; 256]> {
    let (dev_h, dev_n) = luma_hist(rgba.chunks_exact(4).map(|p| (p[0], p[1], p[2])));
    let (prev_h, prev_n) = luma_hist(preview_rgb.chunks_exact(3).map(|p| (p[0], p[1], p[2])));
    if dev_n == 0 || prev_n == 0 {
        return None;
    }
    let dev_c = cdf(&dev_h, dev_n);
    let prev_c = cdf(&prev_h, prev_n);
    // Refuse to stretch toward a preview with almost no tonal spread (a flat
    // frame or a broken thumbnail) — neutral develop is the safer fallback.
    if spread(&dev_c) < 8 || spread(&prev_c) < 8 {
        return None;
    }
    let mut lut = [0u8; 256];
    let mut p = 0usize;
    for (v, lut_v) in lut.iter_mut().enumerate() {
        while p < 255 && prev_c[p] < dev_c[v] {
            p += 1;
        }
        *lut_v = p as u8;
    }
    Some(lut)
}

fn apply_tone_lut(rgba: &mut [u8], lut: &[u8; 256]) {
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

    /// Pre-WS3B writer: round().clamp() per pixel, then a separate orientation
    /// pass. The fused writer quantises through a 12-bit LUT and folds the
    /// orientation into the write, so its output must agree within 1 LSB —
    /// the A/B tolerance signed off for WS3 #7.
    fn reference_rgba(intermediate: Intermediate) -> (Vec<u8>, usize, usize) {
        let (data, w, h): (Vec<[f32; 4]>, usize, usize) = match intermediate {
            Intermediate::ThreeColor(c) => (
                c.data.iter().map(|p| [p[0], p[1], p[2], 255.0]).collect(),
                c.width,
                c.height,
            ),
            Intermediate::Monochrome(c) => (
                c.data.iter().map(|&v| [v, v, v, 255.0]).collect(),
                c.width,
                c.height,
            ),
            Intermediate::FourColor(c) => (
                c.data.iter().map(|p| [p[0], p[1], p[2], 255.0]).collect(),
                c.width,
                c.height,
            ),
        };
        let mut out = Vec::with_capacity(data.len() * 4);
        for px in &data {
            for &v in px {
                out.push((v * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
        (out, w, h)
    }

    fn sample_cells(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let v = ((i * 7919) % 4096) as f32 / 4095.0;
                if i % 97 == 0 {
                    -0.05
                } else if i % 89 == 0 {
                    1.05
                } else {
                    v
                }
            })
            .collect()
    }

    #[test]
    fn fused_writer_matches_reference_writer() {
        use rawler::pixarray::{Color2D, PixF32};
        let (w, h) = (37usize, 23usize);
        let n = w * h;
        let rgb: Vec<[f32; 3]> = sample_cells(n * 3).chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
        let mono = sample_cells(n);
        let four: Vec<[f32; 4]> =
            sample_cells(n * 4).chunks_exact(4).map(|c| [c[0], c[1], c[2], c[3]]).collect();

        for code in 1..=8u16 {
            let cases = [
                Intermediate::ThreeColor(Color2D { width: w, height: h, data: rgb.clone() }),
                Intermediate::Monochrome(PixF32 { width: w, height: h, data: mono.clone(), initialized: true }),
                Intermediate::FourColor(Color2D { width: w, height: h, data: four.clone() }),
            ];
            for case in cases {
                let (got, gw, gh) = intermediate_rgba(case.clone(), code);
                let (want, ww, wh) = rawmeta::apply_orientation_code(
                    reference_rgba(case).0,
                    w,
                    h,
                    code,
                );
                assert_eq!((gw, gh), (ww, wh), "dims differ for code {code}");
                assert_eq!(got.len(), want.len());
                let mut sum = 0u64;
                let mut max = 0u8;
                for (a, b) in got.iter().zip(want.iter()) {
                    let d = a.abs_diff(*b);
                    sum += d as u64;
                    max = max.max(d);
                }
                let mean = sum as f64 / got.len() as f64;
                assert!(max <= 1, "code {code}: {max} LSB max deviation");
                assert!(mean <= 1.0, "code {code}: mean deviation {mean} above 1/255");
                // Alpha stays opaque everywhere.
                assert!(got.chunks_exact(4).all(|p| p[3] == 255));
            }
        }
    }

    #[test]
    fn tone_lut_identity_for_matching_histograms() {
        let mut rgba = Vec::new();
        for v in 0..=255u8 {
            for _ in 0..4 {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let lut = camera_tone_lut(&rgba, &rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect::<Vec<_>>())
            .unwrap();
        for (v, mapped) in lut.iter().enumerate() {
            assert!((*mapped as i32 - v as i32).abs() <= 1, "lut[{v}] = {mapped}");
        }
    }

    #[test]
    fn tone_lut_brightens_toward_preview() {
        let dark: Vec<u8> = (0..=255u8)
            .flat_map(|v| [v, v, v, 255].repeat(4))
            .collect();
        let bright: Vec<u8> = (0..=255u8)
            .flat_map(|v| {
                let b = (v as u32 * 2).min(255) as u8;
                [b, b, b].repeat(4)
            })
            .collect();
        let lut = camera_tone_lut(&dark, &bright).unwrap();
        assert!(lut[128] > 128);
        assert!(lut[200] >= 200);
        assert!(lut.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn tone_lut_none_for_flat_frames() {
        let flat = vec![120u8, 120, 120, 255].repeat(1024);
        let preview = vec![200u8, 200, 200].repeat(1024);
        assert!(camera_tone_lut(&flat, &preview).is_none());
    }

    #[test]
    fn tone_lut_leaves_alpha_untouched() {
        let mut rgba: Vec<u8> = (0..=255u8).flat_map(|v| [v, v / 2, 255 - v, 7]).collect();
        let lut = [255u8; 256];
        apply_tone_lut(&mut rgba, &lut);
        assert!(rgba.chunks_exact(4).all(|p| p == [255, 255, 255, 7]));
    }

    /// Best PSNR between a half-size image and a 2×2 box-binned full-size
    /// image. The default crop is scaled with `(x * 0.5) as usize`, so an odd
    /// crop origin can shift the binning grid by one sensor pixel; the four
    /// block alignments are tried and the best one wins.
    fn best_block_psnr(half: &[u8], hw: usize, hh: usize, full: &[u8], fw: usize, fh: usize) -> f64 {
        let w = (fw / 2).min(hw);
        let h = (fh / 2).min(hh);
        assert!(w > 0 && h > 0, "frames too small: {hw}x{hh} vs {fw}x{fh}");
        let psnr_at = |ox: usize, oy: usize| {
            let w = w.min((fw - ox) / 2);
            let h = h.min((fh - oy) / 2);
            let mut se = 0.0f64;
            let mut n = 0u64;
            for y in 0..h {
                for x in 0..w {
                    for c in 0..3 {
                        let mut sum = 0u32;
                        for dy in 0..2 {
                            for dx in 0..2 {
                                sum += full[((y * 2 + dy + oy) * fw + x * 2 + dx + ox) * 4 + c] as u32;
                            }
                        }
                        let down = (sum as f64 / 4.0).round() as i32;
                        let diff = down - half[(y * hw + x) * 4 + c] as i32;
                        se += (diff * diff) as f64;
                        n += 1;
                    }
                }
            }
            let mse = se / n as f64;
            if mse == 0.0 {
                f64::INFINITY
            } else {
                10.0 * (255.0f64 * 255.0 / mse).log10()
            }
        };
        let mut psnr = f64::NEG_INFINITY;
        for oy in 0..2 {
            for ox in 0..2 {
                psnr = psnr.max(psnr_at(ox, oy));
            }
        }
        psnr
    }

    /// Proxy vs export parity on a real file: `RAW_FILE=<path>` where the
    /// sensor's half-size develop still covers the 3 200 px cap.
    #[test]
    fn superpixel_proxy_matches_export() {
        let Ok(path) = std::env::var("RAW_FILE") else {
            return;
        };
        let bytes = std::fs::read(&path).expect("read RAW_FILE");
        let proxy = decode_raw_proxy(&bytes, 3200).expect("proxy develop");
        if !proxy.half {
            eprintln!("RAW_FILE {} is too small for the superpixel path; skipped", path);
            return;
        }

        // Demosaic-only parity: develop the same sensor half-size (superpixel)
        // and full-size (PPG) with no tone curve. This isolates the fine-detail
        // difference between the two demosaics.
        let rawfile = RawSource::new_from_slice(&bytes);
        let decoder = rawler::get_decoder(&rawfile).expect("decoder");
        let params = RawDecodeParams { image_index: 0 };
        let rawimage = decoder.raw_image(&rawfile, &params, false).expect("raw image");
        let (hp, hw, hh) = intermediate_rgba(RawDevelop::superpixel().develop_intermediate(&rawimage).expect("superpixel develop"), 1);
        let (fp, fw, fh) = intermediate_rgba(RawDevelop::default().develop_intermediate(&rawimage).expect("full develop"), 1);
        let demosaic_psnr = best_block_psnr(&hp, hw, hh, &fp, fw, fh);
        eprintln!("demosaic-only PSNR: {demosaic_psnr:.2} dB ({hw}x{hh})");
        assert!(demosaic_psnr >= 40.0, "demosaic-only PSNR {demosaic_psnr:.2} dB below 40");

        // End to end: the export reuses the proxy's tone curve, as the worker does.
        let (full, fw, fh) = decode_raw_with_tone(&bytes, proxy.tone.as_ref()).expect("full develop");
        let e2e_psnr = best_block_psnr(&proxy.rgba, proxy.w, proxy.h, &full, fw, fh);
        eprintln!("toned proxy vs toned export PSNR: {e2e_psnr:.2} dB ({}x{})", proxy.w, proxy.h);
        // A steep tone curve amplifies residual demosaic differences: on the
        // synthetic stress fixture it costs ~3 dB (40.2 → 36.9). Much lower
        // means the LUT or the crop alignment broke.
        assert!(e2e_psnr >= 30.0, "toned PSNR {e2e_psnr:.2} dB below 30");
    }
}
