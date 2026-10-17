//! The C interface the platform apps call. Everything goes through one opaque `Handle`; calls
//! return 0 (or a count) on success and a negative code on failure, with the message from
//! `we_last_error`. Strings are UTF-16 (what Windows and .NET use), NUL-terminated.
//!
//! Calls return at once: decoding and drawing happen on the engine's own threads, and each
//! frame drawn is reported through the callback given to `we_engine_create`, *on the engine's
//! GPU thread*, so the app must hand it to its UI thread itself.
//!
//! Every pointer argument must be valid (or null where noted) for the duration of the call; the
//! callers are our own platform apps, so the functions stay safe-to-call from Rust's view.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::develop::Edit;
use crate::engine::Engine;
use crate::pipeline::Zoom;
use crate::ratings::{LABELS, label_code};
use crate::viewer::FrameEvent;
use crate::xmp::Pick;

pub struct Handle {
    engine: Engine,
    last_error: Mutex<String>,
}

pub type FrameCallback = extern "C" fn(user: *mut c_void, event: *const FrameEvent);

/// The app's context pointer, passed back to it untouched.
struct User(usize);

/// Runs `f`, turning errors and panics into a negative return value and `last_error`.
fn guard(handle: *const Handle, f: impl FnOnce(&Engine) -> Result<i32, String>) -> i32 {
    let Some(h) = (unsafe { handle.as_ref() }) else { return -1 };
    let (code, message) = match catch_unwind(AssertUnwindSafe(|| f(&h.engine))) {
        Ok(Ok(n)) => return n,
        Ok(Err(message)) => (-2, message),
        Err(_) => (-3, "internal error (panic)".to_string()),
    };
    *h.last_error.lock().unwrap() = message;
    code
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

/// Starts the engine on the fastest GPU. `callback` (may be null) gets every frame drawn, on
/// the engine's GPU thread, with `user` passed back. Returns null if there's no usable
/// Direct3D 12 GPU.
#[unsafe(no_mangle)]
pub extern "C" fn we_engine_create(callback: Option<FrameCallback>, user: *mut c_void) -> *mut Handle {
    let user = User(user as usize);
    let events = Box::new(move |event: &FrameEvent| {
        if let Some(callback) = callback {
            callback(user.0 as *mut c_void, event);
        }
    });
    match catch_unwind(AssertUnwindSafe(|| Engine::new(events))) {
        Ok(Ok(engine)) => Box::into_raw(Box::new(Handle { engine, last_error: Mutex::new(String::new()) })),
        _ => std::ptr::null_mut(),
    }
}

/// Stops the engine's threads and frees it. No callback runs after this returns.
#[unsafe(no_mangle)]
pub extern "C" fn we_engine_destroy(handle: *mut Handle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn we_last_error(handle: *const Handle, buf: *mut u16, len: u32) -> u32 {
    match unsafe { handle.as_ref() } {
        Some(h) => write_wide(&h.last_error.lock().unwrap(), buf, len),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn we_adapter_name(handle: *const Handle, buf: *mut u16, len: u32) -> u32 {
    match unsafe { handle.as_ref() } {
        Some(h) => {
            let info = &h.engine.info;
            let kind = if info.uma { "shared memory" } else { "own memory" };
            write_wide(&format!("{} ({kind})", info.adapter_name), buf, len)
        }
        None => 0,
    }
}

/// How long preparing the GPU kernels took when the engine started, in milliseconds.
#[unsafe(no_mangle)]
pub extern "C" fn we_warm_up_ms(handle: *const Handle) -> f64 {
    unsafe { handle.as_ref() }.map_or(0.0, |h| h.engine.info.warm_up_ms)
}

/// Opens a folder of photos; returns how many there are.
#[unsafe(no_mangle)]
pub extern "C" fn we_open_folder(handle: *const Handle, path: *const u16) -> i32 {
    guard(handle, |e| e.open_folder(&wide_to_path(path)).map(|n| n as i32).map_err(|err| err.to_string()))
}

/// The index of the photo with this file (its raw or its JPEG), or -1.
#[unsafe(no_mangle)]
pub extern "C" fn we_find_photo(handle: *const Handle, path: *const u16) -> i32 {
    guard(handle, |e| {
        let path = wide_to_path(path);
        let same = |p: &Option<PathBuf>| p.as_ref().is_some_and(|p| p.eq(&path));
        Ok(e.photos().iter().position(|p| same(&p.raw) || same(&p.jpeg)).map_or(-1, |i| i as i32))
    })
}

/// The photo's name (its files' shared name, without extension).
#[unsafe(no_mangle)]
pub extern "C" fn we_photo_name(handle: *const Handle, index: i32, buf: *mut u16, len: u32) -> u32 {
    let Some(h) = (unsafe { handle.as_ref() }) else { return 0 };
    let photos = h.engine.photos();
    match photos.get(index as usize) {
        Some(p) => write_wide(&p.name, buf, len),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn we_show(handle: *const Handle, index: i32) -> i32 {
    guard(handle, |e| {
        if index < 0 || index as usize >= e.photos().len() {
            return Err(format!("there's no photo {index}"));
        }
        e.show(index as usize);
        Ok(0)
    })
}

/// Fit the whole photo (`actual` = 0), or show it at 100% centered on (`x`, `y`), given as
/// fractions of the rotated photo's width and height.
#[unsafe(no_mangle)]
pub extern "C" fn we_set_zoom(handle: *const Handle, actual: i32, x: f32, y: f32) -> i32 {
    guard(handle, |e| {
        e.set_zoom(if actual != 0 { Zoom::Actual { x, y } } else { Zoom::Fit });
        Ok(0)
    })
}

/// A photo's adjustments (develop.rs `Edit`).
#[unsafe(no_mangle)]
pub extern "C" fn we_get_edit(handle: *const Handle, index: i32, edit: *mut Edit) -> i32 {
    guard(handle, |e| {
        let current = e.ratings().edit(index as usize).ok_or(format!("there's no photo {index}"))?;
        unsafe { *edit = current };
        Ok(0)
    })
}

/// Changes a photo's adjustments. Pass `commit` 0 while a slider is being dragged (the photo
/// shows it, nothing is saved) and 1 when it's let go (saved, and one step for undo).
#[unsafe(no_mangle)]
pub extern "C" fn we_set_edit(handle: *const Handle, index: i32, edit: *const Edit, commit: i32) -> i32 {
    guard(handle, |e| {
        let edit = unsafe { *edit };
        if e.set_edit(index as usize, edit, commit != 0) { Ok(0) } else { Err(format!("there's no photo {index}")) }
    })
}

/// Undoes (`redo` 0) or redoes (1) a photo's last change, writing its adjustments now into
/// `edit`. Returns 1 if something changed, 0 if there was nothing to undo or redo.
#[unsafe(no_mangle)]
pub extern "C" fn we_undo(handle: *const Handle, index: i32, redo: i32, edit: *mut Edit) -> i32 {
    guard(handle, |e| match e.undo(index as usize, redo != 0) {
        Some(now) => {
            unsafe { *edit = now };
            Ok(1)
        }
        None => Ok(0),
    })
}

/// The photo's white balance as shot (temperature in kelvin, and tint). Returns 1 once its raw
/// has been decoded, 0 before.
#[unsafe(no_mangle)]
pub extern "C" fn we_as_shot_white_balance(handle: *const Handle, index: i32, kelvin: *mut f32, tint: *mut f32) -> i32 {
    guard(handle, |e| match e.as_shot_white_balance(index as usize) {
        Some((k, t)) => {
            unsafe {
                *kelvin = k;
                *tint = t;
            }
            Ok(1)
        }
        None => Ok(0),
    })
}

/// Edit mode (1) shows the developed raw; Cull mode (0) shows the camera's JPEGs.
#[unsafe(no_mangle)]
pub extern "C" fn we_set_edit_mode(handle: *const Handle, on: i32) -> i32 {
    guard(handle, |e| {
        e.set_edit_mode(on != 0);
        Ok(0)
    })
}

/// Creates (or resizes) the swap chain the view shows. Size in physical pixels; `scale` is the
/// view's DPI scale. Writes the IDXGISwapChain pointer (not AddRef'd; the engine owns it) for
/// the app to attach, e.g. with ISwapChainPanelNative::SetSwapChain.
#[unsafe(no_mangle)]
pub extern "C" fn we_view_resize(handle: *const Handle, width: u32, height: u32, scale: f32, chain: *mut *mut c_void) -> i32 {
    guard(handle, |e| {
        let pointer = e.resize_view(width, height, scale)?;
        if !chain.is_null() {
            unsafe { *chain = pointer as *mut c_void };
        }
        Ok(0)
    })
}

/// Allocates GPU memory for photos of this size in the background, ahead of the first one.
#[unsafe(no_mangle)]
pub extern "C" fn we_prepare(handle: *const Handle, width: u32, height: u32) -> i32 {
    guard(handle, |e| {
        e.prepare(width, height);
        Ok(0)
    })
}

/// Saves the last frame the engine drew as a PNG (a diagnostic: compare with the screen).
#[unsafe(no_mangle)]
pub extern "C" fn we_save_snapshot(handle: *const Handle, path: *const u16) -> i32 {
    guard(handle, |e| {
        let (w, h, px) = e.snapshot().ok_or("nothing drawn yet")?;
        image::save_buffer(wide_to_path(path), &px, w, h, image::ExtendedColorType::Rgba8).map_err(|err| err.to_string())?;
        Ok(0)
    })
}

/// A photo's stars (0–5), pick flag (1 picked, -1 rejected, 0 neither), color label
/// (1–5: Red, Yellow, Green, Blue, Purple; 6 some other name; 0 none).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PhotoMeta {
    pub rating: i32,
    pub pick: i32,
    pub label: i32,
    /// 1 if it has adjustments.
    pub edited: i32,
}

/// Copies every photo's metadata (up to `count`) into `out`; returns how many photos there are.
#[unsafe(no_mangle)]
pub extern "C" fn we_get_metadata(handle: *const Handle, out: *mut PhotoMeta, count: i32) -> i32 {
    guard(handle, |e| {
        let all = e.ratings().all();
        for (i, m) in all.iter().take(count.max(0) as usize).enumerate() {
            let pick = match m.pick {
                Pick::Picked => 1,
                Pick::Rejected => -1,
                Pick::Unflagged => 0,
            };
            let meta = PhotoMeta { rating: m.rating as i32, pick, label: label_code(&m.label), edited: !m.edit.is_default() as i32 };
            unsafe { *out.add(i) = meta };
        }
        Ok(all.len() as i32)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn we_set_rating(handle: *const Handle, index: i32, rating: i32) -> i32 {
    guard(handle, |e| {
        let ok = e.ratings().set(index as usize, |m| m.rating = rating.clamp(0, 5) as u8);
        if ok { Ok(0) } else { Err(format!("there's no photo {index}")) }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn we_set_pick(handle: *const Handle, index: i32, pick: i32) -> i32 {
    guard(handle, |e| {
        let pick = match pick.signum() {
            1 => Pick::Picked,
            -1 => Pick::Rejected,
            _ => Pick::Unflagged,
        };
        let ok = e.ratings().set(index as usize, |m| m.pick = pick);
        if ok { Ok(0) } else { Err(format!("there's no photo {index}")) }
    })
}

/// Sets the color label: 1–5 for Red, Yellow, Green, Blue, Purple; 0 clears it.
#[unsafe(no_mangle)]
pub extern "C" fn we_set_label(handle: *const Handle, index: i32, label: i32) -> i32 {
    guard(handle, |e| {
        let name = match label {
            1..=5 => Some(LABELS[label as usize - 1].to_string()),
            0 => None,
            _ => return Err(format!("there's no label {label}")),
        };
        let ok = e.ratings().set(index as usize, |m| m.label = name);
        if ok { Ok(0) } else { Err(format!("there's no photo {index}")) }
    })
}

fn indices(list: *const i32, count: i32) -> Vec<usize> {
    if list.is_null() || count <= 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(list, count as usize) }.iter().filter(|&&i| i >= 0).map(|&i| i as usize).collect()
}

/// The order the user moves through the photos (after sorting and filtering), so the engine
/// decodes the right neighbours ahead.
#[unsafe(no_mangle)]
pub extern "C" fn we_set_order(handle: *const Handle, order: *const i32, count: i32) -> i32 {
    guard(handle, |e| {
        e.set_order(indices(order, count));
        Ok(0)
    })
}

/// The photos whose thumbnails are on screen, most important first. Each one's thumbnail is
/// made in the background and announced with an EVENT_THUMBNAIL event.
#[unsafe(no_mangle)]
pub extern "C" fn we_set_visible(handle: *const Handle, visible: *const i32, count: i32) -> i32 {
    guard(handle, |e| {
        e.set_visible(indices(visible, count));
        Ok(0)
    })
}

/// Copies a photo's thumbnail (8-bit BGRA, opaque, upright, at most 256 pixels on its longer
/// side) into `buf` if it's ready and fits in `capacity` bytes, writing its size. Returns 1 if
/// copied, 0 if not ready yet, 2 if there won't be one (no usable preview in the photo).
#[unsafe(no_mangle)]
pub extern "C" fn we_thumbnail(handle: *const Handle, index: i32, buf: *mut u8, capacity: i32, width: *mut i32, height: *mut i32) -> i32 {
    guard(handle, |e| {
        let Some(t) = e.jobs().thumbnail(index as usize) else {
            return Ok(if e.jobs().failed(index as usize, crate::jobs::Kind::Thumb) { 2 } else { 0 });
        };
        if t.bgra.len() > capacity.max(0) as usize {
            return Err(format!("the thumbnail needs {} bytes", t.bgra.len()));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(t.bgra.as_ptr(), buf, t.bgra.len());
            *width = t.width as i32;
            *height = t.height as i32;
        }
        Ok(1)
    })
}
