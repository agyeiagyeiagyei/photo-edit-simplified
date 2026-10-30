//! EXIF carry-through. At import we keep a TIFF payload per photo, copied
//! verbatim from JPEG APP1 / PNG eXIf (RAW payloads are built in the pixel
//! worker, which owns the decoder).
//! At export we inject it into the output (JPEG APP1 or PNG eXIf chunk).
//! Orientation is normalized to 1 because the pipeline bakes rotation into
//! pixels, and JPEG thumbnails are detached (their offsets would be stale).

// ---------- extraction ----------

/// Pull the EXIF TIFF payload out of a JPEG's APP1 segment.
pub fn tiff_from_jpeg(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            break;
        }
        let marker = b[i + 1];
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if len < 2 || i + 2 + len > b.len() {
            break;
        }
        if marker == 0xE1 && len >= 8 && &b[i + 4..i + 10] == b"Exif\0\0" {
            let mut tiff = b[i + 10..i + 2 + len].to_vec();
            normalize_orientation(&mut tiff);
            return Some(tiff);
        }
        i += 2 + len;
    }
    None
}

/// Pull the TIFF payload out of a PNG's eXIf chunk.
pub fn tiff_from_png(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 8 || &b[1..4] != b"PNG" {
        return None;
    }
    let mut i = 8;
    while i + 12 <= b.len() {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        if i + 12 + len > b.len() {
            break;
        }
        if &b[i + 4..i + 8] == b"eXIf" {
            let mut tiff = b[i + 8..i + 8 + len].to_vec();
            normalize_orientation(&mut tiff);
            return Some(tiff);
        }
        i += 12 + len;
    }
    None
}

/// In-place: set Orientation to 1 and detach the thumbnail IFD (IFD1).
fn normalize_orientation(t: &mut Vec<u8>) {
    if t.len() < 8 {
        return;
    }
    let le = match &t[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return,
    };
    fn u16at(t: &[u8], le: bool, o: usize) -> Option<u16> {
        let s = t.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            u16::from_be_bytes([s[0], s[1]])
        })
    }
    fn u32at(t: &[u8], le: bool, o: usize) -> Option<u32> {
        let s = t.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes([s[0], s[1], s[2], s[3]])
        } else {
            u32::from_be_bytes([s[0], s[1], s[2], s[3]])
        })
    }
    let Some(ifd) = u32at(t, le, 4).map(|v| v as usize) else { return };
    let Some(n) = u16at(t, le, ifd).map(|v| v as usize) else { return };
    for k in 0..n {
        let e = ifd + 2 + 12 * k;
        if u16at(t, le, e) == Some(0x0112) && u16at(t, le, e + 2) == Some(3) && e + 10 <= t.len() {
            // SHORT value sits in the first two bytes of the value field.
            if le {
                t[e + 8] = 1;
                t[e + 9] = 0;
            } else {
                t[e + 8] = 0;
                t[e + 9] = 1;
            }
        }
    }
    let next = ifd + 2 + 12 * n;
    if next + 4 <= t.len() {
        for b in &mut t[next..next + 4] {
            *b = 0;
        }
    }
}

// ---------- capture-time reader ----------

pub use rawmeta::taken_from_tiff;

// ---------- injection ----------

/// Insert an APP1 EXIF segment right after the JPEG SOI marker.
pub fn inject_jpeg(jpeg: &[u8], tiff: &[u8]) -> Vec<u8> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return jpeg.to_vec();
    }
    let seg_len = 2 + 6 + tiff.len(); // length field counts itself
    if seg_len > 0xFFFF {
        crate::web::log("EXIF too large for APP1; exporting without metadata");
        return jpeg.to_vec();
    }
    let mut out = Vec::with_capacity(jpeg.len() + 2 + seg_len);
    out.extend_from_slice(&jpeg[..2]);
    out.extend_from_slice(&[0xFF, 0xE1, (seg_len >> 8) as u8, seg_len as u8]);
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(tiff);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// Insert an eXIf chunk right after IHDR.
pub fn inject_png(png: &[u8], tiff: &[u8]) -> Vec<u8> {
    if png.len() < 8 || &png[1..4] != b"PNG" {
        return png.to_vec();
    }
    let ihdr_len = u32::from_be_bytes([png[8], png[9], png[10], png[11]]) as usize;
    let insert_at = 8 + 12 + ihdr_len;
    if insert_at > png.len() {
        return png.to_vec();
    }
    let mut payload = b"eXIf".to_vec();
    payload.extend_from_slice(tiff);
    let crc = crc32(&payload);
    let mut out = Vec::with_capacity(png.len() + 12 + tiff.len());
    out.extend_from_slice(&png[..insert_at]);
    out.extend_from_slice(&(tiff.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&crc.to_be_bytes());
    out.extend_from_slice(&png[insert_at..]);
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    let mut c = 0xFFFFFFFFu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFFFFFF
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawmeta::tiff::{build_tiff, e_ascii, e_rat, e_short, Rational};

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn tiff_roundtrip_orientation() {
        let tiff = build_tiff(
            vec![e_ascii(0x010F, "Canon"), e_short(0x0112, 1)],
            vec![e_rat(0x829A, Rational { n: 1, d: 125 })],
            vec![],
        );
        assert_eq!(&tiff[0..4], &[b'I', b'I', 42, 0]);
        let ifd = u32::from_le_bytes(tiff[4..8].try_into().unwrap()) as usize;
        let n = u16::from_le_bytes(tiff[ifd..ifd + 2].try_into().unwrap()) as usize;
        assert_eq!(n, 3); // make + orientation + exif-offset placeholder
        // find the orientation entry
        let mut found = false;
        for k in 0..n {
            let e = ifd + 2 + 12 * k;
            let tag = u16::from_le_bytes(tiff[e..e + 2].try_into().unwrap());
            if tag == 0x0112 {
                assert_eq!(tiff[e + 8], 1);
                found = true;
            }
        }
        assert!(found);
        // the make string is reachable via its offset
        assert!(tiff.windows(5).any(|w| w == b"Canon"));
    }

    #[test]
    fn jpeg_inject_and_extract() {
        let tiff = build_tiff(vec![e_ascii(0x0110, "X-T5")], vec![], vec![]);
        let jpeg = [0xFF, 0xD8, 0xFF, 0xD9];
        let out = inject_jpeg(&jpeg, &tiff);
        assert_eq!(&out[2..4], &[0xFF, 0xE1]);
        let back = tiff_from_jpeg(&out).expect("APP1 readable");
        assert_eq!(back, tiff);
    }

    #[test]
    fn jpeg_orientation_normalized_on_extract() {
        // minimal LE TIFF with orientation = 6
        let mut tiff = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&6u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        let seg = 2 + 6 + tiff.len();
        jpeg.extend_from_slice(&(seg as u16).to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        let got = tiff_from_jpeg(&jpeg).unwrap();
        // orientation value (entry value field at 8+2+8) is now 1
        assert_eq!(got[18], 1);
    }

    #[test]
    fn taken_reads_datetime_original() {
        let tiff = build_tiff(
            vec![e_ascii(0x010F, "Canon")],
            vec![e_ascii(0x9003, "2020:03:15 10:30:00")],
            vec![],
        );
        assert_eq!(taken_from_tiff(&tiff), Some(1584268200.0));
    }

    #[test]
    fn taken_prefers_original_over_create_and_ifd0() {
        let tiff = build_tiff(
            vec![e_ascii(0x0132, "1999:12:31 23:59:59")],
            vec![
                e_ascii(0x9003, "2020:03:15 10:30:00"),
                e_ascii(0x9004, "2021:01:01 00:00:00"),
            ],
            vec![],
        );
        assert_eq!(taken_from_tiff(&tiff), Some(1584268200.0));
    }

    #[test]
    fn taken_falls_back_to_ifd0_datetime() {
        let tiff = build_tiff(vec![e_ascii(0x0132, "1999:12:31 23:59:59")], vec![], vec![]);
        assert_eq!(taken_from_tiff(&tiff), Some(946684799.0));
    }

    #[test]
    fn taken_handles_big_endian() {
        // minimal MM TIFF, IFD0 with DateTime "2023:06:01 12:00:00" (20 bytes)
        let mut t = vec![b'M', b'M', 0, 42, 0, 0, 0, 8];
        t.extend_from_slice(&1u16.to_be_bytes());
        t.extend_from_slice(&0x0132u16.to_be_bytes());
        t.extend_from_slice(&2u16.to_be_bytes());
        t.extend_from_slice(&20u32.to_be_bytes());
        t.extend_from_slice(&26u32.to_be_bytes());
        t.extend_from_slice(&0u32.to_be_bytes());
        t.extend_from_slice(b"2023:06:01 12:00:00\0");
        assert_eq!(taken_from_tiff(&t), Some(1685620800.0));
    }

    #[test]
    fn png_inject_and_extract() {
        let tiff = build_tiff(vec![e_ascii(0x010F, "Sony")], vec![], vec![]);
        // minimal PNG: sig + IHDR(13 zero bytes) + IEND
        let mut png = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0; 13]);
        png.extend_from_slice(&[0; 4]);
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&[0; 4]);
        let out = inject_png(&png, &tiff);
        assert_eq!(&out[33 + 4..33 + 8], b"eXIf");
        let back = tiff_from_png(&out).expect("eXIf readable");
        assert_eq!(back, tiff);
    }
}
