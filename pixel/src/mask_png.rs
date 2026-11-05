//! Grayscale mask rasters for recipes: PNG bytes, base64 for JSON transport.

use std::io::Cursor;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode an 8-bit grayscale mask as a base64 PNG. The recipe carrier is JSON,
/// so the bytes need a text-safe form; PNG keeps a 1024² wand mask small.
pub fn encode_gray(data: &[u8], w: usize, h: usize) -> Option<String> {
    if w == 0 || h == 0 || data.len() != w * h {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(data).ok()?;
    }
    Some(b64_encode(&out))
}

/// Decode a base64 PNG back into grayscale bytes and dims. Color inputs are
/// accepted and reduced to their first channel.
pub fn decode_gray(encoded: &str) -> Option<(Vec<u8>, usize, usize)> {
    let bytes = b64_decode(encoded)?;
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let data = match info.color_type {
        png::ColorType::Grayscale => buf[..w * h].to_vec(),
        png::ColorType::GrayscaleAlpha => (0..w * h).map(|i| buf[i * 2]).collect(),
        png::ColorType::Rgb | png::ColorType::Rgba => {
            let step = if info.color_type == png::ColorType::Rgb { 3 } else { 4 };
            (0..w * h).map(|i| buf[i * step]).collect()
        }
        _ => return None,
    };
    Some((data, w, h))
}

fn b64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((chunk[0] as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'=' => break,
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_png_round_trips_exactly() {
        let (w, h) = (37usize, 23usize);
        let data: Vec<u8> = (0..w * h).map(|i| (i * 7 % 256) as u8).collect();
        let encoded = encode_gray(&data, w, h).expect("encode");
        assert!(encoded.chars().all(|c| c.is_ascii() && !c.is_whitespace()));
        let (got, gw, gh) = decode_gray(&encoded).expect("decode");
        assert_eq!((gw, gh), (w, h));
        assert_eq!(got, data);
    }

    #[test]
    fn mask_png_rejects_bad_dims_and_garbage() {
        assert!(encode_gray(&[0; 10], 4, 4).is_none());
        assert!(decode_gray("not base64!!").is_none());
        assert!(decode_gray("AAAA").is_none());
        assert!(b64_decode("AB=C").is_some());
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }
}
