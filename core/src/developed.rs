//! Developed previews (docs/edited-previews.md): our own render of an edited photo, kept on disk,
//! so the filmstrip, the grid and the fitted Cull view show the edit without developing the raw.
//!
//! A preview's file name says exactly what it shows: a hash of the raw (path, size, modified
//! time) and a fingerprint of everything that changes the render (the edit, the masks, the brush
//! strokes and `RENDER_VERSION`). So an entry is valid while its name matches the photo's current
//! one; nothing is ever invalidated, and entries that no longer match age out of the cache by
//! least recent use.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use rayon::prelude::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Result;

use crate::brushgpu::BrushGpu;
use crate::decode::{DecodeError, Orientation};
use crate::develop::Edit;
use crate::export::{self, copy_to_readback};
use crate::gpu::Gpu;
use crate::jobs::Thumbnail;
use crate::jpeg::{self, Pixels};
use crate::masks::{Masks, StrokeMap};
use crate::pipeline::{Image, Layer, MaskInput, Target, Zoom, render_masked};
use crate::preview::{Preview, Source};
use crate::xmp::Metadata;

/// Bumped whenever the pipeline's look changes, so previews made by the old one are not used.
pub const RENDER_VERSION: u32 = 1;
/// A developed preview's longer side, in pixels: enough for a fitted 4K view.
pub const PREVIEW_EDGE: u32 = 2560;
pub const PREVIEW_QUALITY: u8 = 90;
/// The live thumbnail is rendered at this size and shrunk to the thumbnail's (a smaller render
/// costs the GPU thread less than one at 2560).
pub const LIVE_EDGE: u32 = 512;
/// The cache's size limit.
pub const CACHE_LIMIT: u64 = 2 << 30;

/// Whether a photo counts as edited: any adjustment that isn't the default, or any mask.
pub fn is_edited(meta: &Metadata) -> bool {
    !meta.edit.is_default() || meta.masks.is_some()
}

// ---- Keys ----

/// A 128-bit hash made of two FNV-1a streams, fed by `write!`. Nothing here is secret; it only
/// has to tell edits apart, and to give the same answer in every run.
struct Hasher128(u64, u64);

impl Hasher128 {
    fn new() -> Hasher128 {
        Hasher128(0xcbf2_9ce4_8422_2325, 0x8422_2325_cbf2_9ce4)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            self.1 = (self.1 ^ b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15 | 1).rotate_left(5);
        }
    }

    fn finish(&self) -> u128 {
        (self.0 as u128) << 64 | self.1 as u128
    }
}

impl std::fmt::Write for Hasher128 {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.bytes(s.as_bytes());
        Ok(())
    }
}

/// What identifies the photo's raw: its path, size and modified time. None if it can't be read.
pub fn photo_id(raw: &Path) -> Option<u64> {
    let meta = std::fs::metadata(raw).ok()?;
    let modified = meta.modified().ok()?.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    let mut h = Hasher128::new();
    // Windows paths are not case sensitive.
    let _ = write!(h, "{}|{}|{}", raw.to_string_lossy().to_lowercase(), meta.len(), modified.as_nanos());
    Some((h.finish() >> 64) as u64)
}

/// Everything that changes what a photo looks like: not its stars, label or pick.
pub fn fingerprint(edit: &Edit, masks: &Masks, strokes: &StrokeMap) -> u128 {
    let mut h = Hasher128::new();
    let _ = write!(h, "v{RENDER_VERSION}|{edit:?}|{:?}", masks.masks());
    for (id, list) in strokes {
        let _ = write!(h, "|{id}");
        for s in list.iter() {
            let _ = write!(h, ":{}/{}/{}/{}/{}", s.erase, s.radius, s.feather, s.flow, s.density);
            for p in &s.points {
                h.bytes(&p[0].to_le_bytes());
                h.bytes(&p[1].to_le_bytes());
            }
        }
    }
    h.finish()
}

/// The fingerprint of a photo's metadata, or None when it has no edit to develop.
pub fn fingerprint_of(meta: &Metadata) -> Option<u128> {
    if !is_edited(meta) {
        return None;
    }
    let none = StrokeMap::new();
    Some(fingerprint(&meta.edit, meta.masks.as_deref().unwrap_or(&Masks::NONE), meta.strokes.as_deref().unwrap_or(&none)))
}

/// A cache entry's name.
pub type Key = String;

pub fn key(photo: u64, fingerprint: u128) -> Key {
    format!("{photo:016x}-{fingerprint:032x}")
}

/// Whether a file name's stem is a [`key`]: 16 hex digits, a dash, 32 hex digits.
fn is_key(stem: &str) -> bool {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    stem.split_once('-').is_some_and(|(a, b)| hex(a, 16) && hex(b, 32))
}

// ---- The cache ----

/// The cache directory: `%LOCALAPPDATA%\Windedit\Previews`.
pub fn default_dir() -> PathBuf {
    // For measuring with an empty cache, or one somewhere else.
    if let Some(dir) = std::env::var_os("WINDEDIT_PREVIEW_CACHE") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Windedit").join("Previews")
}

/// The previews on disk. Each entry is two files: `<key>.jpg` (the preview) and `<key>.thumb.jpg`.
pub struct Cache {
    dir: PathBuf,
    limit: u64,
    /// What the files add up to, as far as this run knows: counted at the start, then added to.
    bytes: AtomicU64,
}

/// A file is touched (its modified time set to now, which is what the least-recently-used order
/// follows) when a read finds it older than this.
const TOUCH_AFTER: Duration = Duration::from_secs(600);
/// A temporary file this old belongs to a write that died.
const STALE_TEMP: Duration = Duration::from_secs(3600);

static WRITES: AtomicU64 = AtomicU64::new(0);

impl Cache {
    pub fn new(dir: PathBuf, limit: u64) -> Cache {
        Cache { dir, limit, bytes: AtomicU64::new(0) }
    }

    /// The user's cache, with its size counted and kept under the limit (call it off the UI
    /// thread: it lists the directory).
    pub fn open_default() -> Cache {
        let cache = Cache::new(default_dir(), CACHE_LIMIT);
        cache.evict();
        cache
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn preview_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.jpg"))
    }

    fn thumb_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.thumb.jpg"))
    }

    /// Whether the entry's preview is there (the thumbnail is written first, so it is too).
    pub fn has(&self, key: &str) -> bool {
        self.preview_path(key).is_file()
    }

    /// The preview's JPEG bytes. A file that isn't a whole JPEG is deleted, and is as good as absent.
    pub fn read_preview(&self, key: &str) -> Option<Vec<u8>> {
        self.read(&self.preview_path(key))
    }

    pub fn read_thumb(&self, key: &str) -> Option<Vec<u8>> {
        self.read(&self.thumb_path(key))
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        let bytes = std::fs::read(path).ok()?;
        if !is_whole_jpeg(&bytes) {
            let _ = std::fs::remove_file(path);
            return None;
        }
        touch(path);
        Some(bytes)
    }

    /// Removes an entry (its files turned out to be unreadable).
    pub fn discard(&self, key: &str) {
        let _ = std::fs::remove_file(self.preview_path(key));
        let _ = std::fs::remove_file(self.thumb_path(key));
    }

    /// Writes an entry. Each file goes to a temporary name and is renamed into place, so a crash
    /// leaves no half-written file under a real name; the thumbnail goes first, so a preview in
    /// place has its thumbnail.
    pub fn write(&self, key: &str, preview: &[u8], thumb: &[u8]) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        self.put(&self.thumb_path(key), thumb)?;
        self.put(&self.preview_path(key), preview)?;
        let total = self.bytes.fetch_add((preview.len() + thumb.len()) as u64, Ordering::Relaxed) + (preview.len() + thumb.len()) as u64;
        if total > self.limit {
            self.evict();
        }
        Ok(())
    }

    fn put(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), WRITES.fetch_add(1, Ordering::Relaxed)));
        let result = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    /// Counts the cache, and deletes the least recently used entries until it fits the limit.
    /// Returns how many entries went.
    pub fn evict(&self) -> usize {
        struct Entry {
            stem: String,
            bytes: u64,
            used: SystemTime,
        }
        let Ok(listing) = std::fs::read_dir(&self.dir) else { return 0 };
        let mut entries: Vec<Entry> = Vec::new();
        let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for file in listing.flatten() {
            let (Ok(meta), name) = (file.metadata(), file.file_name().to_string_lossy().into_owned()) else { continue };
            let used = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if name.ends_with(".tmp") {
                if is_key(name.split('.').next().unwrap_or("")) && used.elapsed().is_ok_and(|age| age > STALE_TEMP) {
                    let _ = std::fs::remove_file(file.path());
                }
                continue;
            }
            let stem = if let Some(s) = name.strip_suffix(".thumb.jpg") {
                s
            } else if let Some(s) = name.strip_suffix(".jpg") {
                s
            } else {
                continue;
            };
            // Only files named as entries are (`key`): the directory can be pointed elsewhere, and
            // nothing else in it, a photo least of all, is ever the cache's to delete.
            if !is_key(stem) {
                continue;
            }
            let i = *at.entry(stem.to_string()).or_insert_with(|| {
                entries.push(Entry { stem: stem.to_string(), bytes: 0, used });
                entries.len() - 1
            });
            entries[i].bytes += meta.len();
            entries[i].used = entries[i].used.max(used);
        }
        let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
        let mut removed = 0;
        if total > self.limit {
            entries.sort_by_key(|e| e.used);
            for e in &entries {
                if total <= self.limit {
                    break;
                }
                let _ = std::fs::remove_file(self.dir.join(format!("{}.jpg", e.stem)));
                let _ = std::fs::remove_file(self.dir.join(format!("{}.thumb.jpg", e.stem)));
                total -= e.bytes;
                removed += 1;
            }
        }
        self.bytes.store(total, Ordering::Relaxed);
        removed
    }

    /// What the cache's files add up to, as far as this run knows.
    pub fn size(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// Sets a file's modified time to now if it's old, so what is used stays out of the way of
/// eviction. Failing is harmless.
fn touch(path: &Path) {
    let old = std::fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > TOUCH_AFTER));
    if old && let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Starts with a JPEG's start marker and ends with its end marker: not cut short.
fn is_whole_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 4 && bytes[..2] == [0xFF, 0xD8] && bytes[bytes.len() - 2..] == [0xFF, 0xD9]
}

/// The state of each photo's preview for a folder, worked out when it opens: the keys of the
/// edited ones and whether the cache has them. Photos without an edit, or without a raw, have none.
pub struct Keyed {
    pub id: Option<u64>,
    pub key: Option<Key>,
    pub cached: bool,
}

/// Works out every photo's key (in parallel: it reads each edited photo's raw's size and date).
pub fn keys_for(photos: &[crate::library::Photo], fingerprints: &[Option<u128>], cache: &Cache) -> Vec<Keyed> {
    photos
        .par_iter()
        .zip(fingerprints.par_iter())
        .map(|(photo, fp)| {
            let id = photo.raw.as_deref().and_then(photo_id);
            let key = id.zip(*fp).map(|(id, fp)| key(id, fp));
            let cached = key.as_deref().is_some_and(|k| cache.has(k));
            Keyed { id, key, cached }
        })
        .collect()
}

// ---- Pictures in and out of the cache ----

/// Encodes 8-bit BGRA pixels as a JPEG (4:4:4, tagged sRGB).
pub fn encode_bgra(width: u32, height: u32, bgra: &[u8], quality: u8) -> Result<Vec<u8>> {
    let mut encoder = jpeg::Encoder::new(width, height, quality)?;
    let band = 64usize;
    let mut bgr = vec![0u8; width as usize * band * 3];
    for rows in bgra.chunks(width as usize * 4 * band) {
        let count = rows.len() / (width as usize * 4);
        for (d, s) in bgr.chunks_exact_mut(3).zip(rows.chunks_exact(4)) {
            d.copy_from_slice(&s[..3]);
        }
        encoder.write_rows(&bgr, count as u32)?;
    }
    encoder.finish()
}

/// The preview as a `Preview` the viewer can put on the GPU: already upright and cropped.
pub fn preview_from(pixels: Pixels, time: Duration) -> Preview {
    let (source_width, source_height) = (pixels.width, pixels.height);
    Preview { pixels, orientation: Orientation::Normal, source: Source::Developed, source_width, source_height, is_largest: true, time }
}

/// Reads a developed preview from the cache.
pub fn load_preview(cache: &Cache, key: &str) -> std::result::Result<Preview, DecodeError> {
    let start = Instant::now();
    let bytes = cache.read_preview(key).ok_or_else(|| DecodeError("no developed preview".into()))?;
    match jpeg::decode(&bytes, 0) {
        Ok(pixels) => Ok(preview_from(pixels, start.elapsed())),
        Err(e) => {
            cache.discard(key);
            Err(DecodeError(e.to_string()))
        }
    }
}

/// Reads a developed thumbnail from the cache. None if it isn't there or can't be read.
pub fn load_thumb(cache: &Cache, key: &str) -> Option<Thumbnail> {
    let bytes = cache.read_thumb(key)?;
    match jpeg::decode(&bytes, 0) {
        Ok(mut pixels) => {
            // WIC may leave the alpha of a BGR image alone; a bitmap wants it opaque.
            for px in pixels.bgra.chunks_exact_mut(4) {
                px[3] = 255;
            }
            Some(Thumbnail { width: pixels.width, height: pixels.height, bgra: pixels.bgra })
        }
        Err(_) => {
            cache.discard(key);
            None
        }
    }
}

/// What the encoding step makes of a rendered preview: its JPEG, and the thumbnail and its JPEG.
pub struct Encoded {
    pub preview: Vec<u8>,
    pub thumb: Thumbnail,
    pub thumb_jpeg: Vec<u8>,
    pub encode_ms: f64,
}

pub fn encode_preview(p: &Preview, thumb_edge: u32) -> Result<Encoded> {
    let start = Instant::now();
    let thumb = Thumbnail::from_preview(p, thumb_edge);
    let preview = encode_bgra(p.pixels.width, p.pixels.height, &p.pixels.bgra, PREVIEW_QUALITY)?;
    let thumb_jpeg = encode_bgra(thumb.width, thumb.height, &thumb.bgra, 85)?;
    Ok(Encoded { preview, thumb, thumb_jpeg, encode_ms: start.elapsed().as_secs_f64() * 1000.0 })
}

// ---- Rendering ----

/// A rendered picture: 8-bit BGRA, rows packed.
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// GPU time developing, and time copying out and arranging the pixels.
    pub render_ms: f64,
    pub readback_ms: f64,
}

/// Renders edited photos at a preview size on the GPU thread: the same develop pass as Edit mode
/// and export, over the whole crop at once. Its memory is kept between renders.
pub struct PreviewRenderer {
    memory: Option<RenderMemory>,
}

struct RenderMemory {
    edge: u32,
    target: Target,
    readback: ID3D12Resource,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
}

impl Default for PreviewRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// The size the crop is rendered at: its longer side `edge` (it is never made bigger than the
/// photo itself), the shorter side to match, rounded down so the render stays inside the crop.
/// Also the scale from frame pixels.
pub fn output_size(crop: export::Window, edge: u32) -> (u32, u32, f32) {
    let long = crop.width.max(crop.height);
    if long <= edge {
        return (crop.width, crop.height, 1.0);
    }
    let scale = edge as f32 / long as f32;
    let short = |n: u32| ((n as f32 * scale).floor() as u32).max(1);
    if crop.width >= crop.height { (edge, short(crop.height), scale) } else { (short(crop.width), edge, scale) }
}

impl PreviewRenderer {
    pub fn new() -> PreviewRenderer {
        PreviewRenderer { memory: None }
    }

    fn ensure(&mut self, gpu: &Gpu, edge: u32) -> Result<&RenderMemory> {
        if self.memory.as_ref().is_none_or(|m| m.edge < edge) {
            self.memory = None;
            let target = Target::new(gpu, edge, edge)?;
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut total = 0u64;
            unsafe { gpu.device.GetCopyableFootprints(&target.texture.GetDesc(), 0, 1, 0, Some(&mut footprint), None, None, Some(&mut total)) };
            let readback = gpu.buffer(total, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
            self.memory = Some(RenderMemory { edge, target, readback, footprint });
        }
        Ok(self.memory.as_ref().unwrap())
    }

    /// Develops the photo's crop with the given adjustments, fitted to `edge` on its longer side.
    pub fn render(&mut self, gpu: &mut Gpu, image: &Image, edit: &Edit, masks: &Masks, strokes: &StrokeMap, edge: u32) -> Result<Rendered> {
        let photo = image.oriented_size();
        let crop = export::window(photo, edit);
        let (width, height, scale) = output_size(crop, edge);
        if edit.detail().luminance > 0.0 || edit.detail().color > 0.0 {
            image.ensure_noise(gpu)?;
        }
        // Brush layers are rasterised from the strokes each time: a photo without a brush costs nothing.
        let brush = BrushGpu::new();
        brush.sync(gpu, (image.width, image.height), masks, strokes)?;
        brush.upload_now(gpu)?;
        let m = self.ensure(gpu, edge.max(width).max(height))?;
        let view = Target { texture: m.target.texture.clone(), width, height };
        let zoom = Zoom::Region { x: crop.x as f32, y: crop.y as f32, scale };

        gpu.begin()?;
        let a = gpu.timestamp();
        render_masked(gpu, Layer::Raw(image), edit, &MaskInput { masks, overlay: None, brush: Some(&brush) }, zoom, photo, &view);
        let b = gpu.timestamp();
        copy_to_readback(gpu, &m.target.texture, &m.readback, &m.footprint, width, height);
        let c = gpu.timestamp();
        let ts = gpu.submit()?;
        let render_ms = ts[b as usize] - ts[a as usize];

        let start = Instant::now();
        let pitch = m.footprint.Footprint.RowPitch as usize;
        let mut ptr = std::ptr::null_mut();
        unsafe { m.readback.Map(0, None, Some(&mut ptr))? };
        // SAFETY: the readback buffer holds a whole target, `pitch` bytes per row, and the GPU is
        // done with it (submit waited).
        let pixels = unsafe { std::slice::from_raw_parts(ptr as *const u8, pitch * height as usize) };
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        bgra.par_chunks_mut(width as usize * 4).enumerate().for_each(|(y, out)| {
            let src = &pixels[y * pitch..y * pitch + width as usize * 4];
            for (d, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], 255]);
            }
        });
        unsafe { m.readback.Unmap(0, Some(&D3D12_RANGE::default())) };
        let readback_ms = ts[c as usize] - ts[b as usize] + start.elapsed().as_secs_f64() * 1000.0;
        Ok(Rendered { width, height, bgra, render_ms, readback_ms })
    }
}

impl Rendered {
    pub fn into_preview(self) -> Preview {
        preview_from(Pixels { width: self.width, height: self.height, bgra: self.bgra }, Duration::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::masks::{Component, KIND_BRUSH, KIND_RADIAL, Mask, Stroke};
    use std::sync::Arc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("windedit-developed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn print(edit: &Edit, masks: &Masks, strokes: &StrokeMap) -> u128 {
        fingerprint(edit, masks, strokes)
    }

    fn radial() -> Masks {
        let mut m = Masks::default();
        m.masks[0] = Mask::new();
        m.masks[0].components[0] = Component { kind: KIND_RADIAL, p: [0.5, 0.4, 0.2, 0.2, 0.0, 50.0, 0.0, 0.0], ..Default::default() };
        m.masks[0].count = 1;
        m.masks[0].sliders[2] = 0.5;
        m.count = 1;
        m
    }

    #[test]
    fn the_key_changes_with_every_kind_of_edit() {
        let none = StrokeMap::new();
        let base = print(&Edit::default(), &Masks::NONE, &none);
        let mut seen = std::collections::HashSet::from([base]);
        let mut distinct = |what: &str, v: u128| assert!(seen.insert(v), "{what} left the fingerprint alone");

        distinct("a slider", print(&Edit { exposure: 0.1, ..Default::default() }, &Masks::NONE, &none));
        distinct("a smaller slider step", print(&Edit { exposure: 0.01, ..Default::default() }, &Masks::NONE, &none));
        distinct("a detail slider", print(&Edit { sharpen_amount: 5.0, ..Default::default() }, &Masks::NONE, &none));
        distinct("a mixer slider", print(&Edit { mixer: { let mut m = [0.0; crate::develop::MIXER_FIELDS]; m[3] = 10.0; m }, ..Default::default() }, &Masks::NONE, &none));
        distinct("the crop", print(&Edit { crop: [0.1, 0.0, 1.0, 1.0], ..Default::default() }, &Masks::NONE, &none));
        distinct("a rotation", print(&Edit { rotation: 1, ..Default::default() }, &Masks::NONE, &none));
        distinct("the angle", print(&Edit { angle: 1.5, ..Default::default() }, &Masks::NONE, &none));
        distinct("a lens switch", print(&Edit { no_distortion: 1, ..Default::default() }, &Masks::NONE, &none));
        let with_mask = radial();
        distinct("a mask", print(&Edit::default(), &with_mask, &none));
        let mut moved = with_mask;
        moved.masks[0].sliders[2] = 0.6;
        distinct("a mask's slider", print(&Edit::default(), &moved, &none));
        let mut shifted = with_mask;
        shifted.masks[0].components[0].p[0] = 0.51;
        distinct("a mask's shape", print(&Edit::default(), &shifted, &none));

        let mut brush = Masks::default();
        brush.masks[0] = Mask::new();
        brush.masks[0].components[0] = Component { kind: KIND_BRUSH, p: [4.0, 0.3, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0], ..Default::default() };
        brush.masks[0].count = 1;
        brush.count = 1;
        let stroke = |x: f32| {
            let mut s = Stroke::new(false, 0.05, 0.5, 0.5, 1.0);
            s.points = vec![Stroke::quantise([x, 0.3]), Stroke::quantise([x + 0.1, 0.35])];
            s
        };
        let strokes = |list: Vec<Stroke>| -> StrokeMap { StrokeMap::from([(4, Arc::new(list))]) };
        distinct("a brush with no strokes", print(&Edit::default(), &brush, &none));
        distinct("a brush stroke", print(&Edit::default(), &brush, &strokes(vec![stroke(0.3)])));
        distinct("another stroke", print(&Edit::default(), &brush, &strokes(vec![stroke(0.3), stroke(0.5)])));
        distinct("a stroke elsewhere", print(&Edit::default(), &brush, &strokes(vec![stroke(0.31)])));
        let mut erase = stroke(0.3);
        erase.erase = true;
        distinct("an erasing stroke", print(&Edit::default(), &brush, &strokes(vec![erase])));

        // And it is the same every time.
        assert_eq!(base, print(&Edit::default(), &Masks::NONE, &none));
        assert_eq!(print(&Edit::default(), &brush, &strokes(vec![stroke(0.3)])), print(&Edit::default(), &brush, &strokes(vec![stroke(0.3)])));
    }

    #[test]
    fn ratings_labels_and_picks_leave_the_key_alone() {
        let mut meta = Metadata { edit: Edit { exposure: 0.5, ..Default::default() }, ..Default::default() };
        let before = fingerprint_of(&meta).unwrap();
        meta.rating = 4;
        meta.label = Some("Red".into());
        meta.pick = crate::xmp::Pick::Picked;
        assert_eq!(fingerprint_of(&meta), Some(before));
        assert_eq!(fingerprint_of(&Metadata { rating: 3, ..Default::default() }), None, "a photo with no edit has none");
    }

    #[test]
    fn a_photo_is_named_by_its_file() {
        let dir = scratch("id");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("P1.RW2");
        std::fs::write(&raw, b"one").unwrap();
        let first = photo_id(&raw).unwrap();
        assert_eq!(photo_id(&raw), Some(first));
        std::fs::write(&raw, b"changed").unwrap();
        assert_ne!(photo_id(&raw), Some(first), "a changed raw is another photo");
        assert_eq!(photo_id(&dir.join("missing.RW2")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A made-up JPEG: the markers around some bytes, as complete as the cache checks.
    fn jpeg_of(len: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.resize(len - 2, 7);
        v.extend([0xFF, 0xD9]);
        v
    }

    #[test]
    fn entries_are_written_whole_and_read_back() {
        let dir = scratch("rw");
        let cache = Cache::new(dir.clone(), 1 << 20);
        assert!(!cache.has("a"));
        cache.write("a", &jpeg_of(100), &jpeg_of(20)).unwrap();
        assert!(cache.has("a"));
        assert_eq!(cache.read_preview("a").unwrap(), jpeg_of(100));
        assert_eq!(cache.read_thumb("a").unwrap(), jpeg_of(20));
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|f| f.file_name().to_string_lossy().ends_with(".tmp")).collect();
        assert!(leftovers.is_empty(), "no temporary file is left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_or_half_written_file_is_ignored_and_replaced() {
        let dir = scratch("corrupt");
        let cache = Cache::new(dir.clone(), 1 << 20);
        cache.write("a", &jpeg_of(100), &jpeg_of(20)).unwrap();
        // Cut short, as a crash in a naive writer would leave it.
        let half = jpeg_of(100)[..60].to_vec();
        std::fs::write(dir.join("a.jpg"), &half).unwrap();
        assert!(cache.read_preview("a").is_none(), "a truncated file is not served");
        assert!(!cache.has("a"), "and is deleted");
        // Garbage that isn't a JPEG at all.
        std::fs::write(dir.join("a.thumb.jpg"), b"not a jpeg").unwrap();
        assert!(cache.read_thumb("a").is_none());
        // A leftover temporary file from a dead write doesn't count as an entry, and a new write replaces it all.
        std::fs::write(dir.join("a.jpg.99.1.tmp"), b"half").unwrap();
        cache.write("a", &jpeg_of(120), &jpeg_of(30)).unwrap();
        assert_eq!(cache.read_preview("a").unwrap(), jpeg_of(120));
        assert_eq!(cache.read_thumb("a").unwrap(), jpeg_of(30));
        // A real preview that WIC can't decode is dropped by the loader.
        assert!(load_preview(&cache, "a").is_err());
        assert!(!cache.has("a"), "an unreadable preview is discarded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_least_recently_used_entries_go_first() {
        let dir = scratch("lru");
        let cache = Cache::new(dir.clone(), 1000);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        let age = |hours: u64| now - Duration::from_secs(hours * 3600);
        // Entries are named as real keys are; anything else in the directory is never touched.
        let k = |c: char| key(c as u64, c as u128);
        // A photo someone left in the directory: never the cache's to delete, however full it is.
        std::fs::write(dir.join("P1020943.jpg"), jpeg_of(5000)).unwrap();
        // Five entries of 250 bytes (200 + 50), used at different times: 'c' longest ago, then 'a'.
        for (name, hours) in [('a', 4), ('b', 1), ('c', 9), ('d', 2), ('e', 3)] {
            std::fs::write(dir.join(format!("{}.jpg", k(name))), jpeg_of(200)).unwrap();
            std::fs::write(dir.join(format!("{}.thumb.jpg", k(name))), jpeg_of(50)).unwrap();
            for suffix in [".jpg", ".thumb.jpg"] {
                let f = std::fs::OpenOptions::new().write(true).open(dir.join(format!("{}{suffix}", k(name)))).unwrap();
                f.set_modified(age(hours)).unwrap();
            }
        }
        assert_eq!(cache.evict(), 1, "1250 bytes over a limit of 1000 loses the oldest entry");
        assert!(!cache.has(&k('c')) && !dir.join(format!("{}.thumb.jpg", k('c'))).exists(), "both of its files go");
        assert!(['a', 'b', 'd', 'e'].iter().all(|&c| cache.has(&k(c))));
        assert!(dir.join("P1020943.jpg").exists(), "a file that isn't an entry stays");

        // Reading an entry that is old makes it recent again.
        cache.read_preview(&k('a')).unwrap();
        cache.write(&k('f'), &jpeg_of(200), &jpeg_of(50)).unwrap(); // over the limit again: 1250
        assert!(cache.has(&k('f')) && cache.has(&k('a')), "what was just written or read stays");
        assert!(!cache.has(&k('e')), "the oldest of the rest went: {:?}", ['a', 'b', 'd', 'e', 'f'].map(|c| cache.has(&k(c))));
        assert!(dir.join("P1020943.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_preview_size_follows_the_crop() {
        let w = |width, height| export::Window { x: 0, y: 0, width, height };
        assert_eq!(output_size(w(5184, 3888), 2560), (2560, 1920, 2560.0 / 5184.0));
        assert_eq!((output_size(w(3888, 5184), 2560).0, output_size(w(3888, 5184), 2560).1), (1920, 2560));
        assert_eq!(output_size(w(2000, 1000), 2560), (2000, 1000, 1.0), "never bigger than the photo");
        // The shorter side is rounded down, so the render stays inside the crop.
        let (ow, oh, s) = output_size(w(5000, 3333), 2560);
        assert_eq!(ow, 2560);
        assert!(oh as f32 <= 3333.0 * s && oh as f32 + 1.0 > 3333.0 * s);
        assert_eq!(output_size(w(100000, 1), 2560).1, 1);
    }

    #[test]
    fn pictures_survive_the_cache() {
        // 64 × 48 gradient, through the encoder and the loader.
        let (w, h) = (64u32, 48u32);
        let mut bgra = Vec::new();
        for y in 0..h {
            for x in 0..w {
                bgra.extend([(x * 4) as u8, (y * 5) as u8, 128, 255]);
            }
        }
        let p = preview_from(Pixels { width: w, height: h, bgra }, Duration::ZERO);
        let e = encode_preview(&p, 32).unwrap();
        assert_eq!((e.thumb.width, e.thumb.height), (32, 24));
        let dir = scratch("pic");
        let cache = Cache::new(dir.clone(), 1 << 20);
        cache.write("k", &e.preview, &e.thumb_jpeg).unwrap();
        let back = load_preview(&cache, "k").unwrap();
        assert_eq!((back.pixels.width, back.pixels.height), (w, h));
        let err: u32 = back.pixels.bgra.iter().zip(&p.pixels.bgra).enumerate().filter(|(i, _)| i % 4 != 3).map(|(_, (a, b))| a.abs_diff(*b) as u32).sum();
        assert!(err as f32 / (w * h * 3) as f32 <= 3.0, "q90 keeps a gradient within a few levels");
        let thumb = load_thumb(&cache, "k").unwrap();
        assert_eq!((thumb.width, thumb.height), (32, 24));
        assert!(thumb.bgra.chunks(4).all(|px| px[3] == 255));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
