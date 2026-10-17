//! The C interface the platform apps call. Everything goes through one opaque `Engine`; calls
//! return 0 on success and a nonzero code on failure, with the message from `we_last_error`.
//! Strings in are UTF-16 (what Windows and .NET use), NUL-terminated.
//!
//! Every pointer argument must be valid (or null where noted) for the duration of the call; the
//! callers are our own platform apps, so the functions stay safe-to-call from Rust's view.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use windows::core::Interface;

use crate::decode;
use crate::gpu::Gpu;
use crate::pipeline::{Edit, Image, Zoom};
use crate::present::{FrameTimings, Presenter};

pub struct Engine {
    gpu: Gpu,
    image: Option<Image>,
    presenter: Option<Presenter>,
    edit: Edit,
    zoom: Zoom,
    last_error: String,
}

#[repr(C)]
#[derive(Default)]
pub struct OpenInfo {
    pub width: u32,
    pub height: u32,
    pub decode_ms: f64,
    pub upload_ms: f64,
    pub copy_ms: f64,
    pub demosaic_ms: f64,
    pub pyramid_ms: f64,
    /// From the start of decoding until the photo can be developed.
    pub total_ms: f64,
}

/// Runs `f`, turning errors and panics into a return code and `last_error`.
fn guard(engine: *mut Engine, f: impl FnOnce(&mut Engine) -> Result<(), String>) -> i32 {
    let Some(engine) = (unsafe { engine.as_mut() }) else { return -1 };
    match catch_unwind(AssertUnwindSafe(|| f(engine))) {
        Ok(Ok(())) => 0,
        Ok(Err(message)) => {
            engine.last_error = message;
            1
        }
        Err(_) => {
            engine.last_error = "internal error (panic)".into();
            2
        }
    }
}

fn wide_to_path(s: *const u16) -> PathBuf {
    let mut len = 0;
    unsafe {
        while *s.add(len) != 0 {
            len += 1;
        }
        PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(s, len)))
    }
}

/// Copies `text` into `buf` (UTF-16, NUL-terminated, truncated to fit). Returns its full length.
fn write_wide(text: &str, buf: *mut u16, len: u32) -> u32 {
    let wide: Vec<u16> = text.encode_utf16().collect();
    if !buf.is_null() && len > 0 {
        let n = wide.len().min(len as usize - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), buf, n);
            *buf.add(n) = 0;
        }
    }
    wide.len() as u32
}

/// Creates the engine on the fastest GPU. Returns null if there is no usable Direct3D 12 GPU.
#[unsafe(no_mangle)]
pub extern "C" fn we_engine_create() -> *mut Engine {
    match catch_unwind(Gpu::new) {
        Ok(Ok(gpu)) => Box::into_raw(Box::new(Engine {
            gpu,
            image: None,
            presenter: None,
            edit: Edit::default(),
            zoom: Zoom::Fit,
            last_error: String::new(),
        })),
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn we_engine_destroy(engine: *mut Engine) {
    if !engine.is_null() {
        drop(unsafe { Box::from_raw(engine) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn we_last_error(engine: *const Engine, buf: *mut u16, len: u32) -> u32 {
    match unsafe { engine.as_ref() } {
        Some(engine) => write_wide(&engine.last_error, buf, len),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn we_adapter_name(engine: *const Engine, buf: *mut u16, len: u32) -> u32 {
    match unsafe { engine.as_ref() } {
        Some(engine) => {
            let kind = if engine.gpu.uma { "shared memory" } else { "own memory" };
            write_wide(&format!("{} ({kind})", engine.gpu.adapter_name), buf, len)
        }
        None => 0,
    }
}

/// Decodes a raw file and gets it ready to develop. Resets the zoom to fit.
#[unsafe(no_mangle)]
pub extern "C" fn we_open(engine: *mut Engine, path: *const u16, info: *mut OpenInfo) -> i32 {
    guard(engine, |e| {
        let frame = decode::decode_file(&wide_to_path(path)).map_err(|err| err.to_string())?;
        e.image = None; // free the previous photo's GPU memory first
        let (image, t) = Image::open(&mut e.gpu, &frame).map_err(|err| err.to_string())?;
        let (width, height) = image.oriented_size();
        if let Some(info) = unsafe { info.as_mut() } {
            *info = OpenInfo {
                width,
                height,
                decode_ms: frame.decode_time.as_secs_f64() * 1000.0,
                upload_ms: t.upload_ms,
                copy_ms: t.copy_ms,
                demosaic_ms: t.demosaic_ms,
                pyramid_ms: t.pyramid_ms,
                total_ms: frame.decode_time.as_secs_f64() * 1000.0 + t.total_ms,
            };
        }
        e.image = Some(image);
        e.zoom = Zoom::Fit;
        Ok(())
    })
}

/// Creates (or resizes) the swap chain the view shows. Size in physical pixels; `scale` is the
/// view's DPI scale. Writes the IDXGISwapChain pointer (not AddRef'd; the engine owns it) for
/// the app to attach, e.g. with ISwapChainPanelNative::SetSwapChain.
#[unsafe(no_mangle)]
pub extern "C" fn we_view_resize(engine: *mut Engine, width: u32, height: u32, scale: f32, chain: *mut *mut c_void) -> i32 {
    guard(engine, |e| {
        match &mut e.presenter {
            Some(p) => p.resize(&mut e.gpu, width, height, scale).map_err(|err| err.to_string())?,
            None => e.presenter = Some(Presenter::new(&e.gpu, width, height, scale).map_err(|err| err.to_string())?),
        }
        if !chain.is_null() {
            unsafe { *chain = e.presenter.as_ref().unwrap().chain.as_raw() };
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn we_set_exposure(engine: *mut Engine, stops: f32) -> i32 {
    guard(engine, |e| {
        e.edit.exposure = stops;
        Ok(())
    })
}

/// Fit the whole photo (`actual` = 0), or show it at 100% centered on (`x`, `y`), given as
/// fractions of the rotated photo's width and height.
#[unsafe(no_mangle)]
pub extern "C" fn we_set_zoom(engine: *mut Engine, actual: i32, x: f32, y: f32) -> i32 {
    guard(engine, |e| {
        e.zoom = match (&e.image, actual) {
            (Some(image), 1) => {
                let (w, h) = image.oriented_size();
                Zoom::Actual { x: x.clamp(0.0, 1.0) * w as f32, y: y.clamp(0.0, 1.0) * h as f32 }
            }
            _ => Zoom::Fit,
        };
        Ok(())
    })
}

/// Develops the photo with the current edits and shows it. Does nothing without a photo or view.
#[unsafe(no_mangle)]
pub extern "C" fn we_render(engine: *mut Engine, timings: *mut FrameTimings) -> i32 {
    guard(engine, |e| {
        let (Some(image), Some(presenter)) = (&e.image, &mut e.presenter) else { return Ok(()) };
        let t = presenter.present(&mut e.gpu, image, &e.edit, e.zoom).map_err(|err| err.to_string())?;
        if let Some(out) = unsafe { timings.as_mut() } {
            *out = t;
        }
        Ok(())
    })
}
