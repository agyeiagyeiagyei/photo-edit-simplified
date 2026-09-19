//! RAW photo import via rawler (pure-Rust decoder from the dnglab project).
//! Decode path: raw bytes → demosaic/develop to sRGB-gamma f32 RGB →
//! EXIF-orientation-corrected u8 RGBA for the normal photo pipeline.

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

/// Decode + develop a RAW file into orientation-corrected RGBA pixels.
pub fn decode_raw(bytes: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let rawfile = RawSource::new_from_slice(bytes);
    let decoder = rawler::get_decoder(&rawfile).map_err(|e| format!("unsupported RAW: {e}"))?;
    let rawimage = decoder
        .raw_image(&rawfile, &RawDecodeParams { image_index: 0 }, false)
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
    Ok(apply_orientation(rgba, w, h, orientation))
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
}
