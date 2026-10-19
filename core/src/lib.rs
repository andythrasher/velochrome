//! The Velochrome engine: decoding, color science and the GPU develop pipeline. Platform apps
//! talk to it through the C interface in `ffi`; Rust tools (like the benchmark) use it directly.

pub mod color;
pub mod decode;
pub mod gpu;
pub mod pipeline;
pub mod present;
pub mod ffi;
pub mod jpeg;
pub mod library;
pub mod preview;
pub mod jobs;
pub mod sys;
pub mod engine;
pub mod viewer;
pub mod xmp;
pub mod ratings;
pub mod develop;
pub mod tone;
pub mod lens;
pub mod exif;
pub mod export;
pub mod geometry;
pub mod pick;
pub mod detail;
pub mod mixer;
pub mod masks;
pub mod brushgpu;
pub mod import;
pub mod developed;
pub mod profile;

