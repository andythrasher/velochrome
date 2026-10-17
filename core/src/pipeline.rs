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
use crate::develop;
use crate::decode::{Orientation, RawFrame};
use crate::gpu::{Gpu, new_buffer, new_texture};

const LINEAR_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;
pub const TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;

// Descriptor slots. Each kernel reads the slot it's given and writes the next one.
const SLOT_DEMOSAIC: u32 = 0; // raw data, then pyramid level 0
const fn slot_downsample(level: u32) -> u32 {
    2 * level // level - 1, then level
}
const SLOT_DEVELOP: u32 = 100; // the whole pyramid, the tone curve, then the output

/// The GPU memory photos are opened into, kept between photos. Allocating it is slow (about
/// 50 ms on a Lunar Lake laptop, and the GPU's first pass over fresh memory is slower still),
/// so each photo size gets its memory once and every later photo of that size reuses it.
///
/// Opening a photo overwrites the memory of the last photo of the same size, so only the most
/// recent `Image` of each size stays valid.
pub struct ImageMemory {
    /// Most recently used first.
    slots: Vec<Slot>,
}

/// How many photo sizes keep their memory: enough for two cameras mixed in one folder. Each
/// costs about 350 MB for a 24-megapixel photo.
const SLOTS: usize = 2;

/// The GPU memory for one photo size. Can be allocated on any thread (`Slot::allocate`) and
/// handed to the GPU thread's `ImageMemory`.
pub struct Slot {
    pub width: u32,
    pub height: u32,
    levels: u32,
    /// Sensor data, in memory the CPU writes and the GPU can read. Mapped for its whole life.
    upload: ID3D12Resource,
    upload_ptr: *mut u16,
    /// A copy in video memory, for GPUs that have their own (discrete cards).
    raw: Option<ID3D12Resource>,
    linear: ID3D12Resource,
    /// A photo has been opened into it, so its resources are in their after-open states.
    used: bool,
}

impl Default for ImageMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageMemory {
    pub fn new() -> ImageMemory {
        ImageMemory { slots: Vec::new() }
    }

    /// The slot for this size, moved to the front; allocated if there's none yet. Returns
    /// whether it was just allocated.
    fn take(&mut self, gpu: &Gpu, width: u32, height: u32) -> Result<bool> {
        if let Some(i) = self.slots.iter().position(|s| s.width == width && s.height == height) {
            let slot = self.slots.remove(i);
            self.slots.insert(0, slot);
            return Ok(false);
        }
        // Free the least recently used size before allocating, so the peak stays down.
        if self.slots.len() >= SLOTS {
            self.slots.pop();
        }
        let slot = Slot::allocate(&gpu.device, gpu.uma, width, height)?;
        self.slots.insert(0, slot);
        Ok(true)
    }

    /// Whether this photo size already has memory.
    pub fn has(&self, width: u32, height: u32) -> bool {
        self.slots.iter().any(|s| s.width == width && s.height == height)
    }

    /// Adds memory allocated elsewhere (see `Slot::allocate`), as the most recently used.
    pub fn insert(&mut self, slot: Slot) {
        if self.has(slot.width, slot.height) {
            return;
        }
        if self.slots.len() >= SLOTS {
            self.slots.pop();
        }
        self.slots.insert(0, slot);
    }
}

impl Slot {
    /// Allocates the memory for one photo size. Needs only the device, so it can run on a
    /// helper thread while the GPU thread keeps drawing.
    pub fn allocate(device: &ID3D12Device, uma: bool, width: u32, height: u32) -> Result<Slot> {
        let bytes = width as u64 * height as u64 * 2;
        let upload = new_buffer(device, bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
        let mut ptr = std::ptr::null_mut();
        unsafe { upload.Map(0, Some(&D3D12_RANGE::default()), Some(&mut ptr))? };
        let raw = if uma {
            None
        } else {
            Some(new_buffer(device, bytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON)?)
        };
        let levels = 32 - width.max(height).leading_zeros();
        let linear =
            new_texture(device, width, height, levels as u16, LINEAR_FORMAT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
        Ok(Slot { width, height, levels, upload, upload_ptr: ptr as *mut u16, raw, linear, used: false })
    }
}

// SAFETY: Direct3D 12 resources may be used from any thread, and the mapped upload pointer is
// only written by whichever thread owns the slot.
unsafe impl Send for Slot {}

/// A photo ready to develop: demosaiced and on the GPU.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub levels: u32,
    pub linear: ID3D12Resource,
    pub cam_to_working: Mat3,
    pub xyz_to_cam: Mat3,
    /// The white balance the camera recorded, which the demosaic applied.
    pub as_shot: [f32; 3],
    pub orientation: Orientation,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenTimings {
    /// CPU: allocating GPU memory, when this photo size had none yet (otherwise 0).
    pub alloc_ms: f64,
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
    mode: u32,
    out_width: u32,
    out_height: u32,
    saturation: f32,
    vibrance: f32,
}

impl Image {
    pub fn open(gpu: &mut Gpu, memory: &mut ImageMemory, frame: &RawFrame) -> Result<(Image, OpenTimings)> {
        let start = Instant::now();
        let (width, height) = (frame.width, frame.height);
        let bytes = width as u64 * height as u64 * 2;

        let fresh = memory.take(gpu, width, height)?;
        let alloc_ms = if fresh { start.elapsed().as_secs_f64() * 1000.0 } else { 0.0 };
        let slot = &mut memory.slots[0];
        let (levels, linear) = (slot.levels, slot.linear.clone());

        // Sensor data goes into the upload buffer, which lives in system memory. Integrated
        // GPUs read it right there; discrete ones get a copy in their own memory.
        let upload_start = Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(frame.data.as_ptr(), slot.upload_ptr, frame.data.len()) };
        let upload_ms = upload_start.elapsed().as_secs_f64() * 1000.0;

        gpu.begin()?;
        if slot.used {
            // Back from where the last photo left them.
            gpu.transition(
                &linear,
                D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
        }
        let t_start = gpu.timestamp();
        let raw = match &slot.raw {
            None => slot.upload.clone(),
            Some(raw) => {
                if slot.used {
                    gpu.transition(raw, 0, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
                }
                unsafe { gpu.list.CopyBufferRegion(raw, 0, &slot.upload, 0, bytes) };
                gpu.transition(raw, 0, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
                raw.clone()
            }
        };
        slot.used = true;
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
            alloc_ms,
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
            xyz_to_cam: frame.xyz_to_cam,
            as_shot: frame.wb,
            orientation: frame.orientation,
        };
        Ok((image, timings))
    }

    /// Size on screen, after rotation.
    pub fn oriented_size(&self) -> (u32, u32) {
        if self.orientation.swaps_axes() { (self.height, self.width) } else { (self.width, self.height) }
    }
}

/// Runs every kernel once on a tiny made-up photo, so the driver has finished preparing them
/// (some drivers compile shaders on first use) before a real photo is opened. Returns how long
/// it took, in milliseconds.
pub fn warm_up(gpu: &mut Gpu) -> Result<f64> {
    let start = Instant::now();
    const SIZE: u32 = 64;
    let frame = RawFrame {
        make: String::new(),
        model: String::new(),
        data: vec![1000; (SIZE * SIZE) as usize],
        width: SIZE,
        height: SIZE,
        cfa: [[0, 1], [1, 2]],
        black: [[0.0; 2]; 2],
        white: 4095.0,
        wb: [2.0, 1.0, 1.5],
        cam_to_working: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        xyz_to_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        orientation: Orientation::Normal,
        decode_time: Default::default(),
    };
    let (image, _) = Image::open(gpu, &mut ImageMemory::new(), &frame)?;
    let target = Target::new(gpu, SIZE, SIZE)?;
    gpu.begin()?;
    render_image(gpu, &image, &Edit::default(), Zoom::Fit, &target);
    gpu.submit()?;
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

pub use crate::develop::Edit;

#[derive(Debug, Clone, Copy)]
pub enum Zoom {
    /// The whole photo, as large as fits.
    Fit,
    /// One photo pixel per screen pixel, centered on this point, given as fractions (0..1) of the
    /// rotated photo's width and height. Fractions, so the point stays put when a preview is
    /// replaced by a bigger one or by the raw.
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

/// What the view draws: a developed raw, or one of the camera's JPEGs.
#[derive(Clone, Copy)]
pub enum Layer<'a> {
    Raw(&'a Image),
    Preview(&'a PreviewTexture),
}

impl Layer<'_> {
    /// Size of the texture, before rotation.
    fn texture_size(&self) -> (u32, u32) {
        match self {
            Layer::Raw(i) => (i.width, i.height),
            Layer::Preview(p) => (p.width, p.height),
        }
    }

    fn orientation(&self) -> Orientation {
        match self {
            Layer::Raw(i) => i.orientation,
            Layer::Preview(p) => p.orientation,
        }
    }
}

const MODE_DEVELOP: u32 = 0;
const MODE_PREVIEW: u32 = 1;

/// Records drawing `layer` into `target` (between `gpu.begin` and `gpu.submit`).
/// `photo_size` is the photo's full size after rotation, which 100% zoom is relative to; a
/// preview smaller than that is scaled up to stand in for it.
pub fn render(gpu: &Gpu, layer: Layer, edit: &Edit, zoom: Zoom, photo_size: (u32, u32), target: &Target) {
    let (rows, mode) = match layer {
        Layer::Raw(image) => {
            // White balance change, exposure and both color conversions in one matrix: camera
            // RGB (as-shot white balance) → linear sRGB, scene-referred.
            let wb = develop::white_balance_change(edit, &image.xyz_to_cam, image.as_shot);
            let gain = edit.exposure.exp2();
            let to_display = color::mul(&color::REC2020_TO_SRGB, &image.cam_to_working);
            let rows = to_display.map(|r| [r[0] * wb[0] * gain, r[1] * wb[1] * gain, r[2] * wb[2] * gain, 0.0]);
            gpu.set_curve(&develop::tone_curve(edit));
            (rows, MODE_DEVELOP)
        }
        Layer::Preview(_) => ([[0.0; 4]; 3], MODE_PREVIEW),
    };
    let (origin, step_x, step_y, lod) =
        view_transform(layer.texture_size(), layer.orientation(), photo_size, zoom, target.width, target.height);
    let constants = DevelopConstants {
        rows,
        origin,
        step_x,
        step_y,
        lod,
        mode,
        out_width: target.width,
        out_height: target.height,
        saturation: edit.saturation / 100.0,
        vibrance: edit.vibrance / 100.0,
    };
    match layer {
        Layer::Raw(image) => gpu.srv_texture(&image.linear, LINEAR_FORMAT, 0, image.levels, SLOT_DEVELOP),
        Layer::Preview(p) => gpu.srv_texture(&p.texture, PREVIEW_FORMAT, 0, p.levels, SLOT_DEVELOP),
    }
    gpu.srv_curve(SLOT_DEVELOP + 1);
    gpu.uav_texture(&target.texture, TARGET_FORMAT, 0, SLOT_DEVELOP + 2);
    gpu.dispatch(&gpu.kernels.develop, &constants, SLOT_DEVELOP, (target.width, target.height));
}

/// Draws a developed raw on its own (the benchmark and tests).
pub fn render_image(gpu: &Gpu, image: &Image, edit: &Edit, zoom: Zoom, target: &Target) {
    render(gpu, Layer::Raw(image), edit, zoom, image.oriented_size(), target);
}

/// Where each output pixel samples the texture: an affine map from output pixels to texture
/// coordinates covering zoom, pan and the camera's rotation, plus the pyramid level to use.
/// Worked out in the rotated photo's own pixels, then normalized, so the texture's resolution
/// only affects the pyramid level.
fn view_transform(
    texture: (u32, u32),
    orientation: Orientation,
    photo: (u32, u32),
    zoom: Zoom,
    out_w: u32,
    out_h: u32,
) -> ([f32; 2], [f32; 2], [f32; 2], f32) {
    let (pw, ph) = (photo.0 as f32, photo.1 as f32);
    let (scale, center) = match zoom {
        Zoom::Fit => ((out_w as f32 / pw).min(out_h as f32 / ph), (pw / 2.0, ph / 2.0)),
        Zoom::Actual { x, y } => (1.0, (x.clamp(0.0, 1.0) * pw, y.clamp(0.0, 1.0) * ph)),
    };
    // Output pixel → rotated photo (0..1) → texture (0..1).
    let map = |px: f32, py: f32| -> [f32; 2] {
        let u = (center.0 + (px - out_w as f32 / 2.0) / scale) / pw;
        let v = (center.1 + (py - out_h as f32 / 2.0) / scale) / ph;
        match orientation {
            Orientation::Normal => [u, v],
            Orientation::Rotate90 => [v, 1.0 - u],
            Orientation::Rotate180 => [1.0 - u, 1.0 - v],
            Orientation::Rotate270 => [1.0 - v, u],
        }
    };
    let o = map(0.0, 0.0);
    let x = map(1.0, 0.0);
    let y = map(0.0, 1.0);
    // Texture pixels per screen pixel, along the photo's width.
    let texture_w = if orientation.swaps_axes() { texture.1 } else { texture.0 } as f32;
    let lod = (texture_w / pw / scale).log2().max(0.0);
    (o, [x[0] - o[0], x[1] - o[1]], [y[0] - o[0], y[1] - o[1]], lod)
}

const PREVIEW_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;
// Pyramid levels of a preview: level - 1, then level.
const fn slot_preview(level: u32) -> u32 {
    120 + 2 * level
}

/// A camera JPEG on the GPU, with a pyramid so a big one can be shown small without shimmer.
/// Holds WIC's BGRA bytes as they are; the shader swaps the channels back.
pub struct PreviewTexture {
    pub texture: ID3D12Resource,
    pub width: u32,
    pub height: u32,
    pub levels: u32,
    pub orientation: Orientation,
    upload: ID3D12Resource,
    upload_size: u64,
}

impl PreviewTexture {
    /// Puts `pixels` on the GPU, reusing this texture's memory when the size matches.
    pub fn upload(
        gpu: &mut Gpu,
        reuse: Option<PreviewTexture>,
        pixels: &crate::jpeg::Pixels,
        orientation: Orientation,
    ) -> Result<PreviewTexture> {
        let (width, height) = (pixels.width, pixels.height);
        let levels = 32 - width.max(height).leading_zeros();
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width as u64,
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: levels as u16,
            Format: PREVIEW_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut upload_size = 0u64;
        unsafe {
            gpu.device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut footprint), None, None, Some(&mut upload_size))
        };

        let mut tex = match reuse {
            Some(t) if (t.width, t.height) == (width, height) => {
                // Back from where the last preview left it, to receive a copy.
                gpu.begin()?;
                gpu.transition(
                    &t.texture,
                    D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                );
                gpu.submit()?;
                PreviewTexture { orientation, ..t }
            }
            reuse => {
                let texture = gpu.texture(width, height, levels as u16, PREVIEW_FORMAT, D3D12_RESOURCE_STATE_COPY_DEST)?;
                let upload = match reuse {
                    Some(t) if t.upload_size >= upload_size => t.upload,
                    _ => gpu.buffer(upload_size, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?,
                };
                let upload_size = unsafe { upload.GetDesc().Width };
                PreviewTexture { texture, width, height, levels, orientation, upload, upload_size }
            }
        };

        unsafe {
            let mut ptr = std::ptr::null_mut();
            tex.upload.Map(0, Some(&D3D12_RANGE::default()), Some(&mut ptr))?;
            let pitch = footprint.Footprint.RowPitch as usize;
            let row = width as usize * 4;
            for y in 0..height as usize {
                std::ptr::copy_nonoverlapping(pixels.bgra.as_ptr().add(y * row), (ptr as *mut u8).add(y * pitch), row);
            }
            tex.upload.Unmap(0, None);
        }

        gpu.begin()?;
        unsafe {
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&tex.texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&tex.upload),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
            };
            gpu.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        // Level 0 to readable, the rest to writable, then build the pyramid as for raws.
        gpu.transition(&tex.texture, 0, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
        let (mut w, mut h) = (width, height);
        for level in 1..levels {
            gpu.transition(&tex.texture, level, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
            let (pw, ph) = (w, h);
            (w, h) = ((w / 2).max(1), (h / 2).max(1));
            let slot = slot_preview(level);
            gpu.srv_texture(&tex.texture, PREVIEW_FORMAT, level - 1, 1, slot);
            gpu.uav_texture(&tex.texture, PREVIEW_FORMAT, level, slot + 1);
            let constants = DownsampleConstants { dst_width: w, dst_height: h, src_width: pw, src_height: ph };
            gpu.dispatch(&gpu.kernels.downsample, &constants, slot, (w, h));
            gpu.transition(&tex.texture, level, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
        }
        gpu.submit()?;
        tex.levels = levels;
        Ok(tex)
    }
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
