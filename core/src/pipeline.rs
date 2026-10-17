//! The develop pipeline, split by how often each part runs:
//!
//! - `Image::open` (once per photo): upload the sensor data, demosaic it at full resolution and
//!   build a pyramid of half-size levels, all kept on the GPU.
//! - `render` (every slider move): one pass over the *output* pixels, sampling the pyramid level
//!   that matches the zoom. Its cost follows the window size, not the sensor size.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Result;

use crate::color::{self, Mat3};
use crate::decode::{Orientation, RawFrame};
use crate::develop;
use crate::geometry;
use crate::lens::{LensCorrection, LensModel};
use crate::gpu::{Gpu, new_buffer, new_texture};
use crate::tone;

const LINEAR_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;
pub const TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;

// Descriptor slots. Each kernel reads the slot it's given and writes the next one.
const SLOT_DEMOSAIC: u32 = 0; // raw data, then pyramid level 0
const fn slot_downsample(level: u32) -> u32 {
    2 * level // level - 1, then level
}
const SLOT_DEVELOP: u32 = 100; // the whole pyramid, the tone parameters, the base coefficients, the statistics, then the output
const SLOT_TONE: u32 = 200; // the tone pre-pass: a pyramid level, the parameters, then six outputs

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
    /// Distortion and chromatic aberration correction, if the camera recorded any.
    pub lens: Option<LensCorrection>,
    /// How many stops this camera's raws are rendered brighter or darker (`tone::camera_offset`).
    pub camera_offset: f32,
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
    lum: [f32; 4],
    origin: [f32; 2],
    step_x: [f32; 2],
    step_y: [f32; 2],
    lod: f32,
    mode: u32,
    out_width: u32,
    out_height: u32,
    saturation: f32,
    vibrance: f32,
    base_scale: [f32; 2],
    base_size: [f32; 2],
    lens: [[f32; 4]; 4],
    clip: [f32; 4],
    canvas: [f32; 4],
}

/// What the tone shaders read, field for field as in tone.hlsli: the model's numbers, the
/// slider values, and the base curve's fixed numbers.
#[repr(C)]
#[derive(Clone, Copy)]
struct ToneParams {
    model: tone::Model,
    sliders: tone::Sliders,
    n: f32,
    log2_sigma: f32,
    ymax: f32,
    middle: f32,
    near_white: f32,
}

// tone.hlsli's ToneParams: 27 model numbers, 5 sliders and 5 fixed numbers.
const _: () = assert!(size_of::<ToneParams>() == 37 * 4);

#[repr(C)]
#[derive(Clone, Copy)]
struct SmallCopyConstants {
    lum: [f32; 4],
    small: [u32; 2],
    source: [u32; 2],
    factor: f32,
    exposure: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GuideConstants {
    small: [u32; 2],
    radius: u32,
    pass: u32,
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
            lens: frame.lens,
            camera_offset: tone::camera_offset(&frame.make),
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
        lens: None,
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
    /// cropped photo's width and height. Fractions, so the point stays put when a preview is
    /// replaced by a bigger one or by the raw.
    Actual { x: f32, y: f32 },
    /// An exact window of the frame, for export: its top-left corner in frame pixels,
    /// and output pixels per photo pixel (1 for the photo's own size). Output pixel (0, 0) sits
    /// on the corner, so tiles at whole-pixel corners line up without any resampling.
    Region { x: f32, y: f32, scale: f32 },
    /// The whole frame (not just the crop) as large as fits, with the photo turned beneath it: crop mode.
    Frame,
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

/// The empty canvas around the photo (red, green, blue as f32 bits). The app sets it from its
/// theme; it's the same for every view.
static CANVAS: [AtomicU32; 3] = [const { AtomicU32::new(0.11_f32.to_bits()) }; 3];

pub fn set_canvas_color(rgb: [f32; 3]) {
    for (slot, v) in CANVAS.iter().zip(rgb) {
        slot.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

fn canvas_color() -> [f32; 4] {
    let c = |i: usize| f32::from_bits(CANVAS[i].load(Ordering::Relaxed));
    [c(0), c(1), c(2), 0.0]
}

const MODE_DEVELOP: u32 = 0;
const MODE_PREVIEW: u32 = 1;

/// Records drawing `layer` into `target` (between `gpu.begin` and `gpu.submit`).
/// `photo_size` is the photo's full size after the camera's orientation (before the edit's own
/// turns); a preview smaller than that is scaled up to stand in for it. The edit's rotation,
/// angle and crop decide what part of it is shown, and `zoom` is relative to that.
pub fn render(gpu: &Gpu, layer: Layer, edit: &Edit, zoom: Zoom, photo_size: (u32, u32), target: &Target) {
    let params = ToneParams {
        model: tone::MODEL,
        sliders: tone::Sliders::from_edit(edit),
        n: tone::N,
        log2_sigma: tone::LOG2_SIGMA,
        ymax: tone::YMAX,
        middle: tone::MIDDLE,
        near_white: tone::NEAR_WHITE,
    };
    gpu.set_params(&params);
    let (rows, lum, mode, base) = match layer {
        Layer::Raw(image) => {
            // White balance change, exposure, the camera's offset and the conversion to linear
            // Rec.2020 in one matrix: camera RGB (as-shot white balance) → scene-referred light.
            let wb = develop::white_balance_change(edit, &image.xyz_to_cam, image.as_shot);
            let gain = (image.camera_offset + edit.exposure).exp2();
            let rows = image.cam_to_working.map(|r| [r[0] * wb[0] * gain, r[1] * wb[1] * gain, r[2] * wb[2] * gain, 0.0]);
            // The same matrix to brightness: Rec.709 luminance of the sRGB conversion.
            let to_srgb = color::mul(&color::REC2020_TO_SRGB, &image.cam_to_working);
            let lum = [0, 1, 2].map(|c| {
                gain * wb[c] * (0.2126 * to_srgb[0][c] + 0.7152 * to_srgb[1][c] + 0.0722 * to_srgb[2][c])
            });
            let base = tone_prepass(gpu, image, [lum[0], lum[1], lum[2], 0.0], edit.exposure);
            (rows, [lum[0], lum[1], lum[2], 0.0], MODE_DEVELOP, base)
        }
        Layer::Preview(_) => ([[0.0; 4]; 3], [0.0; 4], MODE_PREVIEW, ([1.0; 2], [1.0; 2])),
    };
    let frame = geometry::frame_size(photo_size, edit.rotation);
    let turns = geometry::turns(layer.orientation(), edit.rotation);
    let view = geometry::view_map(layer.texture_size(), turns, frame, edit.angle, edit.crop, zoom, (target.width, target.height));
    let constants = DevelopConstants {
        rows,
        lum,
        origin: view.origin,
        step_x: view.step_x,
        step_y: view.step_y,
        lod: view.lod,
        mode,
        out_width: target.width,
        out_height: target.height,
        saturation: edit.saturation / 100.0,
        vibrance: edit.vibrance / 100.0,
        base_scale: base.0,
        base_size: base.1,
        lens: lens_constants(layer),
        clip: view.clip,
        canvas: canvas_color(),
    };
    match layer {
        Layer::Raw(image) => gpu.srv_texture(&image.linear, LINEAR_FORMAT, 0, image.levels, SLOT_DEVELOP),
        Layer::Preview(p) => gpu.srv_texture(&p.texture, PREVIEW_FORMAT, 0, p.levels, SLOT_DEVELOP),
    }
    gpu.srv_params::<ToneParams>(SLOT_DEVELOP + 1);
    gpu.srv_texture(&gpu.tone.coefficients, DXGI_FORMAT_R32G32_FLOAT, 0, 1, SLOT_DEVELOP + 2);
    gpu.srv_buffer(&gpu.tone.stats, DXGI_FORMAT_R32_FLOAT, 2, SLOT_DEVELOP + 3);
    gpu.uav_texture(&target.texture, TARGET_FORMAT, 0, SLOT_DEVELOP + 4);
    let reads = [&gpu.tone.coefficients, &gpu.tone.stats];
    let (uav, srv) = (D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    if mode == MODE_DEVELOP {
        reads.iter().for_each(|r| gpu.transition(r, 0, uav, srv));
    }
    gpu.dispatch(&gpu.kernels.develop, &constants, SLOT_DEVELOP, (target.width, target.height));
    if mode == MODE_DEVELOP {
        reads.iter().for_each(|r| gpu.transition(r, 0, srv, uav));
    }
}

/// Reduces the photo to what the tone model needs of the whole image: a small copy of its tones,
/// the guided filter's coefficients on it (the base layer behind Highlights and Shadows) and two
/// percentiles. Reads a coarse pyramid level, never the full-resolution image; the copy is in
/// the texture's own orientation, like the image. Returns what the develop pass needs to find
/// the coefficients: image coordinates → small-copy pixels, and the small copy's size.
fn tone_prepass(gpu: &Gpu, image: &Image, lum: [f32; 4], exposure: f32) -> ([f32; 2], [f32; 2]) {
    let (width, height) = (image.width, image.height);
    let edge = width.max(height);
    // A small copy at most BASE_EDGE across, each pixel a block of `factor` photo pixels, and
    // the pyramid level with the smallest long edge that is still 256 or more to read it from.
    let factor = ((edge as f32 / tone::BASE_EDGE).ceil() as u32).max(1);
    let small = [width.div_ceil(factor), height.div_ceil(factor)];
    let radius = ((edge as f32 * tone::MODEL.base_radius / factor as f32).round() as u32).max(1);
    let mut level = 0;
    while level + 1 < image.levels && edge >> (level + 1) >= 256 {
        level += 1;
    }

    let t = &gpu.tone;
    gpu.srv_texture(&image.linear, LINEAR_FORMAT, level, 1, SLOT_TONE);
    gpu.srv_params::<ToneParams>(SLOT_TONE + 1);
    gpu.uav_texture(&t.small, DXGI_FORMAT_R32_FLOAT, 0, SLOT_TONE + 2);
    gpu.uav_texture(&t.scratch_a, DXGI_FORMAT_R32_FLOAT, 0, SLOT_TONE + 3);
    gpu.uav_texture(&t.scratch_b, DXGI_FORMAT_R32_FLOAT, 0, SLOT_TONE + 4);
    gpu.uav_texture(&t.coefficients, DXGI_FORMAT_R32G32_FLOAT, 0, SLOT_TONE + 5);
    gpu.uav_buffer(&t.stats, DXGI_FORMAT_R32_FLOAT, 2, SLOT_TONE + 6);
    gpu.uav_texture(&t.scene, DXGI_FORMAT_R32_FLOAT, 0, SLOT_TONE + 7);

    let constants = SmallCopyConstants { lum, small, source: [width, height], factor: factor as f32, exposure };
    gpu.dispatch(&gpu.kernels.tone_small, &constants, SLOT_TONE, (small[0], small[1]));
    gpu.uav_barrier();
    for pass in 0..4 {
        gpu.dispatch(&gpu.kernels.tone_guide, &GuideConstants { small, radius, pass }, SLOT_TONE, (small[0], small[1]));
        gpu.uav_barrier();
    }
    gpu.dispatch_groups(&gpu.kernels.tone_stats, &small, SLOT_TONE, (1, 1));
    gpu.uav_barrier();
    ([width as f32 / factor as f32, height as f32 / factor as f32], [small[0] as f32, small[1] as f32])
}

/// Draws a developed raw on its own (the benchmark and tests).
pub fn render_image(gpu: &Gpu, image: &Image, edit: &Edit, zoom: Zoom, target: &Target) {
    render(gpu, Layer::Raw(image), edit, zoom, image.oriented_size(), target);
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

/// What the last tone pre-pass worked out, copied back to the CPU (for the checks): the small
/// copy of the tones, the guided filter's a and b on it (both with the row length `small[0]`),
/// and the two percentiles (99.5th, 25th).
pub struct ToneMaps {
    pub small: Vec<f32>,
    pub coefficients: Vec<[f32; 2]>,
    pub stats: [f32; 2],
}

/// Reads back the tone pre-pass results of the last render of `image`, whose size they follow.
pub fn read_tone_maps(gpu: &mut Gpu, image: &Image) -> Result<ToneMaps> {
    let edge = image.width.max(image.height);
    let factor = ((edge as f32 / tone::BASE_EDGE).ceil() as u32).max(1);
    let (sw, sh) = (image.width.div_ceil(factor) as usize, image.height.div_ceil(factor) as usize);
    let (small, coefficients, stats) = (gpu.tone.small.clone(), gpu.tone.coefficients.clone(), gpu.tone.stats.clone());
    let (small, coefficients) = (read_floats(gpu, &small, 1, sw, sh)?, read_floats(gpu, &coefficients, 2, sw, sh)?);
    let stats = unsafe {
        let readback = gpu.buffer(16, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
        gpu.begin()?;
        gpu.transition(&stats, 0, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        gpu.list.CopyBufferRegion(&readback, 0, &stats, 0, 8);
        gpu.transition(&stats, 0, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        gpu.submit()?;
        let mut ptr = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut ptr))?;
        let v = std::slice::from_raw_parts(ptr as *const f32, 2);
        let out = [v[0], v[1]];
        readback.Unmap(0, Some(&D3D12_RANGE::default()));
        out
    };
    Ok(ToneMaps { small, coefficients: coefficients.as_chunks::<2>().0.to_vec(), stats })
}

/// The top-left `w` × `h` of a float texture with `channels` floats per pixel, tightly packed.
fn read_floats(gpu: &mut Gpu, texture: &ID3D12Resource, channels: usize, w: usize, h: usize) -> Result<Vec<f32>> {
    unsafe {
        let desc = texture.GetDesc();
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        gpu.device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut footprint), None, None, Some(&mut total));
        let readback = gpu.buffer(total, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
        gpu.begin()?;
        gpu.transition(texture, 0, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&readback),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        gpu.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        gpu.transition(texture, 0, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        gpu.submit()?;
        let mut ptr = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut ptr))?;
        let pitch = footprint.Footprint.RowPitch as usize;
        let mut out = Vec::with_capacity(w * h * channels);
        for y in 0..h {
            let row = std::slice::from_raw_parts((ptr as *const f32).byte_add(y * pitch), w * channels);
            out.extend_from_slice(row);
        }
        readback.Unmap(0, Some(&D3D12_RANGE::default()));
        Ok(out)
    }
}

/// The lens correction as the develop shader reads it: model (0 none, 1 Olympus, 2 Panasonic),
/// scale, zoom and a chromatic aberration flag; then the distortion terms and red's and blue's
/// chromatic aberration terms. A camera JPEG is already corrected, so it gets none.
fn lens_constants(layer: Layer) -> [[f32; 4]; 4] {
    let Layer::Raw(Image { lens: Some(l), .. }) = layer else { return [[0.0; 4]; 4] };
    let model = match l.model {
        LensModel::Olympus => 1.0,
        LensModel::Panasonic => 2.0,
    };
    [
        [model, l.scale, l.zoom, l.has_ca() as u32 as f32],
        [l.k[0], l.k[1], l.k[2], 0.0],
        [l.ca_red[0], l.ca_red[1], l.ca_red[2], 0.0],
        [l.ca_blue[0], l.ca_blue[1], l.ca_blue[2], 0.0],
    ]
}
