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
    /// The camera's XYZ → camera RGB matrix (D65), for white balance in temperature and tint.
    pub xyz_to_cam: Mat3,
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
    let orientation = to_orientation(tiff_orientation(source.buf()).unwrap_or(raw.orientation));

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
        xyz_to_cam,
        orientation,
        decode_time: start.elapsed(),
    })
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
    embedded_jpegs(bytes).into_iter().max_by_key(|j| j.width as u64 * j.height as u64)
}

/// Every JPEG in the file that has a readable frame header, in file order. A JPEG's own data
/// is skipped, so what's nested inside it (an EXIF thumbnail, or a stub whose header claims a
/// size its data doesn't have, as in Lumix S9 files) isn't taken for a separate image. If a
/// JPEG runs past the end of `bytes`, the scan stops there.
pub fn embedded_jpegs(bytes: &[u8]) -> Vec<EmbeddedJpeg> {
    let mut found = Vec::new();
    let mut i = 0;
    let finder = memchr::memmem::Finder::new(&[0xFF, 0xD8, 0xFF]);
    while let Some(pos) = finder.find(&bytes[i..]) {
        let soi = i + pos;
        let Some((width, height)) = jpeg_dimensions(&bytes[soi..]) else {
            i = soi + 3;
            continue;
        };
        found.push(EmbeddedJpeg { offset: soi, width, height });
        match jpeg_extent(&bytes[soi..], 0) {
            Extent::Complete(len) => i = soi + len,
            Extent::Truncated => break,
            Extent::Invalid => {
                found.pop();
                i = soi + 3;
            }
        }
    }
    found
}

/// Walks the JPEG's marker segments up to the scan, returning the frame size.
pub fn jpeg_dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
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

fn to_orientation(o: rawler::Orientation) -> Orientation {
    match o {
        rawler::Orientation::Rotate90 => Orientation::Rotate90,
        rawler::Orientation::Rotate180 => Orientation::Rotate180,
        rawler::Orientation::Rotate270 => Orientation::Rotate270,
        _ => Orientation::Normal,
    }
}

/// The orientation recorded in a raw file, from its first bytes (Normal if there's none).
pub fn orientation_of(head: &[u8]) -> Orientation {
    tiff_orientation(head).map(to_orientation).unwrap_or_default()
}

/// Where a JPEG ends (just past its end marker), if `jpeg` holds all of it. Only the image data
/// after the scan header is searched, so a thumbnail's end marker inside the EXIF block isn't
/// mistaken for it; `from` skips data already searched by an earlier call.
pub fn jpeg_end(jpeg: &[u8], from: usize) -> Option<usize> {
    match jpeg_extent(jpeg, from) {
        Extent::Complete(len) => Some(len),
        Extent::Truncated | Extent::Invalid => None,
    }
}

/// How much of `jpeg` a JPEG starting at its first byte takes up.
#[derive(Debug, PartialEq, Eq)]
pub enum Extent {
    /// All there: this many bytes, through the end marker.
    Complete(usize),
    /// Well-formed as far as it goes, but it runs past the end of `jpeg`.
    Truncated,
    /// Not a JPEG after all: its marker segments don't chain together (a chance FF D8 FF in
    /// other data, as found in some OM-5 II files).
    Invalid,
}

pub fn jpeg_extent(jpeg: &[u8], from: usize) -> Extent {
    let mut j = 2;
    let data_start = loop {
        if j + 4 > jpeg.len() {
            return Extent::Truncated;
        }
        let marker = jpeg[j + 1];
        // Every segment before the scan starts with FF and a marker byte, then a length of at
        // least 2. Anything else means these bytes were never a JPEG.
        let len = u16::from_be_bytes([jpeg[j + 2], jpeg[j + 3]]) as usize;
        if jpeg[j] != 0xFF || marker == 0x00 || marker == 0xFF || len < 2 {
            return Extent::Invalid;
        }
        if marker == 0xDA {
            break j + 2 + len;
        }
        j += 2 + len;
    };
    let start = from.max(data_start);
    if start >= jpeg.len() {
        return Extent::Truncated;
    }
    match memchr::memmem::find(&jpeg[start..], &[0xFF, 0xD9]) {
        Some(p) => Extent::Complete(start + p + 2),
        None => Extent::Truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal JPEG: frame header with this size, a scan header, some image data, the end.
    fn jpeg(width: u16, height: u16, data: &[u8]) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
        j.extend(height.to_be_bytes());
        j.extend(width.to_be_bytes());
        j.extend([0x03, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        j.extend([0xFF, 0xDA, 0x00, 0x0C, 0x03, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0]);
        j.extend_from_slice(data);
        j.extend([0xFF, 0xD9]);
        j
    }

    #[test]
    fn finds_dimensions_and_end() {
        let j = jpeg(6000, 4000, &[1, 2, 0xFF, 0x00, 3]);
        assert_eq!(jpeg_dimensions(&j), Some((6000, 4000)));
        assert_eq!(jpeg_end(&j, 0), Some(j.len()));
        // Not all there yet: no end found.
        assert_eq!(jpeg_end(&j[..j.len() - 1], 0), None);
    }

    #[test]
    fn skips_jpegs_nested_inside_another() {
        // A preview whose data happens to contain a stub claiming to be full size (as in Lumix
        // S9 files), then a real full-size JPEG after it.
        let stub = jpeg(6000, 4000, &[]);
        let mut preview_data = vec![5u8; 16];
        preview_data.extend(&stub[..stub.len() - 2]); // its end marker would end the preview early
        let mut file = vec![0u8; 100];
        file.extend(jpeg(1920, 1280, &preview_data));
        let full_at = file.len() + 50;
        file.extend(vec![0u8; 50]);
        file.extend(jpeg(6000, 4000, &[7; 64]));

        let found = embedded_jpegs(&file);
        let sizes: Vec<_> = found.iter().map(|j| (j.offset, j.width, j.height)).collect();
        assert_eq!(sizes, vec![(100, 1920, 1280), (full_at, 6000, 4000)]);
    }

    #[test]
    fn skips_a_false_jpeg_start() {
        // FF D8 FF and a frame header by chance, but the segments don't chain (as in some
        // OM-5 II raws), then the real preview.
        let mut file = vec![0u8; 20];
        file.extend([0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x0A, 0xFF, 0x06, 0x00, 0x03, 0x01, 0x00, 0x00, 0x7F, 0x13]);
        file.extend([0x42u8; 40]);
        let real_at = file.len();
        file.extend(jpeg(3200, 2400, &[9; 32]));
        let found: Vec<_> = embedded_jpegs(&file).iter().map(|j| (j.offset, j.width)).collect();
        assert_eq!(found, vec![(real_at, 3200)]);
    }

    #[test]
    fn orientation_from_tiff_header() {
        // Little-endian TIFF, IFD0 at 8 with one entry: Orientation = 6 (rotate 90° clockwise).
        let mut tiff = b"II*\0".to_vec();
        tiff.extend(8u32.to_le_bytes());
        tiff.extend(1u16.to_le_bytes());
        tiff.extend([0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        assert_eq!(orientation_of(&tiff), Orientation::Rotate90);
        assert_eq!(orientation_of(b"not a tiff"), Orientation::Normal);
    }
}
