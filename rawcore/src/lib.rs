//! RAW decode core shared by the app and the pixel worker: demosaic via
//! rawler, embedded-JPEG previews, and EXIF orientation. No browser APIs, so
//! it compiles for both wasm32 targets unchanged.

use rawler::decoders::RawDecodeParams;
use rawler::imgop::develop::{Intermediate, RawDevelop};
use rawler::rawsource::RawSource;

pub const RAW_EXTENSIONS: [&str; 11] = [
    "dng", "cr2", "cr3", "nef", "nrw", "arw", "orf", "rw2", "raf", "pef", "srw",
];

pub fn is_raw_name(name: &str) -> bool {
    let n = name.to_lowercase();
    RAW_EXTENSIONS.iter().any(|ext| n.ends_with(&format!(".{ext}")))
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
    let orientation = rawimage.orientation;
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
    let (mut rgba, w, h) = apply_orientation(rgba, w, h, orientation);
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
    Some(apply_orientation(rgba.into_raw(), w, h, raw_orientation(bytes)))
}

/// Largest JPEG embedded in a TIFF-based RAW's main IFD chain, with its pixel
/// size. rawler reads only IFD0 — the 1616×1080 preview on Sony bodies — while
/// the full-size camera JPEG sits in a later IFD of the same chain.
pub fn largest_embedded_jpeg(bytes: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let endian = match bytes.get(0..2)? {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return None,
    };
    if rd16(bytes, 2, endian)? != 42 {
        return None;
    }
    let mut ifd = rd32(bytes, 4, endian)? as usize;
    let mut seen = Vec::new();
    let mut best: Option<(usize, usize)> = None;
    while ifd >= 8 && seen.len() < 32 && !seen.contains(&ifd) {
        seen.push(ifd);
        let Some(entries) = ifd.checked_add(2) else { break };
        if entries + 2 > bytes.len() {
            break;
        }
        let count = rd16(bytes, ifd, endian)? as usize;
        let Some(next_at) = entries.checked_add(count * 12) else {
            break;
        };
        if next_at + 4 > bytes.len() {
            break;
        }
        let (mut joff, mut jlen) = (None, None);
        for i in 0..count {
            let e = entries + i * 12;
            let tag = rd16(bytes, e, endian)?;
            if tag != 0x0201 && tag != 0x0202 {
                continue;
            }
            let typ = rd16(bytes, e + 2, endian)?;
            if rd32(bytes, e + 4, endian)? != 1 {
                continue;
            }
            let value = match typ {
                3 => rd16(bytes, e + 8, endian)? as usize,
                4 => rd32(bytes, e + 8, endian)? as usize,
                _ => continue,
            };
            if tag == 0x0201 {
                joff = Some(value);
            } else {
                jlen = Some(value);
            }
        }
        if let (Some(o), Some(l)) = (joff, jlen) {
            let in_file = l >= 4
                && o >= 8
                && o.checked_add(l).map_or(false, |end| end <= bytes.len());
            if in_file
                && bytes[o] == 0xFF
                && bytes[o + 1] == 0xD8
                && best.map_or(true, |(_, bl)| l > bl)
            {
                best = Some((o, l));
            }
        }
        ifd = rd32(bytes, next_at, endian)? as usize;
    }
    let (o, l) = best?;
    let (w, h) = jpeg_dimensions(&bytes[o..o + l])?;
    Some((bytes[o..o + l].to_vec(), w, h))
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

fn rd16(bytes: &[u8], at: usize, endian: Endian) -> Option<u16> {
    let s = bytes.get(at..at + 2)?;
    Some(match endian {
        Endian::Little => u16::from_le_bytes([s[0], s[1]]),
        Endian::Big => u16::from_be_bytes([s[0], s[1]]),
    })
}

fn rd32(bytes: &[u8], at: usize, endian: Endian) -> Option<u32> {
    let s = bytes.get(at..at + 4)?;
    Some(match endian {
        Endian::Little => u32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        Endian::Big => u32::from_be_bytes([s[0], s[1], s[2], s[3]]),
    })
}

/// Dimensions from the first Start-Of-Frame segment, without decoding pixels.
fn jpeg_dimensions(j: &[u8]) -> Option<(usize, usize)> {
    if j.get(0..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    while i + 4 <= j.len() {
        if j[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = j[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            return None;
        }
        let len = u16::from_be_bytes([j[i + 2], j[i + 3]]) as usize;
        if len < 2 || i + 2 + len > j.len() {
            return None;
        }
        let sof = (0xC0..=0xCF).contains(&marker)
            && marker != 0xC4
            && marker != 0xC8
            && marker != 0xCC;
        if sof {
            if i + 9 > j.len() {
                return None;
            }
            let h = u16::from_be_bytes([j[i + 5], j[i + 6]]) as usize;
            let w = u16::from_be_bytes([j[i + 7], j[i + 8]]) as usize;
            return (w > 0 && h > 0).then_some((w, h));
        }
        i += 2 + len;
    }
    None
}

fn raw_orientation(bytes: &[u8]) -> rawler::decoders::Orientation {
    let rawfile = RawSource::new_from_slice(bytes);
    rawler::get_decoder(&rawfile)
        .ok()
        .and_then(|d| {
            d.raw_metadata(&rawfile, &RawDecodeParams { image_index: 0 })
                .ok()
        })
        .and_then(|m| m.exif.orientation)
        .map(rawler::decoders::Orientation::from_u16)
        .unwrap_or(rawler::decoders::Orientation::Normal)
}

/// Apply the RAW's metadata orientation to pixels decoded from an embedded
/// JPEG, which carries no EXIF of its own.
pub fn orientation_corrected(
    rgba: Vec<u8>,
    w: usize,
    h: usize,
    raw_bytes: &[u8],
) -> (Vec<u8>, usize, usize) {
    apply_orientation(rgba, w, h, raw_orientation(raw_bytes))
}

/// EXIF orientation code (1–8) from RAW metadata, for storing alongside a
/// cached proxy so later decodes need not re-parse the RAW header.
pub fn orientation_code(bytes: &[u8]) -> u16 {
    raw_orientation(bytes).to_u16()
}

pub fn orientation_corrected_code(
    rgba: Vec<u8>,
    w: usize,
    h: usize,
    code: u16,
) -> (Vec<u8>, usize, usize) {
    apply_orientation(rgba, w, h, rawler::decoders::Orientation::from_u16(code))
}

/// Decode an embedded JPEG (camera preview) into orientation-corrected RGBA.
pub fn camera_jpeg_rgba(jpeg: &[u8], orientation: u16) -> Option<(Vec<u8>, usize, usize)> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Some(orientation_corrected_code(rgba.into_raw(), w, h, orientation))
}

/// rawler gives orientation as (transpose, flip_x, flip_y); per its docs the
/// flips must be applied before the transpose.
fn apply_orientation(
    rgba: Vec<u8>,
    w: usize,
    h: usize,
    o: rawler::decoders::Orientation,
) -> (Vec<u8>, usize, usize) {
    let (do_transpose, flip_x, flip_y) = o.to_flips();
    let mut px = rgba;
    if flip_x {
        px = flip_horizontal(&px, w, h);
    }
    if flip_y {
        px = flip_vertical(&px, w, h);
    }
    if do_transpose {
        let (out, nw, nh) = transpose(&px, w, h);
        (out, nw, nh)
    } else {
        (px, w, h)
    }
}

fn flip_horizontal(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    for y in 0..h {
        for x in 0..w {
            let s = (y * w + x) * 4;
            let d = (y * w + (w - 1 - x)) * 4;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

fn flip_vertical(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let row = w * 4;
    let mut out = vec![0u8; src.len()];
    for y in 0..h {
        let d = (h - 1 - y) * row;
        out[d..d + row].copy_from_slice(&src[y * row..(y + 1) * row]);
    }
    out
}

fn transpose(src: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let mut out = vec![0u8; src.len()];
    for y in 0..h {
        for x in 0..w {
            let s = (y * w + x) * 4;
            let d = (x * h + y) * 4;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    (out, h, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(r: u8, g: u8, b: u8) -> [u8; 4] {
        [r, g, b, 255]
    }

    #[test]
    fn raw_name_detection() {
        assert!(is_raw_name("IMG_0001.CR2"));
        assert!(is_raw_name("photo.dng"));
        assert!(is_raw_name("A.RAW".replace(".RAW", ".arw").as_str()));
        assert!(!is_raw_name("photo.jpg"));
        assert!(!is_raw_name("video.mp4"));
    }

    #[test]
    fn flip_horizontal_mirrors() {
        let src = [px(1, 0, 0), px(2, 0, 0)].concat();
        let out = flip_horizontal(&src, 2, 1);
        assert_eq!(&out[0..4], &px(2, 0, 0));
        assert_eq!(&out[4..8], &px(1, 0, 0));
    }

    #[test]
    fn flip_vertical_mirrors() {
        let src = [px(1, 0, 0), px(1, 0, 0), px(2, 0, 0), px(2, 0, 0)].concat();
        let out = flip_vertical(&src, 2, 2);
        assert_eq!(&out[0..4], &px(2, 0, 0));
        assert_eq!(&out[12..16], &px(1, 0, 0));
    }

    #[test]
    fn transpose_swaps_dims() {
        // 2x1: [A B] -> 1x2: [A; B]
        let src = [px(1, 0, 0), px(2, 0, 0)].concat();
        let (out, w, h) = transpose(&src, 2, 1);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[0..4], &px(1, 0, 0));
        assert_eq!(&out[4..8], &px(2, 0, 0));
    }

    #[test]
    fn orientation_rotate90_matches_exif() {
        // EXIF 6 (Rotate90) = transpose + flip... verify via to_flips mapping:
        // Rotate90 → (true, false, true): flip vertical then transpose.
        // 2x1 [A B] -> flip_v (no-op, h=1) -> transpose -> 1x2 [A; B]
        let src = [px(9, 0, 0), px(5, 0, 0)].concat();
        let (out, w, h) = apply_orientation(src, 2, 1, rawler::decoders::Orientation::Rotate90);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[0..4], &px(9, 0, 0));
        assert_eq!(&out[4..8], &px(5, 0, 0));
    }

    #[test]
    fn garbage_bytes_error_not_panic() {
        assert!(decode_raw(b"not a raw file").is_err());
    }

    #[test]
    fn empty_bytes_error_not_panic() {
        assert!(decode_raw(&[]).is_err());
    }

    fn fake_jpeg(w: u16, h: u16, pad: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08];
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        v.extend(std::iter::repeat(0u8).take(pad));
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    fn put_entry(b: &mut [u8], at: usize, tag: u16, typ: u16, value: u32) {
        b[at..at + 2].copy_from_slice(&tag.to_le_bytes());
        b[at + 2..at + 4].copy_from_slice(&typ.to_le_bytes());
        b[at + 4..at + 8].copy_from_slice(&1u32.to_le_bytes());
        b[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn finds_largest_jpeg_across_tiff_ifds() {
        let small = fake_jpeg(160, 120, 4);
        let large = fake_jpeg(6000, 4000, 40);
        let small_at = 200usize;
        let large_at = small_at + small.len();
        let mut b = vec![0u8; large_at + large.len()];
        b[0..2].copy_from_slice(b"II");
        b[2..4].copy_from_slice(&42u16.to_le_bytes());
        b[4..8].copy_from_slice(&8u32.to_le_bytes());
        // IFD0: the 160x120 preview + link to IFD1.
        let ifd0 = 8usize;
        let ifd1 = 38usize;
        b[ifd0..ifd0 + 2].copy_from_slice(&2u16.to_le_bytes());
        put_entry(&mut b, ifd0 + 2, 0x0201, 4, small_at as u32);
        put_entry(&mut b, ifd0 + 14, 0x0202, 4, small.len() as u32);
        b[ifd0 + 26..ifd0 + 30].copy_from_slice(&(ifd1 as u32).to_le_bytes());
        // IFD1: the full-size camera JPEG + end of chain.
        b[ifd1..ifd1 + 2].copy_from_slice(&2u16.to_le_bytes());
        put_entry(&mut b, ifd1 + 2, 0x0201, 4, large_at as u32);
        put_entry(&mut b, ifd1 + 14, 0x0202, 4, large.len() as u32);
        b[ifd1 + 26..ifd1 + 30].copy_from_slice(&0u32.to_le_bytes());
        b[small_at..small_at + small.len()].copy_from_slice(&small);
        b[large_at..large_at + large.len()].copy_from_slice(&large);

        let (jpeg, w, h) = largest_embedded_jpeg(&b).unwrap();
        assert_eq!((w, h), (6000, 4000));
        assert_eq!(jpeg, large);
    }

    #[test]
    fn embedded_jpeg_lookup_rejects_garbage() {
        assert!(largest_embedded_jpeg(b"not a tiff").is_none());
        assert!(largest_embedded_jpeg(&[]).is_none());
    }
}
