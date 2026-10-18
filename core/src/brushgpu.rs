//! The brush layers on the GPU (docs/phase2d-masks.md, part B): one `Raster` per brush component
//! (masks.rs) on the CPU, kept in step with the photo's strokes, and a texture array holding them
//! for the develop pass (t7), a slice each. Layers are made when a component needs one and freed
//! when it goes; the array grows in powers of two and is freed with its last layer, so a photo
//! without a brush costs nothing.
//!
//! Painting changes the CPU raster a dab at a time and marks the rectangle it changed; the next
//! frame copies just that rectangle into the texture (`record_uploads`).

use std::cell::RefCell;
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Result;

use crate::gpu::{Gpu, new_buffer};
use crate::masks::{Masks, Raster, Stroke, StrokeMap, brush_ids, layer_dims};

const FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16_UNORM;
/// Room for changed rectangles in one frame: a whole 2048 × 2048 layer is 8 MB.
const UPLOAD_BYTES: u64 = 16 << 20;
const READABLE: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;

struct Layer {
    id: u32,
    raster: Raster,
    /// The strokes this raster shows (None: none yet), and how many there are.
    source: Option<Arc<Vec<Stroke>>>,
    strokes: usize,
    /// Painted on since `source` (a stroke under way): it holds one more stroke than `strokes`.
    live: bool,
}

struct Inner {
    /// The layers' size (`layer_dims` of the photo's frame); 0 × 0 with no layers.
    dims: (u32, u32),
    layers: Vec<Option<Layer>>,
    texture: Option<ID3D12Resource>,
    /// Slices the texture has.
    capacity: u32,
    upload: Option<(ID3D12Resource, *mut u8)>,
}

pub struct BrushGpu {
    inner: RefCell<Inner>,
}

impl Default for BrushGpu {
    fn default() -> Self {
        Self::new()
    }
}

impl BrushGpu {
    pub fn new() -> BrushGpu {
        BrushGpu { inner: RefCell::new(Inner { dims: (0, 0), layers: Vec::new(), texture: None, capacity: 0, upload: None }) }
    }

    /// Whether a layer has changes not yet copied to the texture.
    pub fn has_dirty(&self) -> bool {
        self.inner.borrow().layers.iter().flatten().any(|l| l.raster.is_dirty())
    }

    /// Whether any layer is held.
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().layers.iter().all(|l| l.is_none())
    }

    /// Makes the layers the brush components of `masks` need, from `strokes`, for a photo whose
    /// frame is `frame` pixels; frees the others. A layer that already shows its strokes is kept
    /// (and one painted on live that now has its stroke committed as well).
    pub fn sync(&self, gpu: &Gpu, frame: (u32, u32), masks: &Masks, strokes: &StrokeMap) -> Result<()> {
        let mut s = self.inner.borrow_mut();
        let ids = brush_ids(masks);
        let dims = layer_dims(frame);
        if ids.is_empty() || dims != s.dims {
            s.layers.clear();
            s.texture = None;
            s.capacity = 0;
            s.dims = if ids.is_empty() { (0, 0) } else { dims };
        }
        for slot in &mut s.layers {
            if slot.as_ref().is_some_and(|l| !ids.contains(&l.id)) {
                *slot = None;
            }
        }
        let empty = Arc::new(Vec::new());
        for &id in &ids {
            let wanted = strokes.get(&id).unwrap_or(&empty);
            let index = s.layers.iter().position(|l| l.as_ref().is_some_and(|l| l.id == id));
            match index {
                Some(i) => {
                    let l = s.layers[i].as_mut().unwrap();
                    let same = l.source.as_ref().is_some_and(|src| Arc::ptr_eq(src, wanted)) || (l.strokes == 0 && wanted.is_empty());
                    if same {
                        continue;
                    }
                    if l.live && wanted.len() == l.strokes + 1 {
                        // The stroke under way is committed: the layer already shows it.
                        l.source = Some(wanted.clone());
                        l.strokes = wanted.len();
                        l.live = false;
                    } else if l.live && wanted.len() == l.strokes {
                        // The same strokes in another vector (another brush's were dropped): keep what is being painted.
                        l.source = Some(wanted.clone());
                    } else {
                        l.raster = Raster::rasterize(frame, wanted);
                        l.source = Some(wanted.clone());
                        l.strokes = wanted.len();
                        l.live = false;
                    }
                }
                None => {
                    let layer = Layer { id, raster: Raster::rasterize(frame, wanted), source: Some(wanted.clone()), strokes: wanted.len(), live: false };
                    match s.layers.iter().position(|l| l.is_none()) {
                        Some(i) => s.layers[i] = Some(layer),
                        None => s.layers.push(Some(layer)),
                    }
                }
            }
        }
        while s.layers.last().is_some_and(|l| l.is_none()) {
            s.layers.pop();
        }
        let needed = (s.layers.len() as u32).next_power_of_two().max(1);
        if !s.layers.is_empty() && (s.texture.is_none() || s.capacity != needed) {
            let (w, h) = s.dims;
            s.texture = Some(new_array(gpu, w, h, needed)?);
            s.capacity = needed;
            for l in s.layers.iter_mut().flatten() {
                l.raster.mark_all_dirty();
            }
        }
        if s.layers.is_empty() {
            s.texture = None;
            s.capacity = 0;
        }
        if s.upload.is_none() && !s.layers.is_empty() {
            let buffer = new_buffer(&gpu.device, UPLOAD_BYTES, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let mut ptr = std::ptr::null_mut();
            unsafe { buffer.Map(0, Some(&D3D12_RANGE::default()), Some(&mut ptr))? };
            s.upload = Some((buffer, ptr as *mut u8));
        }
        Ok(())
    }

    /// Paints dabs into the layer of brush `id` (a stroke under way; the layer must be there, see
    /// [`sync`](Self::sync)).
    pub fn paint(&self, id: u32, stroke: &Stroke, dabs: &[[f32; 2]]) {
        let mut s = self.inner.borrow_mut();
        let Some(l) = s.layers.iter_mut().flatten().find(|l| l.id == id) else { return };
        for d in dabs {
            l.raster.stamp(*d, stroke.radius, stroke.feather, stroke.flow, stroke.density, stroke.erase);
        }
        l.live = true;
    }

    /// The slice holding brush `id`'s layer.
    pub fn slice_of(&self, id: u32) -> Option<u32> {
        self.inner.borrow().layers.iter().position(|l| l.as_ref().is_some_and(|l| l.id == id)).map(|i| i as u32)
    }

    /// A layer's value at `uv`, as the shader samples it (for checks).
    pub fn sample(&self, slice: u32, uv: [f32; 2]) -> f32 {
        self.inner.borrow().layers.get(slice as usize).and_then(|l| l.as_ref()).map_or(0.0, |l| l.raster.sample(uv))
    }

    /// Bytes of CPU rasters and GPU texture held now.
    pub fn memory(&self) -> (u64, u64) {
        let s = self.inner.borrow();
        let layer = s.dims.0 as u64 * s.dims.1 as u64 * 2;
        (layer * s.layers.iter().flatten().count() as u64, layer * s.capacity as u64)
    }

    /// Records copying what changed in the layers into the texture (between `gpu.begin` and
    /// `gpu.submit`, ahead of the develop pass). Returns whether changes remain that didn't fit in
    /// this frame's upload memory.
    pub fn record_uploads(&self, gpu: &Gpu) -> bool {
        let mut s = self.inner.borrow_mut();
        let Some(texture) = s.texture.clone() else { return false };
        let Some((buffer, ptr)) = s.upload.clone() else { return false };
        let mut used = 0u64;
        let mut more = false;
        for (slice, layer) in s.layers.iter_mut().enumerate() {
            let Some(l) = layer else { continue };
            let Some([x0, y0, x1, y1]) = l.raster.take_dirty() else { continue };
            let (rw, rh) = ((x1 - x0) as usize, (y1 - y0) as usize);
            let pitch = (rw * 2).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as usize);
            let offset = used.next_multiple_of(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64);
            if offset + (pitch * rh) as u64 > UPLOAD_BYTES {
                // Keep it for the next frame (the rectangle is what it was).
                l.raster.mark_rect_dirty([x0, y0, x1, y1]);
                more = true;
                continue;
            }
            for row in 0..rh {
                let src = &l.raster.data[(y0 as usize + row) * l.raster.width as usize + x0 as usize..][..rw];
                // SAFETY: the buffer holds UPLOAD_BYTES and this row ends inside it (checked above).
                unsafe { std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, ptr.add(offset as usize + row * pitch), rw * 2) };
            }
            used = offset + (pitch * rh) as u64;
            gpu.transition(&texture, slice as u32, READABLE, D3D12_RESOURCE_STATE_COPY_DEST);
            unsafe {
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: slice as u32 },
                };
                let src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&buffer),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: offset,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT { Format: FORMAT, Width: rw as u32, Height: rh as u32, Depth: 1, RowPitch: pitch as u32 },
                        },
                    },
                };
                gpu.list.CopyTextureRegion(&dst, x0, y0, 0, &src, None);
            }
            gpu.transition(&texture, slice as u32, D3D12_RESOURCE_STATE_COPY_DEST, READABLE);
        }
        more
    }

    /// Uploads every change now, in submissions of its own (an export's layers, or many at once).
    pub fn upload_now(&self, gpu: &mut Gpu) -> Result<()> {
        loop {
            gpu.begin()?;
            let more = self.record_uploads(gpu);
            gpu.submit()?;
            if !more {
                return Ok(());
            }
        }
    }

    /// Points table slot `slot` at the texture array (or at nothing, with no layers).
    pub fn srv(&self, gpu: &Gpu, slot: u32) {
        let s = self.inner.borrow();
        match &s.texture {
            Some(t) => gpu.srv_texture_array(Some(t), FORMAT, s.capacity, slot),
            None => gpu.srv_texture_array(None, FORMAT, 1, slot),
        }
    }
}

fn new_array(gpu: &Gpu, width: u32, height: u32, slices: u32) -> Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: width as u64,
        Height: height,
        DepthOrArraySize: slices as u16,
        MipLevels: 1,
        Format: FORMAT,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    let props = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
    let mut resource: Option<ID3D12Resource> = None;
    unsafe { gpu.device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, READABLE, None, &mut resource)? };
    Ok(resource.unwrap())
}
