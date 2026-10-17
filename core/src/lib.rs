//! The Windedit engine: decoding, color science and the GPU develop pipeline. Platform apps
//! talk to it through the C interface in `ffi`; Rust tools (like the benchmark) use it directly.

pub mod color;
pub mod decode;
pub mod gpu;
pub mod pipeline;
pub mod present;
pub mod ffi;
