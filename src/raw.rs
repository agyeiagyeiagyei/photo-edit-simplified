//! RAW helpers. Decoding lives in the shared `rawcore` crate so the pixel
//! worker can develop RAW files off the main thread; this module re-exports it
//! and adds the app-side EXIF bridge.

pub use rawcore::{
    decode_raw, is_raw_name, largest_embedded_jpeg, orientation_code, orientation_corrected,
    preview_rgba, RAW_EXTENSIONS,
};

use rawler::decoders::RawDecodeParams;
use rawler::rawsource::RawSource;

/// Read camera metadata (make, model, exposure, GPS…) into an EXIF TIFF
/// payload carried through to export.
pub fn extract_exif_tiff(bytes: &[u8]) -> Option<Vec<u8>> {
    let rawfile = RawSource::new_from_slice(bytes);
    let decoder = rawler::get_decoder(&rawfile).ok()?;
    let meta = decoder
        .raw_metadata(&rawfile, &RawDecodeParams { image_index: 0 })
        .ok()?;
    Some(crate::exif::tiff_from_raw_metadata(&meta))
}
