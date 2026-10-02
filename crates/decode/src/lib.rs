//! H.264 and HEVC decoding on the GPU through FFmpeg's d3d11va.
//!
//! FFmpeg's DLLs are loaded at the first [`Decoder::new`], from the folder
//! of the running exe only, so booth.exe starts and hosts without them. A
//! small C file compiled against FFmpeg's headers reads and writes the few
//! struct fields needed; no layout is copied into Rust. Every HEVC SPS a
//! friend's PC sends is read in Rust before FFmpeg sees it. [`probe`] asks
//! Direct3D alone whether the GPU decodes a codec at a size.

#![deny(unsafe_op_in_unsafe_fn)]

mod decoder;
mod error;
mod ffi;
mod guard;
mod library;
mod probe;
mod timing;

pub use decoder::{Codec, Decoded, Decoder, MAX_ACCESS_UNIT};
pub use error::DecodeError;
pub use probe::probe;
pub use timing::GpuTime;
