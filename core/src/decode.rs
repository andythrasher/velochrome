//! Turns a raw file into a `RawFrame`: the sensor data plus everything the GPU pipeline needs to
//! develop it. This is the only module that knows about rawler, so the decoder can be swapped or
//! supplemented (e.g. our own fast paths for ORF and RW2) without touching the pipeline.

use std::path::Path;
use std::time::{Duration, Instant};

use rawler::RawImageData;
use rawler::decoders::RawDecodeParams;
use rawler::imgop::xyz::Illuminant;
use rawler::rawimage::RawPhotometricInterpretation;
use rawler::rawsource::RawSource;

use crate::color::{self, Mat3};

/// Clockwise rotation (and mirroring) to apply when displaying, from the file's metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl Orientation {
    /// Whether width and height swap on screen.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Orientation::Rotate90 | Orientation::Rotate270)
    }
}

/// A decoded Bayer frame, cropped to the area that should be shown.
pub struct RawFrame {
    pub make: String,
    pub model: String,
    /// Sensor values for the cropped area, row by row.
    pub data: Vec<u16>,
    pub width: u32,
    pub height: u32,
    /// Which color (0 = red, 1 = green, 2 = blue) sits at each position of the 2×2 pattern,
    /// indexed `[row % 2][col % 2]`, relative to the cropped area's top-left corner.
    pub cfa: [[u8; 2]; 2],
    /// Black level per position of the 2×2 pattern, same indexing as `cfa`.
    pub black: [[f32; 2]; 2],
    pub white: f32,
    /// As-shot white balance multipliers (R, G, B), normalized so G = 1.
    pub wb: [f32; 3],
    /// Camera RGB (white balanced) to linear Rec.2020, rows summing to 1 so white stays white.
    pub cam_to_working: Mat3,
    pub orientation: Orientation,
    pub decode_time: Duration,
}

#[derive(Debug)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

fn err(e: impl std::fmt::Display) -> DecodeError {
    DecodeError(e.to_string())
}

pub fn decode_file(path: &Path) -> Result<RawFrame, DecodeError> {
    let start = Instant::now();
    let source = RawSource::new(path).map_err(err)?;
    let raw = rawler::decode(&source, &RawDecodeParams::default()).map_err(err)?;

    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return Err(DecodeError("only Bayer sensors are supported for now".into()));
    };
    if raw.cpp != 1 || !config.cfa.is_rgb() || config.cfa.width != 2 || config.cfa.height != 2 {
        return Err(DecodeError(format!("unsupported sensor layout {}", config.cfa.name)));
    }
    let RawImageData::Integer(full) = &raw.data else {
        return Err(DecodeError("floating-point raw data is not supported yet".into()));
    };

    // Crop to the recommended area if there is one, else the area with real pixels.
    let (x0, y0, w, h) = match raw.crop_area.or(raw.active_area) {
        Some(r) => (r.p.x, r.p.y, r.d.w, r.d.h),
        None => (0, 0, raw.width, raw.height),
    };
    // Keep the crop aligned to the 2×2 pattern so the pattern indexing stays simple.
    let (w, h) = (w & !1, h & !1);
    let mut data = Vec::with_capacity(w * h);
    for row in y0..y0 + h {
        let start = row * raw.width + x0;
        data.extend_from_slice(&full[start..start + w]);
    }

    let mut cfa = [[0u8; 2]; 2];
    for (r, row) in cfa.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            *cell = config.cfa.color_at(y0 + r, x0 + c) as u8;
        }
    }

    // Black levels come either as one value or one per pattern position (in pattern order).
    let levels = raw.blacklevel.as_vec();
    let bw = raw.blacklevel.width.max(1);
    let bh = raw.blacklevel.height.max(1);
    let mut black = [[0f32; 2]; 2];
    for (r, row) in black.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            let idx = ((y0 + r) % bh) * bw + (x0 + c) % bw;
            *cell = levels.get(idx).or(levels.first()).copied().unwrap_or(0.0);
        }
    }
    let white = raw.whitelevel.0.first().copied().unwrap_or(u16::MAX as u32) as f32;

    let wb = if raw.wb_coeffs[1].is_finite() && raw.wb_coeffs[1] > 0.0 {
        let g = raw.wb_coeffs[1];
        [raw.wb_coeffs[0] / g, 1.0, raw.wb_coeffs[2] / g]
    } else {
        [2.0, 1.0, 1.5]
    };

    let xyz_to_cam = raw
        .color_matrix
        .get(&Illuminant::D65)
        .or_else(|| raw.color_matrix.values().next())
        .filter(|m| m.len() >= 9)
        .map(|m| [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]])
        .unwrap_or_else(|| {
            let m = raw.xyz_to_cam;
            [m[0], m[1], m[2]]
        });

    // rawler leaves orientation unset for ORF and RW2; both keep the standard tag in IFD0.
    let orientation = match tiff_orientation(source.buf()).unwrap_or(raw.orientation) {
        rawler::Orientation::Rotate90 => Orientation::Rotate90,
        rawler::Orientation::Rotate180 => Orientation::Rotate180,
        rawler::Orientation::Rotate270 => Orientation::Rotate270,
        _ => Orientation::Normal,
    };

    Ok(RawFrame {
        make: raw.clean_make.clone(),
        model: raw.clean_model.clone(),
        data,
        width: w as u32,
        height: h as u32,
        cfa,
        black,
        white,
        wb,
        cam_to_working: color::camera_to_working(xyz_to_cam),
        orientation,
        decode_time: start.elapsed(),
    })
}

/// The JPEG the camera embedded in the raw file, decoded. This is what culling shows.
pub struct Preview {
    pub width: u32,
    pub height: u32,
    pub time: Duration,
}

pub fn decode_preview(path: &Path) -> Result<Preview, DecodeError> {
    let start = Instant::now();
    let source = RawSource::new(path).map_err(err)?;
    // rawler knows where most makers keep the preview (Panasonic's is found this way); for the
    // rest (Olympus keeps it in its maker notes), look for the biggest JPEG in the file.
    let params = RawDecodeParams::default();
    let known = rawler::get_decoder(&source)
        .ok()
        .and_then(|d| d.full_image(&source, &params).ok().flatten().or_else(|| d.preview_image(&source, &params).ok().flatten()));
    let image = match known {
        Some(image) => image,
        None => {
            let bytes = source.buf();
            let jpeg = largest_embedded_jpeg(bytes).ok_or_else(|| DecodeError("no embedded preview".into()))?;
            image::load_from_memory_with_format(&bytes[jpeg.offset..], image::ImageFormat::Jpeg).map_err(err)?
        }
    };
    Ok(Preview { width: image.width(), height: image.height(), time: start.elapsed() })
}

/// Where a JPEG sits inside a raw file, and its size.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedJpeg {
    pub offset: usize,
    pub width: u32,
    pub height: u32,
}

/// Finds the biggest JPEG embedded in a raw file, by its frame header. Cameras keep their
/// previews in different places (Olympus in its maker notes, Panasonic in a JpgFromRaw tag), but
/// they're all ordinary JPEGs, so a scan finds them without knowing each maker's layout.
pub fn largest_embedded_jpeg(bytes: &[u8]) -> Option<EmbeddedJpeg> {
    let mut best: Option<EmbeddedJpeg> = None;
    let mut i = 0;
    let finder = memchr::memmem::Finder::new(&[0xFF, 0xD8, 0xFF]);
    while let Some(pos) = finder.find(&bytes[i..]) {
        let soi = i + pos;
        if let Some((width, height)) = jpeg_dimensions(&bytes[soi..]) {
            let area = width as u64 * height as u64;
            if best.is_none_or(|b| area > b.width as u64 * b.height as u64) {
                best = Some(EmbeddedJpeg { offset: soi, width, height });
            }
        }
        i = soi + 3;
    }
    best
}

/// Walks the JPEG's marker segments up to the scan, returning the frame size.
fn jpeg_dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut j = 2;
    while j + 4 <= jpeg.len() && jpeg[j] == 0xFF {
        let marker = jpeg[j + 1];
        let len = u16::from_be_bytes([jpeg[j + 2], jpeg[j + 3]]) as usize;
        if matches!(marker, 0xC0..=0xC3) && j + 9 <= jpeg.len() {
            let h = u16::from_be_bytes([jpeg[j + 5], jpeg[j + 6]]) as u32;
            let w = u16::from_be_bytes([jpeg[j + 7], jpeg[j + 8]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        if marker == 0xDA || len < 2 {
            return None;
        }
        j += 2 + len;
    }
    None
}

/// The TIFF Orientation tag (0x0112) from the first directory. ORF and RW2 are TIFF variants
/// that differ only in their magic number, so this reads both.
fn tiff_orientation(bytes: &[u8]) -> Option<rawler::Orientation> {
    let little = match bytes.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    (0..count.min(512)).find_map(|i| {
        let entry = ifd + 2 + i * 12;
        (u16_at(entry)? == 0x0112).then(|| u16_at(entry + 8).map(rawler::Orientation::from_u16))?
    })
}
