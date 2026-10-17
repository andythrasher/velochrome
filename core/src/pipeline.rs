//! The develop pipeline, split by how often each part runs:
//!
//! - `Image::open` (once per photo): upload the sensor data, demosaic it at full resolution and
//!   build a pyramid of half-size levels, all kept on the GPU.
//! - `render` (every slider move): one pass over the *output* pixels, sampling the pyramid level
//!   that matches the zoom. Its cost follows the window size, not the sensor size.

use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Result;

use crate::color::{self, Mat3};
use crate::decode::{Orientation, RawFrame};
use crate::gpu::Gpu;

const LINEAR_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;
pub const TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;

// Descriptor slots. Each kernel reads the slot it's given and writes the next one.
const SLOT_DEMOSAIC: u32 = 0; // raw data, then pyramid level 0
const fn slot_downsample(level: u32) -> u32 {
    2 * level // level - 1, then level
}
const SLOT_DEVELOP: u32 = 100; // the whole pyramid, then the output

/// A photo ready to develop: demosaiced and on the GPU.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub levels: u32,
    pub linear: ID3D12Resource,
    pub cam_to_working: Mat3,
    pub orientation: Orientation,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenTimings {
    /// CPU: copying the sensor data into memory the GPU can read.
    pub upload_ms: f64,
    /// GPU: copying it into video memory (none on integrated graphics).
    pub copy_ms: f64,
    pub demosaic_ms: f64,
    pub pyramid_ms: f64,
    /// Wall clock for all of the above, from the decoded frame to a developable image.
    pub total_ms: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DemosaicConstants {
    width: u32,
    height: u32,
    cfa: u32,
    white: f32,
    black: [f32; 4],
    wb: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DownsampleConstants {
    dst_width: u32,
    dst_height: u32,
    src_width: u32,
    src_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DevelopConstants {
    rows: [[f32; 4]; 3],
    origin: [f32; 2],
    step_x: [f32; 2],
    step_y: [f32; 2],
    lod: f32,
    pad: f32,
    out_width: u32,
    out_height: u32,
}

impl Image {
    pub fn open(gpu: &mut Gpu, frame: &RawFrame) -> Result<(Image, OpenTimings)> {
        let start = Instant::now();
        let (width, height) = (frame.width, frame.height);
        let bytes = width as u64 * height as u64 * 2;

        // Sensor data goes into an upload buffer, which lives in system memory. Integrated
        // GPUs read it right there; discrete ones get a copy in their own memory.
        let upload = gpu.buffer(bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
        unsafe {
            let mut ptr = std::ptr::null_mut();
            upload.Map(0, Some(&D3D12_RANGE::default()), Some(&mut ptr))?;
            std::ptr::copy_nonoverlapping(frame.data.as_ptr(), ptr as *mut u16, frame.data.len());
            upload.Unmap(0, None);
        }
        let upload_ms = start.elapsed().as_secs_f64() * 1000.0;

        let levels = 32 - width.max(height).leading_zeros();
        let linear = gpu.texture(width, height, levels as u16, LINEAR_FORMAT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;

        gpu.begin()?;
        let t_start = gpu.timestamp();
        let raw = if gpu.uma {
            upload.clone()
        } else {
            let raw = gpu.buffer(bytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON)?;
            unsafe { gpu.list.CopyBufferRegion(&raw, 0, &upload, 0, bytes) };
            gpu.transition(
                &raw,
                0,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            raw
        };
        let t_copied = gpu.timestamp();

        gpu.srv_buffer(&raw, DXGI_FORMAT_R16_UINT, width * height, SLOT_DEMOSAIC);
        gpu.uav_texture(&linear, LINEAR_FORMAT, 0, SLOT_DEMOSAIC + 1);
        let mut cfa = 0u32;
        let mut black = [0f32; 4];
        for r in 0..2 {
            for c in 0..2 {
                cfa |= (frame.cfa[r][c] as u32) << ((r * 2 + c) * 2);
                black[r * 2 + c] = frame.black[r][c];
            }
        }
        let constants = DemosaicConstants {
            width,
            height,
            cfa,
            white: frame.white,
            black,
            wb: [frame.wb[0], frame.wb[1], frame.wb[2], 0.0],
        };
        gpu.dispatch(&gpu.kernels.demosaic, &constants, SLOT_DEMOSAIC, (width, height));
        let t_demosaiced = gpu.timestamp();

        let (mut w, mut h) = (width, height);
        for level in 1..levels {
            let (pw, ph) = (w, h);
            (w, h) = ((w / 2).max(1), (h / 2).max(1));
            gpu.transition(
                &linear,
                level - 1,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            let slot = slot_downsample(level);
            gpu.srv_texture(&linear, LINEAR_FORMAT, level - 1, 1, slot);
            gpu.uav_texture(&linear, LINEAR_FORMAT, level, slot + 1);
            let constants = DownsampleConstants { dst_width: w, dst_height: h, src_width: pw, src_height: ph };
            gpu.dispatch(&gpu.kernels.downsample, &constants, slot, (w, h));
        }
        gpu.transition(
            &linear,
            levels - 1,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        let t_done = gpu.timestamp();
        let ts = gpu.submit()?;
        drop(raw);

        let timings = OpenTimings {
            upload_ms,
            copy_ms: ts[t_copied as usize] - ts[t_start as usize],
            demosaic_ms: ts[t_demosaiced as usize] - ts[t_copied as usize],
            pyramid_ms: ts[t_done as usize] - ts[t_demosaiced as usize],
            total_ms: start.elapsed().as_secs_f64() * 1000.0,
        };
        let image = Image {
            width,
            height,
            levels,
            linear,
            cam_to_working: frame.cam_to_working,
            orientation: frame.orientation,
        };
        Ok((image, timings))
    }

    /// Size on screen, after rotation.
    pub fn oriented_size(&self) -> (u32, u32) {
        if self.orientation.swaps_axes() { (self.height, self.width) } else { (self.width, self.height) }
    }
}

/// The edits. Phase 0 has just one.
#[derive(Debug, Clone, Copy, Default)]
pub struct Edit {
    /// Exposure in stops.
    pub exposure: f32,
}

#[derive(Debug, Clone, Copy)]
pub enum Zoom {
    /// The whole photo, as large as fits.
    Fit,
    /// One image pixel per screen pixel, centered on this point (in rotated image pixels).
    Actual { x: f32, y: f32 },
}

/// A texture the develop pass draws into, the size of the view.
pub struct Target {
    pub texture: ID3D12Resource,
    pub width: u32,
    pub height: u32,
}

impl Target {
    /// Starts out ready for `render` (unordered access).
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Target> {
        let texture = gpu.texture(width, height, 1, TARGET_FORMAT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
        Ok(Target { texture, width, height })
    }
}

/// Records the develop pass (between `gpu.begin` and `gpu.submit`).
pub fn render(gpu: &Gpu, image: &Image, edit: &Edit, zoom: Zoom, target: &Target) {
    let to_display = color::mul(&color::REC2020_TO_SRGB, &image.cam_to_working);
    let gain = edit.exposure.exp2();
    let rows = to_display.map(|r| [r[0] * gain, r[1] * gain, r[2] * gain, 0.0]);
    let (origin, step_x, step_y, lod) = view_transform(image, zoom, target.width, target.height);
    let constants = DevelopConstants {
        rows,
        origin,
        step_x,
        step_y,
        lod,
        pad: 0.0,
        out_width: target.width,
        out_height: target.height,
    };
    gpu.srv_texture(&image.linear, LINEAR_FORMAT, 0, image.levels, SLOT_DEVELOP);
    gpu.uav_texture(&target.texture, TARGET_FORMAT, 0, SLOT_DEVELOP + 1);
    gpu.dispatch(&gpu.kernels.develop, &constants, SLOT_DEVELOP, (target.width, target.height));
}

/// Where each output pixel samples the image: an affine map from output pixels to texture
/// coordinates, covering zoom, pan and the camera's rotation, plus the pyramid level to use.
fn view_transform(image: &Image, zoom: Zoom, out_w: u32, out_h: u32) -> ([f32; 2], [f32; 2], [f32; 2], f32) {
    let (ow, oh) = image.oriented_size();
    let (w, h) = (image.width as f32, image.height as f32);
    let (scale, center) = match zoom {
        Zoom::Fit => ((out_w as f32 / ow as f32).min(out_h as f32 / oh as f32), (ow as f32 / 2.0, oh as f32 / 2.0)),
        Zoom::Actual { x, y } => (1.0, (x, y)),
    };
    let orientation = image.orientation;
    // Output pixel → rotated image pixel → sensor pixel → texture coordinate.
    let map = |px: f32, py: f32| -> [f32; 2] {
        let qx = center.0 + (px - out_w as f32 / 2.0) / scale;
        let qy = center.1 + (py - out_h as f32 / 2.0) / scale;
        let (rx, ry) = match orientation {
            Orientation::Normal => (qx, qy),
            Orientation::Rotate90 => (qy, h - qx),
            Orientation::Rotate180 => (w - qx, h - qy),
            Orientation::Rotate270 => (w - qy, qx),
        };
        [rx / w, ry / h]
    };
    let o = map(0.0, 0.0);
    let x = map(1.0, 0.0);
    let y = map(0.0, 1.0);
    let lod = (1.0 / scale).log2().max(0.0);
    (o, [x[0] - o[0], x[1] - o[1]], [y[0] - o[0], y[1] - o[1]], lod)
}

/// Copies a target back to the CPU as tightly packed RGBA bytes (for tests and the benchmark).
pub fn read_target(gpu: &mut Gpu, target: &Target) -> Result<Vec<u8>> {
    unsafe {
        let desc = target.texture.GetDesc();
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        gpu.device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut footprint), None, None, Some(&mut total));
        let readback = gpu.buffer(total, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;

        gpu.begin()?;
        gpu.transition(&target.texture, 0, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&readback),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&target.texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        gpu.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        gpu.transition(&target.texture, 0, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        gpu.submit()?;

        let mut ptr = std::ptr::null_mut();
        readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: total as usize }), Some(&mut ptr))?;
        let pitch = footprint.Footprint.RowPitch as usize;
        let row = target.width as usize * 4;
        let mut pixels = Vec::with_capacity(row * target.height as usize);
        for y in 0..target.height as usize {
            pixels.extend_from_slice(std::slice::from_raw_parts((ptr as *const u8).add(y * pitch), row));
        }
        readback.Unmap(0, Some(&D3D12_RANGE::default()));
        Ok(pixels)
    }
}
