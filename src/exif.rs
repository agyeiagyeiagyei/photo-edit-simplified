//! EXIF carry-through. At import we keep a TIFF payload per photo: copied
//! verbatim from JPEG APP1 / PNG eXIf, or built from RAW metadata via rawler.
//! At export we inject it into the output (JPEG APP1 or PNG eXIf chunk).
//! Orientation is normalized to 1 because the pipeline bakes rotation into
//! pixels, and JPEG thumbnails are detached (their offsets would be stale).

use rawler::decoders::RawMetadata;
use rawler::formats::tiff::{Rational, SRational};

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

// ---------- TIFF writer ----------

const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;
const BYTE: u16 = 1;
const SRATIONAL: u16 = 10;

struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    data: Vec<u8>,
}

fn e_ascii(tag: u16, s: &str) -> Entry {
    let mut data = s.trim().as_bytes().to_vec();
    data.push(0);
    let count = data.len() as u32;
    Entry { tag, typ: ASCII, count, data }
}

fn e_short(tag: u16, v: u16) -> Entry {
    Entry { tag, typ: SHORT, count: 1, data: v.to_le_bytes().to_vec() }
}

fn e_long(tag: u16, v: u32) -> Entry {
    Entry { tag, typ: LONG, count: 1, data: v.to_le_bytes().to_vec() }
}

fn e_byte(tag: u16, v: u8) -> Entry {
    Entry { tag, typ: BYTE, count: 1, data: vec![v] }
}

fn e_rat(tag: u16, r: Rational) -> Entry {
    let mut data = r.n.to_le_bytes().to_vec();
    data.extend_from_slice(&r.d.max(1).to_le_bytes());
    Entry { tag, typ: RATIONAL, count: 1, data }
}

fn e_rats(tag: u16, rs: &[Rational]) -> Entry {
    let mut data = Vec::with_capacity(rs.len() * 8);
    for r in rs {
        data.extend_from_slice(&r.n.to_le_bytes());
        data.extend_from_slice(&r.d.max(1).to_le_bytes());
    }
    Entry { tag, typ: RATIONAL, count: rs.len() as u32, data }
}

fn e_srat(tag: u16, r: SRational) -> Entry {
    let mut data = r.n.to_le_bytes().to_vec();
    data.extend_from_slice(&r.d.max(1).to_le_bytes());
    Entry { tag, typ: SRATIONAL, count: 1, data }
}

/// Serialize one IFD (little-endian) at the current end of `out`.
fn write_ifd(out: &mut Vec<u8>, mut entries: Vec<Entry>) {
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
fn build_tiff(mut ifd0: Vec<Entry>, exif: Vec<Entry>, gps: Vec<Entry>) -> Vec<u8> {
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

/// Build a TIFF payload from a RAW file's metadata.
pub fn tiff_from_raw_metadata(meta: &RawMetadata) -> Vec<u8> {
    let x = &meta.exif;
    let mut ifd0 = vec![
        e_ascii(0x010F, &meta.make),
        e_ascii(0x0110, &meta.model),
        e_short(0x0112, 1),
        e_ascii(0x0131, "photo-edit-simplified"),
    ];
    if let Some(s) = &x.modify_date {
        ifd0.push(e_ascii(0x0132, s));
    }
    if let Some(s) = &x.artist {
        ifd0.push(e_ascii(0x013B, s));
    }
    if let Some(s) = &x.copyright {
        ifd0.push(e_ascii(0x8298, s));
    }

    let mut ex: Vec<Entry> = Vec::new();
    if let Some(r) = x.exposure_time {
        ex.push(e_rat(0x829A, r));
    }
    if let Some(r) = x.fnumber {
        ex.push(e_rat(0x829D, r));
    }
    if let Some(v) = x.exposure_program {
        ex.push(e_short(0x8822, v));
    }
    let iso = x.iso_speed_ratings.map(|v| v as u32).or(x.iso_speed);
    if let Some(v) = iso {
        ex.push(e_short(0x8827, v.min(u16::MAX as u32) as u16));
    }
    if let Some(v) = x.sensitivity_type {
        ex.push(e_short(0x8830, v));
    }
    if let Some(v) = x.recommended_exposure_index {
        ex.push(e_long(0x8832, v));
    }
    if let Some(s) = &x.date_time_original {
        ex.push(e_ascii(0x9003, s));
    }
    if let Some(s) = &x.create_date {
        ex.push(e_ascii(0x9004, s));
    }
    if let Some(s) = &x.offset_time {
        ex.push(e_ascii(0x9010, s));
    }
    if let Some(s) = &x.offset_time_original {
        ex.push(e_ascii(0x9011, s));
    }
    if let Some(r) = x.shutter_speed_value {
        ex.push(e_srat(0x9201, r));
    }
    if let Some(r) = x.aperture_value {
        ex.push(e_rat(0x9202, r));
    }
    if let Some(r) = x.brightness_value {
        ex.push(e_srat(0x9203, r));
    }
    if let Some(r) = x.exposure_bias {
        ex.push(e_srat(0x9204, r));
    }
    if let Some(r) = x.max_aperture_value {
        ex.push(e_rat(0x9205, r));
    }
    if let Some(r) = x.subject_distance {
        ex.push(e_rat(0x9206, r));
    }
    if let Some(v) = x.metering_mode {
        ex.push(e_short(0x9207, v));
    }
    if let Some(v) = x.light_source {
        ex.push(e_short(0x9208, v));
    }
    if let Some(v) = x.flash {
        ex.push(e_short(0x9209, v));
    }
    if let Some(r) = x.focal_length {
        ex.push(e_rat(0x920A, r));
    }
    if let Some(r) = x.flash_energy {
        ex.push(e_rat(0xA20B, r));
    }
    // Exports are always sRGB.
    ex.push(e_short(0xA001, x.color_space.unwrap_or(1)));
    if let Some(v) = x.exposure_mode {
        ex.push(e_short(0xA402, v));
    }
    if let Some(v) = x.white_balance {
        ex.push(e_short(0xA403, v));
    }
    if let Some(v) = x.scene_capture_type {
        ex.push(e_short(0xA406, v));
    }
    if let Some(v) = x.subject_distance_range {
        ex.push(e_short(0xA40C, v));
    }
    if let Some(s) = &x.owner_name {
        ex.push(e_ascii(0xA430, s));
    }
    if let Some(s) = &x.serial_number {
        ex.push(e_ascii(0xA431, s));
    }
    if let Some(rs) = &x.lens_spec {
        ex.push(e_rats(0xA432, rs));
    }
    if let Some(s) = &x.lens_make {
        ex.push(e_ascii(0xA433, s));
    }
    if let Some(s) = &x.lens_model {
        ex.push(e_ascii(0xA434, s));
    }
    if let Some(s) = &x.lens_serial_number {
        ex.push(e_ascii(0xA435, s));
    }

    let mut gps_entries: Vec<Entry> = Vec::new();
    if let Some(g) = &x.gps {
        if let Some(v) = g.gps_version_id {
            gps_entries.push(Entry { tag: 0, typ: BYTE, count: 4, data: v.to_vec() });
        }
        if let Some(s) = &g.gps_latitude_ref {
            gps_entries.push(e_ascii(1, s));
        }
        if let Some(rs) = &g.gps_latitude {
            gps_entries.push(e_rats(2, rs));
        }
        if let Some(s) = &g.gps_longitude_ref {
            gps_entries.push(e_ascii(3, s));
        }
        if let Some(rs) = &g.gps_longitude {
            gps_entries.push(e_rats(4, rs));
        }
        if let Some(v) = g.gps_altitude_ref {
            gps_entries.push(e_byte(5, v));
        }
        if let Some(r) = g.gps_altitude {
            gps_entries.push(e_rat(6, r));
        }
        if let Some(rs) = &g.gps_timestamp {
            gps_entries.push(e_rats(7, rs));
        }
        if let Some(s) = &g.gps_map_datum {
            gps_entries.push(e_ascii(18, s));
        }
        if let Some(s) = &g.gps_date_stamp {
            gps_entries.push(e_ascii(29, s));
        }
    }

    build_tiff(ifd0, ex, gps_entries)
}

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
