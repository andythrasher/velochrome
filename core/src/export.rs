//! Full-size export. The develop pass runs over the photo in tiles of `TILE` × `TILE` pixels,
//! each an exact 1:1 window (`Zoom::Region`) of the frame (the rotated, straightened photo), so a tile needs no more
//! memory than a screen-sized view. Tiles are read back and arranged into bands of rows, and
//! each finished band goes to an encoder thread while the next is rendered. The photo is never
//! held in memory whole: at most a few bands are (about 32 MB each at 24 megapixels).
//!
//! Tone statistics and the base layer are worked out per image, not per tile, so tiles join
//! without a seam (checked by `bench export`).

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use rayon::prelude::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Result;

use crate::exif;
use crate::geometry;
use crate::gpu::Gpu;
use crate::jpeg;
use crate::masks::Masks;
use crate::pipeline::{Edit, Image, Layer, MaskInput, Target, Zoom, render_masked};

/// Tile size in pixels. Four megapixels: big enough that per-tile work is small, small enough
/// that the target and the readback buffer are 16 MB each.
pub const TILE: u32 = 2048;

/// A straightened photo's tiles cover a turned square, so the detail pass would need a bigger region
/// around each; they are smaller to keep that within the memory the pass has.
const TURNED_TILE: u32 = 1024;

/// The tile size for an edit.
pub fn tile_size(edit: &Edit) -> u32 {
    if edit.angle != 0.0 && !edit.detail().is_neutral() { TURNED_TILE } else { TILE }
}

pub const DEFAULT_QUALITY: u8 = 92;

/// What to export, besides the adjustments (which carry the crop, rotation and angle) and the photo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request {
    /// JPEG quality, 1–100.
    pub quality: u8,
}

impl Default for Request {
    fn default() -> Self {
        Request { quality: DEFAULT_QUALITY }
    }
}

/// A rectangle of the frame, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The pixels an edit keeps: its crop in whole pixels of the frame, which is the photo (`photo`
/// is its size after the camera's orientation) turned by the edit's rotation. The output is
/// that size, and the angle is applied by the render, not here.
pub fn window(photo: (u32, u32), edit: &Edit) -> Window {
    let frame = geometry::frame_size(photo, edit.rotation);
    let edge = |from: f32, to: f32, size: u32| {
        let a = (from * size as f32).round().clamp(0.0, size as f32 - 1.0) as u32;
        let b = (to * size as f32).round().clamp(a as f32 + 1.0, size as f32) as u32;
        (a, b - a)
    };
    let [l, t, r, b] = edit.crop;
    let ((x, width), (y, height)) = (edge(l, r, frame.0), edge(t, b, frame.1));
    Window { x, y, width, height }
}

/// A band of finished rows: 8-bit BGR with no padding, as the encoder takes it.
pub struct Strip {
    pub bgr: Vec<u8>,
    pub rows: u32,
}

/// Renders a window of a developed photo one tile at a time, so a caller can do other work
/// (draw a frame) between tiles.
pub struct TileRenderer {
    target: Target,
    readback: ID3D12Resource,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    window: Window,
    tile: u32,
    /// The next tile, in tiles from the window's top-left.
    col: u32,
    row: u32,
    strip: Vec<u8>,
    /// GPU time developing the tiles, and time reading them back and arranging them.
    pub render_ms: f64,
    pub readback_ms: f64,
}

impl TileRenderer {
    pub fn new(gpu: &Gpu, window: Window, edit: &Edit) -> Result<TileRenderer> {
        let tile = tile_size(edit);
        let target = Target::new(gpu, tile.min(window.width), tile.min(window.height))?;
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        unsafe {
            gpu.device.GetCopyableFootprints(&target.texture.GetDesc(), 0, 1, 0, Some(&mut footprint), None, None, Some(&mut total))
        };
        let readback = gpu.buffer(total, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
        Ok(TileRenderer { target, readback, footprint, window, tile, col: 0, row: 0, strip: Vec::new(), render_ms: 0.0, readback_ms: 0.0 })
    }

    pub fn tiles(&self) -> u32 {
        self.window.width.div_ceil(self.tile) * self.window.height.div_ceil(self.tile)
    }

    pub fn tiles_done(&self) -> u32 {
        self.row * self.window.width.div_ceil(self.tile) + self.col
    }

    pub fn done(&self) -> bool {
        self.tiles_done() >= self.tiles()
    }

    /// Develops the next tile. Returns the band of rows it completed, if it was the band's last.
    pub fn step(&mut self, gpu: &mut Gpu, image: &Image, edit: &Edit) -> Result<Option<Strip>> {
        self.step_masked(gpu, image, edit, &Masks::NONE)
    }

    /// Like [`step`](Self::step), with the photo's masks.
    pub fn step_masked(&mut self, gpu: &mut Gpu, image: &Image, edit: &Edit, masks: &Masks) -> Result<Option<Strip>> {
        if edit.detail().luminance > 0.0 || edit.detail().color > 0.0 {
            image.ensure_noise(gpu)?;
        }
        let w = self.window;
        let (tile, (left, top)) = (self.tile, (self.col * self.tile, self.row * self.tile));
        let (tile_w, tile_h) = (tile.min(w.width - left), tile.min(w.height - top));
        let band_rows = tile.min(w.height - top);
        if self.strip.is_empty() {
            self.strip = vec![0; w.width as usize * band_rows as usize * 3];
        }

        // The target may be bigger than this tile (at the right and bottom edges); the pass
        // only runs over the part that's used.
        let view = Target { texture: self.target.texture.clone(), width: tile_w, height: tile_h };
        let zoom = Zoom::Region { x: (w.x + left) as f32, y: (w.y + top) as f32, scale: 1.0 };
        gpu.begin()?;
        let a = gpu.timestamp();
        // Masks are in frame coordinates, so a tile needs nothing of them but the same set.
        render_masked(gpu, Layer::Raw(image), edit, &MaskInput { masks, overlay: None }, zoom, image.oriented_size(), &view);
        let b = gpu.timestamp();
        self.copy_to_readback(gpu, tile_w, tile_h);
        let c = gpu.timestamp();
        let ts = gpu.submit()?;
        self.render_ms += ts[b as usize] - ts[a as usize];

        let start = Instant::now();
        let (pitch, row_len) = (self.footprint.Footprint.RowPitch as usize, w.width as usize * 3);
        let mut ptr = std::ptr::null_mut();
        unsafe { self.readback.Map(0, None, Some(&mut ptr))? };
        // SAFETY: the readback buffer holds a whole target, `pitch` bytes per row, and the GPU
        // is done with it (submit waited).
        let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u8, pitch * tile_h as usize) };
        self.strip.par_chunks_mut(row_len).take(tile_h as usize).enumerate().for_each(|(y, out)| {
            let (src, _) = pixels[y * pitch..y * pitch + tile_w as usize * 4].as_chunks::<4>();
            for (d, s) in out[left as usize * 3..].as_chunks_mut::<3>().0.iter_mut().zip(src) {
                *d = [s[2], s[1], s[0]];
            }
        });
        unsafe { self.readback.Unmap(0, Some(&D3D12_RANGE::default())) };
        self.readback_ms += ts[c as usize] - ts[b as usize] + start.elapsed().as_secs_f64() * 1000.0;

        self.col += 1;
        if self.col * tile < w.width {
            return Ok(None);
        }
        self.col = 0;
        self.row += 1;
        Ok(Some(Strip { bgr: std::mem::take(&mut self.strip), rows: band_rows }))
    }

    /// Records copying the top-left `w` × `h` of the target into the readback buffer.
    fn copy_to_readback(&self, gpu: &Gpu, w: u32, h: u32) {
        let (uav, src_state) = (D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        gpu.transition(&self.target.texture, 0, uav, src_state);
        unsafe {
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: self.footprint },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&self.target.texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let region = D3D12_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
            gpu.list.CopyTextureRegion(&dst, 0, 0, 0, &src, Some(&region));
        }
        gpu.transition(&self.target.texture, 0, src_state, uav);
    }
}

/// What the encoder thread made.
#[derive(Debug, Clone)]
pub struct Encoded {
    /// Where the file ended up: the requested path, or one with -2, -3, … if that was taken.
    pub path: PathBuf,
    /// Time spent encoding (not waiting for bands), and writing the file.
    pub encode_ms: f64,
    pub write_ms: f64,
    pub bytes: usize,
    /// Whether the camera's EXIF was copied in.
    pub exif: bool,
}

/// Encodes the bands that arrive on `strips` and writes the file. Run on its own thread. The
/// camera's EXIF is copied from `raw` (the photo's raw file).
pub fn encode(
    strips: Receiver<Strip>,
    size: (u32, u32),
    quality: u8,
    raw: &Path,
    path: &Path,
) -> std::result::Result<Encoded, String> {
    let e = |err: windows::core::Error| format!("couldn't encode the JPEG: {err}");
    let mut encoder = jpeg::Encoder::new(size.0, size.1, quality).map_err(e)?;
    let mut encode_ms = 0.0;
    let mut rows = 0;
    for strip in strips {
        let start = Instant::now();
        encoder.write_rows(&strip.bgr, strip.rows).map_err(e)?;
        encode_ms += start.elapsed().as_secs_f64() * 1000.0;
        rows += strip.rows;
    }
    if rows != size.1 {
        return Err("the export was cut short".into());
    }
    let start = Instant::now();
    let bytes = encoder.finish().map_err(e)?;
    encode_ms += start.elapsed().as_secs_f64() * 1000.0;

    let start = Instant::now();
    let exif = exif::for_export(raw, size.0, size.1);
    let bytes = match &exif {
        Some(block) => exif::insert(&bytes, block),
        None => bytes,
    };
    let path = write_new_file(path, &bytes).map_err(|err| format!("couldn't save {}: {err}", path.display()))?;
    Ok(Encoded { path, encode_ms, write_ms: start.elapsed().as_secs_f64() * 1000.0, bytes: bytes.len(), exif: exif.is_some() })
}

/// Writes `bytes` to `path` without replacing anything: if the name is taken it becomes
/// `name-2.jpg`, `name-3.jpg`, … Written to a temporary file and swapped in, so a crash can't
/// leave half a JPEG under the final name. Returns the path used.
pub fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("jpg.windedit-tmp");
    std::fs::write(&temp, bytes)?;
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 1.. {
        let candidate = if n == 1 { path.to_path_buf() } else { path.with_file_name(format!("{stem}-{n}{ext}")) };
        // Claim the name first, so two exports never pick the same one, then swap the file in.
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(_) => {
                return match std::fs::rename(&temp, &candidate) {
                    Ok(()) => Ok(candidate),
                    Err(e) => {
                        let _ = std::fs::remove_file(&candidate);
                        let _ = std::fs::remove_file(&temp);
                        Err(e)
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(e);
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_defaults_to_the_whole_photo_and_crops_to_whole_pixels() {
        let edit = Edit::default();
        assert_eq!(window((5184, 3888), &edit), Window { x: 0, y: 0, width: 5184, height: 3888 });
        let edit = Edit { crop: [0.25, 0.5, 0.75, 1.0], ..Default::default() };
        assert_eq!(window((4000, 3000), &edit), Window { x: 1000, y: 1500, width: 2000, height: 1500 });
        // A sliver stays a pixel wide, and a crop past the edge is held inside the photo.
        let edit = Edit { crop: [0.999, 0.9, 1.5, 1.0], ..Default::default() };
        assert_eq!(window((100, 100), &edit), Window { x: 99, y: 90, width: 1, height: 10 });
        // The frame is the turned photo: a quarter turn swaps the sides.
        let edit = Edit { rotation: 1, crop: [0.0, 0.0, 0.5, 1.0], ..Default::default() };
        assert_eq!(window((6000, 4000), &edit), Window { x: 0, y: 0, width: 2000, height: 6000 });
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("windedit-export-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("photo.jpg");
        assert_eq!(write_new_file(&path, b"one").unwrap(), path);
        assert_eq!(write_new_file(&path, b"two").unwrap(), dir.join("photo-2.jpg"));
        assert_eq!(write_new_file(&path, b"three").unwrap(), dir.join("photo-3.jpg"));
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        assert_eq!(std::fs::read(dir.join("photo-3.jpg")).unwrap(), b"three");
        assert!(!dir.join("photo.jpg.windedit-tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
