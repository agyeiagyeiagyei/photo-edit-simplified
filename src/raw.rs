//! RAW name/orientation helpers shared with the pixel worker. The decoder
//! itself ships only in the worker; this crate links `rawmeta`, the
//! dependency-free half of `rawcore`.

pub use rawmeta::{apply_orientation_code, is_raw_name};
