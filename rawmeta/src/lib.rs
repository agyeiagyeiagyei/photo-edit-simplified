//! RAW metadata helpers shared by the app and the pixel worker, with no
//! decoder dependency: RAW name detection, the embedded-JPEG TIFF walk,
//! EXIF orientation, capture-time parsing, and a small TIFF writer. The app
//! links this crate alone; rawler (the demosaicer) ships only in the worker.

pub const RAW_EXTENSIONS: [&str; 11] = [
    "dng", "cr2", "cr3", "nef", "nrw", "arw", "orf", "rw2", "raf", "pef", "srw",
];

pub fn is_raw_name(name: &str) -> bool {
    let n = name.to_lowercase();
    RAW_EXTENSIONS.iter().any(|ext| n.ends_with(&format!(".{ext}")))
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

// ---------- orientation ----------

/// EXIF orientation (1–8) as rawler's (transpose, flip_x, flip_y) tuple. Per
/// rawler's docs the flips must be applied before the transpose. Values
/// outside 1–8 (including rawler's `Unknown` → 0) mean "leave as-is".
pub fn orientation_flips(code: u16) -> (bool, bool, bool) {
    match code {
        2 => (false, true, false),
        3 => (false, true, true),
        4 => (false, false, true),
        5 => (true, false, false),
        6 => (true, false, true),
        7 => (true, true, true),
        8 => (true, true, false),
        _ => (false, false, false),
    }
}

pub fn oriented_dims(w: usize, h: usize, code: u16) -> (usize, usize) {
    if orientation_flips(code).0 {
        (h, w)
    } else {
        (w, h)
    }
}

/// Apply an EXIF orientation code to RGBA pixels.
pub fn apply_orientation_code(
    rgba: Vec<u8>,
    w: usize,
    h: usize,
    code: u16,
) -> (Vec<u8>, usize, usize) {
    let (do_transpose, flip_x, flip_y) = orientation_flips(code);
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

// ---------- capture-time reader ----------

/// Capture time from a TIFF payload, as epoch seconds. EXIF datetimes carry
/// no zone, so the string is read as-if-UTC — only relative ordering matters.
/// ExifIFD DateTimeOriginal (0x9003) → CreateDate (0x9004) → IFD0 DateTime
/// (0x0132).
pub fn taken_from_tiff(t: &[u8]) -> Option<f64> {
    if t.len() < 8 {
        return None;
    }
    let le = match &t[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16at = |o: usize| -> Option<u16> {
        let s = t.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            u16::from_be_bytes([s[0], s[1]])
        })
    };
    let u32at = |o: usize| -> Option<u32> {
        let s = t.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes([s[0], s[1], s[2], s[3]])
        } else {
            u32::from_be_bytes([s[0], s[1], s[2], s[3]])
        })
    };
    // ASCII value of `tag` in the IFD at offset `ifd`.
    let ascii_at = |ifd: usize, tag: u16| -> Option<String> {
        let n = u16at(ifd)? as usize;
        for k in 0..n {
            let e = ifd + 2 + 12 * k;
            if u16at(e)? != tag {
                continue;
            }
            let count = u32at(e + 4)? as usize;
            let data = if count <= 4 {
                t.get(e + 8..e + 8 + count)?
            } else {
                let off = u32at(e + 8)? as usize;
                t.get(off..off + count)?
            };
            let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
            return std::str::from_utf8(&data[..end]).ok().map(|s| s.trim().to_string());
        }
        None
    };
    let ifd0 = u32at(4)? as usize;
    let mut exif_ifd = None;
    if let Some(n) = u16at(ifd0) {
        for k in 0..n as usize {
            let e = ifd0 + 2 + 12 * k;
            if u16at(e) == Some(0x8769) {
                exif_ifd = u32at(e + 8).map(|v| v as usize);
            }
        }
    }
    if let Some(x) = exif_ifd {
        for tag in [0x9003u16, 0x9004] {
            if let Some(v) = ascii_at(x, tag).and_then(|s| parse_exif_datetime(&s)) {
                return Some(v);
            }
        }
    }
    ascii_at(ifd0, 0x0132).and_then(|s| parse_exif_datetime(&s))
}

/// "YYYY:MM:DD HH:MM:SS" → epoch seconds (as-if-UTC), via days-from-civil.
fn parse_exif_datetime(s: &str) -> Option<f64> {
    if s.len() < 19 {
        return None;
    }
    let num = |i: usize, n: usize| s.get(i..i + n)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, se) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60 + se) as f64)
}

// ---------- TIFF writer ----------

pub mod tiff {
    //! Minimal little-endian TIFF writer: enough for EXIF carry-through and
    //! for tests that need to fabricate payloads.

    pub const ASCII: u16 = 2;
    pub const SHORT: u16 = 3;
    pub const LONG: u16 = 4;
    pub const RATIONAL: u16 = 5;
    pub const BYTE: u16 = 1;
    pub const SRATIONAL: u16 = 10;

    #[derive(Clone, Copy)]
    pub struct Rational {
        pub n: u32,
        pub d: u32,
    }

    #[derive(Clone, Copy)]
    pub struct SRational {
        pub n: i32,
        pub d: i32,
    }

    pub struct Entry {
        pub tag: u16,
        pub typ: u16,
        pub count: u32,
        pub data: Vec<u8>,
    }

    pub fn e_ascii(tag: u16, s: &str) -> Entry {
        let mut data = s.trim().as_bytes().to_vec();
        data.push(0);
        let count = data.len() as u32;
        Entry { tag, typ: ASCII, count, data }
    }

    pub fn e_short(tag: u16, v: u16) -> Entry {
        Entry { tag, typ: SHORT, count: 1, data: v.to_le_bytes().to_vec() }
    }

    pub fn e_long(tag: u16, v: u32) -> Entry {
        Entry { tag, typ: LONG, count: 1, data: v.to_le_bytes().to_vec() }
    }

    pub fn e_byte(tag: u16, v: u8) -> Entry {
        Entry { tag, typ: BYTE, count: 1, data: vec![v] }
    }

    pub fn e_rat(tag: u16, r: Rational) -> Entry {
        let mut data = r.n.to_le_bytes().to_vec();
        data.extend_from_slice(&r.d.max(1).to_le_bytes());
        Entry { tag, typ: RATIONAL, count: 1, data }
    }

    pub fn e_rats(tag: u16, rs: &[Rational]) -> Entry {
        let mut data = Vec::with_capacity(rs.len() * 8);
        for r in rs {
            data.extend_from_slice(&r.n.to_le_bytes());
            data.extend_from_slice(&r.d.max(1).to_le_bytes());
        }
        Entry { tag, typ: RATIONAL, count: rs.len() as u32, data }
    }

    pub fn e_srat(tag: u16, r: SRational) -> Entry {
        let mut data = r.n.to_le_bytes().to_vec();
        data.extend_from_slice(&r.d.max(1).to_le_bytes());
        Entry { tag, typ: SRATIONAL, count: 1, data }
    }

    /// Serialize one IFD (little-endian) at the current end of `out`.
    pub fn write_ifd(out: &mut Vec<u8>, mut entries: Vec<Entry>) {
        entries.sort_by_key(|e| e.tag);
        let n = entries.len();
        out.extend_from_slice(&(n as u16).to_le_bytes());
        let data_base = out.len() + 12 * n + 4;
        let mut data_area: Vec<u8> = Vec::new();
        for e in &entries {
            out.extend_from_slice(&e.tag.to_le_bytes());
            out.extend_from_slice(&e.typ.to_le_bytes());
            out.extend_from_slice(&e.count.to_le_bytes());
            if e.data.len() <= 4 {
                out.extend_from_slice(&e.data);
                out.resize(out.len() + (4 - e.data.len()), 0);
            } else {
                let offset = (data_base + data_area.len()) as u32;
                out.extend_from_slice(&offset.to_le_bytes());
                data_area.extend_from_slice(&e.data);
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes()); // next IFD: none
        out.extend_from_slice(&data_area);
    }

    fn patch_u32(out: &mut Vec<u8>, pos: usize, v: u32) {
        out[pos..pos + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn set_len_as_offset(out: &mut Vec<u8>, pos: usize) {
        let v = out.len() as u32;
        patch_u32(out, pos, v);
    }

    /// Assemble a little-endian TIFF with IFD0 plus optional Exif/GPS sub-IFDs.
    pub fn build_tiff(mut ifd0: Vec<Entry>, exif: Vec<Entry>, gps: Vec<Entry>) -> Vec<u8> {
        if !exif.is_empty() {
            ifd0.push(Entry { tag: 0x8769, typ: LONG, count: 1, data: vec![0; 4] });
        }
        if !gps.is_empty() {
            ifd0.push(Entry { tag: 0x8825, typ: LONG, count: 1, data: vec![0; 4] });
        }
        ifd0.sort_by_key(|e| e.tag);
        let pos_of = |tag: u16| 8 + 2 + 12 * ifd0.iter().position(|e| e.tag == tag).unwrap() + 8;
        let exif_patch = (!exif.is_empty()).then(|| pos_of(0x8769));
        let gps_patch = (!gps.is_empty()).then(|| pos_of(0x8825));

        let mut out = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        write_ifd(&mut out, ifd0);
        if let Some(pos) = exif_patch {
            set_len_as_offset(&mut out, pos);
            write_ifd(&mut out, exif);
        }
        if let Some(pos) = gps_patch {
            set_len_as_offset(&mut out, pos);
            write_ifd(&mut out, gps);
        }
        out
    }
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
        // EXIF 6 (Rotate90) = transpose + flip_y. 2x1 [A B] -> flip_v (no-op,
        // h=1) -> transpose -> 1x2 [A; B].
        let src = [px(9, 0, 0), px(5, 0, 0)].concat();
        let (out, w, h) = apply_orientation_code(src, 2, 1, 6);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[0..4], &px(9, 0, 0));
        assert_eq!(&out[4..8], &px(5, 0, 0));
        assert_eq!(oriented_dims(2, 1, 6), (1, 2));
        assert_eq!(oriented_dims(2, 1, 1), (2, 1));
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

    use crate::tiff::{build_tiff, e_ascii, e_rat, e_short, Rational};

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
    fn tiff_writer_roundtrip() {
        let tiff = build_tiff(
            vec![e_ascii(0x010F, "Canon"), e_short(0x0112, 1)],
            vec![e_rat(0x829A, Rational { n: 1, d: 125 })],
            vec![],
        );
        assert_eq!(&tiff[0..4], &[b'I', b'I', 42, 0]);
        assert!(tiff.windows(5).any(|w| w == b"Canon"));
    }
}
