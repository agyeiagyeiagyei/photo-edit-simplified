//! Pure pixel operations and the data/wire types they need, shared by the
//! photo-edit-simplified app and its pixel worker.

// Buffer kernels index fixed-width samples directly; the `as_chunks` rewrite
// this lint wants is newer API and its autofix for the mutable case does not
// compile. Kernel signatures stay flat because they are hot paths.
#![allow(clippy::chunks_exact_to_as_chunks, clippy::too_many_arguments)]

pub mod blur;
pub mod clone;
pub mod color;
pub mod curves;
pub mod denoise;
pub mod fill;
pub mod geo;
pub mod heal;
pub mod levels;
pub mod noise;
pub mod ops;
pub mod pipeline;
pub mod rawops;
pub mod recipe;
pub mod resample;
pub mod stroke;
pub mod types;
pub mod wand;
pub mod wide;
