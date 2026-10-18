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
use crate::brushgpu::BrushGpu;
use crate::masks::{GpuMask, GpuMaskSet, MAX_MASKS, Masks};
use crate::gpu::{Gpu, new_buffer, new_texture};
use crate::tone;

const LINEAR_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;
pub const TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;

// Descriptor slots. Each kernel reads the slot it's given and writes the next one.
const SLOT_DEMOSAIC: u32 = 0; // raw data, then pyramid level 0
const fn slot_downsample(level: u32) -> u32 {
    2 * level // level - 1, then level
}
const SLOT_DEVELOP: u32 = 100; // the whole pyramid, the tone parameters, the base coefficients, the statistics, the detail texture, Clarity's coefficients, the masks, then the output
const SLOT_TONE: u32 = 200; // the tone pre-pass: a pyramid level, the parameters, then six outputs
const SLOT_CLARITY: u32 = 215; // the same for Clarity's base

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
    /// Tells this photo's `Image` from the next one opened into the same memory (the detail
    /// pass's cache key).
    pub id: u64,
    /// The photo's noise for the noise reduction, measured on first need (`ensure_noise`).
    pub noise: std::cell::Cell<Option<crate::detail::Noise>>,
}

static NEXT_IMAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

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
    /// Camera RGB → linear Rec.2020, a row each, with the detail texture's rectangle (left and
    /// top, then width and height, 16 bits each) and level + 1 (0 for none) in the spare words.
    row0: [f32; 3],
    detail_origin: u32,
    row1: [f32; 3],
    detail_size: u32,
    row2: [f32; 3],
    detail_level: u32,
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
    /// Manual vignetting: stops at the corners, and the radius where it starts.
    vignette: [f32; 4],
    /// How many masks the pass evaluates (0 skips them), and flags: bit 0 says Clarity's base is there
    /// for a mask's Clarity even with the global slider at 0; bit 1 draws mask `flags >> 8` as the red overlay.
    mask_count: u32,
    mask_flags: u32,
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
    /// The colour mixer (mixer.rs); the develop pass reads it, the tone kernels ignore it.
    mixer: crate::mixer::Params,
}

// tone.hlsli's ToneParams: 27 model numbers, 5 sliders, 5 fixed numbers and the mixer's 4 × 8.
const _: () = assert!(size_of::<ToneParams>() == (37 + 32) * 4);

#[repr(C)]
#[derive(Clone, Copy)]
struct SmallCopyConstants {
    lum: [f32; 4],
    lens_a: [f32; 4],
    lens_k: [f32; 4],
    vignette: [f32; 4],
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
    eps: f32,
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
            id: NEXT_IMAGE.fetch_add(1, Ordering::Relaxed),
            noise: std::cell::Cell::new(None),
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
    // Every detail kernel, on memory that is freed after (the real allocation is made when
    // a view first needs it).
    image.noise.set(Some([0.01; 3]));
    render_image(gpu, &image, &Edit::default(), Zoom::Fit, &target);
    // And the pass with masks, which has its own kernel.
    let mut masks = Masks::default();
    masks.masks[0] = crate::masks::Mask::new();
    masks.masks[0].components[0] = crate::masks::Component { kind: crate::masks::KIND_LINEAR, p: [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0], ..Default::default() };
    masks.masks[0].count = 1;
    masks.masks[0].sliders[crate::masks::slider::CLARITY] = 10.0;
    masks.count = 1;
    render_image_masked(gpu, &image, &Edit::default(), &MaskInput { masks: &masks, overlay: Some(0), brush: None }, Zoom::Fit, &target);
    gpu.submit()?;
    {
        let mut detail = gpu.detail.borrow_mut();
        detail.release();
        detail.runs = 0;
    }
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
    /// The whole frame (not just the crop) as large as fits, with a margin, and the photo turned
    /// beneath it: crop mode. The point of the frame (fractions of its size) at the view's centre is
    /// (`x`, `y`): the middle, except while the photo is dragged under a fixed crop box.
    Frame { x: f32, y: f32 },
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
    render_masked(gpu, layer, edit, &MaskInput::NONE, zoom, photo_size, target);
}

/// The masks to draw with, and the one to show as a red overlay (the Masks panel's selection).
#[derive(Clone, Copy)]
pub struct MaskInput<'a> {
    pub masks: &'a Masks,
    pub overlay: Option<usize>,
    /// The brush layers of the masks' brush components (brushgpu.rs), if any.
    pub brush: Option<&'a BrushGpu>,
}

impl MaskInput<'static> {
    /// No masks: the pass is what it was before masks existed.
    pub const NONE: MaskInput<'static> = MaskInput { masks: &Masks::NONE, overlay: None, brush: None };
}

/// Like [`render`], with local adjustments.
pub fn render_masked(gpu: &Gpu, layer: Layer, edit: &Edit, mask_input: &MaskInput, zoom: Zoom, photo_size: (u32, u32), target: &Target) {
    let masks = mask_input.masks;
    let masks_on = !masks.is_empty() && matches!(layer, Layer::Raw(_));
    let mixer = crate::mixer::Params::from_sliders(&edit.mixer);
    let params = ToneParams {
        model: tone::MODEL,
        sliders: tone::Sliders::from_edit(edit),
        n: tone::N,
        log2_sigma: tone::LOG2_SIGMA,
        ymax: tone::YMAX,
        middle: tone::MIDDLE,
        near_white: tone::NEAR_WHITE,
        mixer,
    };
    gpu.set_params(&params);
    let (rows, lum, mode, base, clarity) = match layer {
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
            let base = tone_prepass(gpu, image, [lum[0], lum[1], lum[2], 0.0], edit);
            let clarity = (edit.clarity != 0.0 || (masks_on && masks.any_clarity())).then(|| clarity_prepass(gpu, image, [lum[0], lum[1], lum[2], 0.0], edit));
            (rows, [lum[0], lum[1], lum[2], 0.0], MODE_DEVELOP, base, clarity)
        }
        Layer::Preview(_) => ([[0.0; 4]; 3], [0.0; 4], MODE_PREVIEW, ([1.0; 2], [1.0; 2]), None),
    };
    let frame = geometry::frame_size(photo_size, edit.rotation);
    let turns = geometry::turns(layer.orientation(), edit.rotation);
    let view = geometry::view_map(layer.texture_size(), turns, frame, edit.angle, edit.crop, zoom, (target.width, target.height));
    // The detail pass (noise reduction, sharpening) writes a texture the develop pass samples in
    // place of the pyramid; None leaves the pyramid.
    let detail = match layer {
        Layer::Raw(image) => crate::detail::prepare(gpu, image, edit, &view, (target.width, target.height), zoom),
        Layer::Preview(_) => None,
    };
    if let (true, Layer::Raw(image)) = (masks_on, layer) {
        let set = match mask_input.brush {
            Some(b) => GpuMaskSet::with_layers(masks, &image.xyz_to_cam, &|id| b.slice_of(id)),
            None => GpuMaskSet::new(masks, &image.xyz_to_cam),
        };
        gpu.set_masks(&set.masks);
    }
    let (detail_origin, detail_size, detail_level) = detail.as_ref().map_or((0, 0, 0), |d| d.constants());
    let constants = DevelopConstants {
        row0: [rows[0][0], rows[0][1], rows[0][2]],
        detail_origin,
        row1: [rows[1][0], rows[1][1], rows[1][2]],
        detail_size,
        row2: [rows[2][0], rows[2][1], rows[2][2]],
        detail_level,
        // lum.w carries Clarity (−1..1; 0 skips it).
        lum: [lum[0], lum[1], lum[2], if clarity.is_some() { edit.clarity / 100.0 } else { 0.0 }],
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
        lens: lens_constants(layer, edit),
        clip: view.clip,
        // canvas.w is 1 when the colour mixer has something to do (its values are in the parameters).
        canvas: { let mut c = canvas_color(); c[3] = if mixer.is_neutral() { 0.0 } else { 1.0 }; c },
        // vignette.zw carries the scale from image coordinates to the Clarity base's pixels.
        vignette: {
            let v = vignette_constants(layer, edit);
            let s = clarity.unwrap_or([0.0; 2]);
            [v[0], v[1], s[0], s[1]]
        },
        mask_count: if masks_on { masks.count } else { 0 },
        mask_flags: if masks_on {
            let overlay = mask_input.overlay.filter(|&i| i < masks.count as usize);
            clarity.is_some() as u32 | overlay.map_or(0, |i| 2 | (i as u32) << 8)
        } else {
            0
        },
    };
    match layer {
        Layer::Raw(image) => gpu.srv_texture(&image.linear, LINEAR_FORMAT, 0, image.levels, SLOT_DEVELOP),
        Layer::Preview(p) => gpu.srv_texture(&p.texture, PREVIEW_FORMAT, 0, p.levels, SLOT_DEVELOP),
    }
    gpu.srv_params::<ToneParams>(SLOT_DEVELOP + 1);
    gpu.srv_texture(&gpu.tone.coefficients, DXGI_FORMAT_R32G32_FLOAT, 0, 1, SLOT_DEVELOP + 2);
    gpu.srv_buffer(&gpu.tone.stats, DXGI_FORMAT_R32_FLOAT, 2, SLOT_DEVELOP + 3);
    match &detail {
        Some(d) => gpu.srv_texture(&d.texture, LINEAR_FORMAT, 0, 1, SLOT_DEVELOP + 4),
        None => gpu.srv_null(SLOT_DEVELOP + 4),
    }
    if clarity.is_some() {
        gpu.srv_texture(&gpu.clarity.coefficients, DXGI_FORMAT_R32G32_FLOAT, 0, 1, SLOT_DEVELOP + 5);
    } else {
        gpu.srv_null(SLOT_DEVELOP + 5);
    }
    gpu.srv_masks::<GpuMask>(MAX_MASKS as u32, SLOT_DEVELOP + 6);
    match mask_input.brush.filter(|_| masks_on) {
        Some(b) => {
            b.record_uploads(gpu);
            b.srv(gpu, SLOT_DEVELOP + 7);
        }
        None => gpu.srv_texture_array(None, DXGI_FORMAT_R16_UNORM, 1, SLOT_DEVELOP + 7),
    }
    gpu.uav_texture(&target.texture, TARGET_FORMAT, 0, SLOT_DEVELOP + 8);
    let mut reads = vec![&gpu.tone.coefficients, &gpu.tone.stats];
    if clarity.is_some() {
        reads.push(&gpu.clarity.coefficients);
    }
    let (uav, srv) = (D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    if mode == MODE_DEVELOP {
        reads.iter().for_each(|r| gpu.transition(r, 0, uav, srv));
    }
    let kernel = if constants.mask_count != 0 { &gpu.kernels.develop_masks } else { &gpu.kernels.develop };
    gpu.dispatch(kernel, &constants, SLOT_DEVELOP, (target.width, target.height));
    if mode == MODE_DEVELOP {
        reads.iter().for_each(|r| gpu.transition(r, 0, srv, uav));
    }
}

/// What a guided-filter base is made of: the copy's long edge, the filter's radius (a fraction of
/// the photo's long edge) and epsilon, and the descriptor slots it uses.
struct GuidedBase {
    edge: f32,
    radius_fraction: f32,
    eps: f32,
    /// Also take the percentiles (the tone model's base does; Clarity's doesn't).
    stats: bool,
    slot: u32,
}

/// Reduces the photo to what the tone model needs of the whole image: a small copy of its tones,
/// the guided filter's coefficients on it (the base layer behind Highlights and Shadows) and two
/// percentiles. Reads a coarse pyramid level, never the full-resolution image; the copy is in
/// the texture's own orientation, like the image. Returns what the develop pass needs to find
/// the coefficients: image coordinates → small-copy pixels, and the small copy's size.
fn tone_prepass(gpu: &Gpu, image: &Image, lum: [f32; 4], edit: &Edit) -> ([f32; 2], [f32; 2]) {
    let g = GuidedBase { edge: tone::BASE_EDGE, radius_fraction: tone::MODEL.base_radius, eps: tone::MODEL.base_eps, stats: true, slot: SLOT_TONE };
    guided_base(gpu, image, &gpu.tone, &g, lum, edit)
}

/// Clarity's base: the same steps at a 512-pixel copy with its own filter. Skipped when what it
/// depends on (the photo, Exposure, Contrast, Blacks, the vignetting and the lens switches) hasn't
/// changed since it was last made. Returns image coordinates → copy pixels, for the develop pass.
fn clarity_prepass(gpu: &Gpu, image: &Image, lum: [f32; 4], edit: &Edit) -> [f32; 2] {
    let edge = image.width.max(image.height) as f32;
    let factor = (edge / tone::CLARITY_EDGE).ceil().max(1.0);
    let scale = [image.width as f32 / factor, image.height as f32 / factor];
    let key = (
        image.id,
        [
            edit.exposure.to_bits(),
            edit.contrast.to_bits(),
            edit.blacks.to_bits(),
            edit.vignette_amount.to_bits(),
            edit.vignette_midpoint.to_bits(),
            edit.no_distortion,
            edit.no_chromatic_aberration,
            edit.custom_white_balance,
            edit.temperature.to_bits(),
            edit.tint.to_bits(),
        ],
    );
    if gpu.clarity_key.get() != Some(key) {
        let g = GuidedBase { edge: tone::CLARITY_EDGE, radius_fraction: tone::CLARITY_RADIUS, eps: tone::CLARITY_EPS, stats: false, slot: SLOT_CLARITY };
        guided_base(gpu, image, &gpu.clarity, &g, lum, edit);
        gpu.clarity_key.set(Some(key));
    }
    scale
}

fn guided_base(gpu: &Gpu, image: &Image, t: &crate::gpu::ToneResources, g: &GuidedBase, lum: [f32; 4], edit: &Edit) -> ([f32; 2], [f32; 2]) {
    let (width, height) = (image.width, image.height);
    let edge = width.max(height);
    // A small copy at most `g.edge` across, each pixel a block of `factor` photo pixels, and the
    // pyramid level with the smallest long edge that is still twice that to read it from.
    let factor = ((edge as f32 / g.edge).ceil() as u32).max(1);
    let small = [width.div_ceil(factor), height.div_ceil(factor)];
    let radius = ((edge as f32 * g.radius_fraction / factor as f32).round() as u32).max(1);
    let mut level = 0;
    while level + 1 < image.levels && edge >> (level + 1) >= 2 * g.edge as u32 {
        level += 1;
    }

    let slot = g.slot;
    gpu.srv_texture(&image.linear, LINEAR_FORMAT, level, 1, slot);
    gpu.srv_params::<ToneParams>(slot + 1);
    gpu.uav_texture(&t.small, DXGI_FORMAT_R32_FLOAT, 0, slot + 2);
    gpu.uav_texture(&t.scratch_a, DXGI_FORMAT_R32_FLOAT, 0, slot + 3);
    gpu.uav_texture(&t.scratch_b, DXGI_FORMAT_R32_FLOAT, 0, slot + 4);
    gpu.uav_texture(&t.coefficients, DXGI_FORMAT_R32G32_FLOAT, 0, slot + 5);
    gpu.uav_buffer(&t.stats, DXGI_FORMAT_R32_FLOAT, 2, slot + 6);
    gpu.uav_texture(&t.scene, DXGI_FORMAT_R32_FLOAT, 0, slot + 7);

    let lens = lens_constants(Layer::Raw(image), edit);
    let constants = SmallCopyConstants {
        lum,
        lens_a: lens[0],
        lens_k: lens[1],
        vignette: vignette_constants(Layer::Raw(image), edit),
        small,
        source: [width, height],
        factor: factor as f32,
        exposure: edit.exposure,
    };
    gpu.dispatch(&gpu.kernels.tone_small, &constants, slot, (small[0], small[1]));
    gpu.uav_barrier();
    for pass in 0..4 {
        gpu.dispatch(&gpu.kernels.tone_guide, &GuideConstants { small, radius, pass, eps: g.eps }, slot, (small[0], small[1]));
        gpu.uav_barrier();
    }
    if g.stats {
        gpu.dispatch_groups(&gpu.kernels.tone_stats, &small, slot, (1, 1));
        gpu.uav_barrier();
    }
    ([width as f32 / factor as f32, height as f32 / factor as f32], [small[0] as f32, small[1] as f32])
}

/// Draws a developed raw on its own (the benchmark and tests).
pub fn render_image(gpu: &Gpu, image: &Image, edit: &Edit, zoom: Zoom, target: &Target) {
    render(gpu, Layer::Raw(image), edit, zoom, image.oriented_size(), target);
}

/// Like [`render_image`], with masks.
pub fn render_image_masked(gpu: &Gpu, image: &Image, edit: &Edit, masks: &MaskInput, zoom: Zoom, target: &Target) {
    render_masked(gpu, Layer::Raw(image), edit, masks, zoom, image.oriented_size(), target);
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

/// Half-precision float to single.
pub fn f16_to_f32(h: u16) -> f32 {
    let (sign, exp, frac) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as u32, (h & 0x3ff) as u32);
    let bits = match (exp, frac) {
        (0, 0) => sign << 31,
        (0, f) => {
            // Subnormal: normalise.
            let shift = f.leading_zeros() - 21;
            (sign << 31) | ((113 - shift) << 23) | (((f << shift) & 0x3ff) << 13)
        }
        (0x1f, f) => (sign << 31) | (0xff << 23) | (f << 13),
        (e, f) => (sign << 31) | ((e + 112) << 23) | (f << 13),
    };
    f32::from_bits(bits)
}

/// A rectangle (x, y, width, height) of one mip level of the developed image, copied back as
/// RGBA. For measuring the photo (the noise estimate, the benchmark): it waits for the GPU.
pub fn read_rgba16f(gpu: &mut Gpu, texture: &ID3D12Resource, mip: u32, rect: (u32, u32, u32, u32)) -> Result<Vec<[f32; 4]>> {
    let (x, y, w, h) = rect;
    unsafe {
        let pitch = (w * 8).div_ceil(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT) * D3D12_TEXTURE_DATA_PITCH_ALIGNMENT;
        let readback = gpu.buffer(pitch as u64 * h as u64, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
        let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: LINEAR_FORMAT, Width: w, Height: h, Depth: 1, RowPitch: pitch },
        };
        gpu.begin()?;
        let (read, copy) = (D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_COPY_SOURCE);
        gpu.transition(texture, mip, read, copy);
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&readback),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: mip },
        };
        let region = D3D12_BOX { left: x, top: y, front: 0, right: x + w, bottom: y + h, back: 1 };
        gpu.list.CopyTextureRegion(&dst, 0, 0, 0, &src, Some(&region));
        gpu.transition(texture, mip, copy, read);
        gpu.submit()?;
        let mut ptr = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut ptr))?;
        let mut out = Vec::with_capacity((w * h) as usize);
        for row in 0..h as usize {
            let line = std::slice::from_raw_parts((ptr as *const u16).byte_add(row * pitch as usize), w as usize * 4);
            out.extend(line.as_chunks::<4>().0.iter().map(|p| p.map(f16_to_f32)));
        }
        readback.Unmap(0, Some(&D3D12_RANGE::default()));
        Ok(out)
    }
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
/// chromatic aberration terms. A camera JPEG is already corrected, so it gets none; the edit's
/// switches take out the distortion or the colour terms (`LensCorrection::with_switches`).
fn lens_constants(layer: Layer, edit: &Edit) -> [[f32; 4]; 4] {
    let Layer::Raw(Image { lens: Some(l), .. }) = layer else { return [[0.0; 4]; 4] };
    let Some(l) = l.with_switches(edit.no_distortion == 0, edit.no_chromatic_aberration == 0) else {
        return [[0.0; 4]; 4];
    };
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

/// The manual vignetting as the shaders read it (develop::vignette_terms). Not for a camera JPEG.
fn vignette_constants(layer: Layer, edit: &Edit) -> [f32; 4] {
    let Layer::Raw(_) = layer else { return [0.0; 4] };
    let (stops, start) = develop::vignette_terms(edit.vignette_amount, edit.vignette_midpoint);
    [stops, start, 0.0, 0.0]
}
