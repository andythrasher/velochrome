//! The cheap way to show a photo: the JPEGs the camera already made. Every raw from these
//! cameras keeps its preview JPEGs in its first few hundred kilobytes, and in RAW+JPEG mode
//! there's a full-size JPEG beside it. A preview reads just the one JPEG it needs and decodes it
//! at the size it will be shown.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::decode::{self, DecodeError, EmbeddedJpeg, Orientation};
use crate::jpeg::{self, Pixels};
use crate::library::Photo;

/// How much of a raw file to read for its header and embedded JPEGs. The JPEGs begin within the
/// first 400 KB in every file seen so far; they may run past this, and are then read to the end.
const HEAD_BYTES: usize = 1 << 20;
/// Enough of a JPEG file to find its frame header past the EXIF block (at most 64 KB).
const JPEG_HEAD_BYTES: usize = 256 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A JPEG inside the raw file.
    Embedded,
    /// The camera's JPEG beside the raw.
    CameraJpeg,
}

/// Which preview to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Whatever can be shown soonest: the smallest JPEG big enough for a loupe.
    Fast,
    /// The cheapest JPEG whose longer side is at least this many pixels, decoded at about that
    /// size (or the biggest there is, if none is that big).
    Cover(u32),
    /// The biggest JPEG, at full size: for checking focus at 100%.
    Largest,
}

pub struct Preview {
    pub pixels: Pixels,
    pub orientation: Orientation,
    pub source: Source,
    /// The size of the JPEG it was decoded from (before any scaling).
    pub source_width: u32,
    pub source_height: u32,
    /// It came from the photo's biggest JPEG, so a sharper preview isn't possible.
    pub is_largest: bool,
    pub time: Duration,
}

/// What can be learned from the start of a raw file.
pub struct RawHeader {
    pub orientation: Orientation,
    pub jpegs: Vec<EmbeddedJpeg>,
}

pub fn read_raw_header(path: &Path) -> Result<RawHeader, DecodeError> {
    let head = read_head(path, 0, HEAD_BYTES)?;
    Ok(RawHeader { orientation: decode::orientation_of(&head), jpegs: decode::embedded_jpegs(&head) })
}

pub fn load(photo: &Photo, want: Want) -> Result<Preview, DecodeError> {
    let start = Instant::now();
    let header = match &photo.raw {
        Some(raw) => Some(read_raw_header(raw)?),
        None => None,
    };

    // (source, offset in its file, width, height)
    let mut candidates: Vec<(Source, usize, u32, u32)> = Vec::new();
    if let Some(header) = &header {
        // Skip the tiny thumbnails; anything under 640 pixels is no use in a loupe.
        for j in header.jpegs.iter().filter(|j| j.width.max(j.height) >= 640) {
            candidates.push((Source::Embedded, j.offset, j.width, j.height));
        }
    }
    if let Some(path) = &photo.jpeg {
        let head = read_head(path, 0, JPEG_HEAD_BYTES)?;
        if let Some((w, h)) = decode::jpeg_dimensions(&head) {
            candidates.push((Source::CameraJpeg, 0, w, h));
        }
    }
    // Smallest first; embedded first among equals, as it's a shorter read.
    candidates.sort_by_key(|&(source, _, w, h)| (w.max(h), source == Source::CameraJpeg));
    let max_edge = candidates.last().map(|c| c.2.max(c.3)).ok_or_else(|| DecodeError("no preview in this photo".into()))?;
    // Among the biggest, the embedded one (the sort put it first): it's the cheaper read.
    let largest = *candidates.iter().find(|c| c.2.max(c.3) == max_edge).unwrap();
    // The best candidate first, then the rest in the order they'd next best do, so a JPEG that
    // won't decode (damaged, or not really a JPEG) falls back to another.
    let mut order: Vec<_> = candidates.clone();
    let decode_edge = match want {
        Want::Fast => 0,
        Want::Cover(edge) => {
            // Those that cover the view, smallest first; then the smaller ones, biggest first.
            order.sort_by_key(|c| {
                let e = c.2.max(c.3);
                if e >= edge { (0, e as i64) } else { (1, -(e as i64)) }
            });
            edge
        }
        Want::Largest => {
            order.sort_by_key(|c| (std::cmp::Reverse(c.2.max(c.3)), c.0 == Source::CameraJpeg));
            0
        }
    };
    // The cameras store JPEG pixels unrotated, with the same orientation as the raw.
    let orientation = header.map(|h| h.orientation).unwrap_or_default();
    let mut last_error = DecodeError("no preview in this photo".into());
    for (source, offset, source_width, source_height) in order {
        let bytes = match source {
            Source::Embedded => read_jpeg_at(photo.raw.as_ref().unwrap(), offset),
            Source::CameraJpeg => std::fs::read(photo.jpeg.as_ref().unwrap()).map_err(|e| DecodeError(e.to_string())),
        };
        match bytes.and_then(|b| jpeg::decode(&b, decode_edge).map_err(|e| DecodeError(e.to_string()))) {
            Ok(pixels) => {
                let is_largest = (source_width, source_height) == (largest.2, largest.3);
                return Ok(Preview { pixels, orientation, source, source_width, source_height, is_largest, time: start.elapsed() });
            }
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

fn read_head(path: &Path, offset: u64, len: usize) -> Result<Vec<u8>, DecodeError> {
    let mut file = File::open(path).map_err(|e| DecodeError(e.to_string()))?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| DecodeError(e.to_string()))?;
    let mut buf = Vec::with_capacity(len);
    file.take(len as u64).read_to_end(&mut buf).map_err(|e| DecodeError(e.to_string()))?;
    Ok(buf)
}

/// Reads one JPEG out of a file, from its start marker through its end marker.
fn read_jpeg_at(path: &Path, offset: usize) -> Result<Vec<u8>, DecodeError> {
    let mut file = File::open(path).map_err(|e| DecodeError(e.to_string()))?;
    file.seek(SeekFrom::Start(offset as u64)).map_err(|e| DecodeError(e.to_string()))?;
    let mut bytes = Vec::with_capacity(4 << 20);
    let mut chunk = vec![0u8; 512 << 10];
    let mut scan_from = 0;
    loop {
        let n = file.read(&mut chunk).map_err(|e| DecodeError(e.to_string()))?;
        if n == 0 {
            return Ok(bytes); // no end marker: let the decoder make what it can of it
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = decode::jpeg_end(&bytes, scan_from) {
            bytes.truncate(end);
            return Ok(bytes);
        }
        scan_from = bytes.len().saturating_sub(1);
    }
}
