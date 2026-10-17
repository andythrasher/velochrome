//! Small shared pieces: sRGB conversion, the PFM file format, box downscaling and quantiles.

use std::fs;
use std::path::Path;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Pfm {
    pub width: usize,
    pub height: usize,
    /// Top row first, three floats per pixel.
    pub pixels: Vec<[f32; 3]>,
}

pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

pub fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

/// Rec.709 luminance of linear RGB.
pub fn luminance(p: [f32; 3]) -> f32 {
    0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]
}

pub fn write_pfm(path: &Path, image: &Pfm) -> Result<()> {
    let mut out = format!("PF\n{} {}\n-1.0\n", image.width, image.height).into_bytes();
    // PFM stores the bottom row first.
    for row in image.pixels.chunks(image.width).rev() {
        for p in row {
            for v in p {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    fs::write(path, out)?;
    Ok(())
}

pub fn read_pfm(path: &Path) -> Result<Pfm> {
    let bytes = fs::read(path)?;
    // Three header lines: magic, size, scale (negative = little endian).
    let mut pos = 0;
    let mut lines = Vec::new();
    while lines.len() < 3 {
        let end = bytes[pos..].iter().position(|&b| b == b'\n').ok_or("bad PFM header")?;
        lines.push(String::from_utf8_lossy(&bytes[pos..pos + end]).trim().to_string());
        pos += end + 1;
    }
    if lines[0] != "PF" || !lines[2].starts_with('-') {
        return Err("only little-endian 3-channel PFM is supported".into());
    }
    let mut size = lines[1].split_whitespace().map(|v| v.parse::<usize>());
    let (width, height) = (size.next().ok_or("bad PFM size")??, size.next().ok_or("bad PFM size")??);
    let floats: Vec<f32> = bytes[pos..].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
    if floats.len() < width * height * 3 {
        return Err("PFM data is short".into());
    }
    let mut pixels = vec![[0f32; 3]; width * height];
    for (i, row) in floats.chunks_exact(width * 3).take(height).enumerate() {
        let y = height - 1 - i;
        for x in 0..width {
            pixels[y * width + x] = [row[x * 3], row[x * 3 + 1], row[x * 3 + 2]];
        }
    }
    Ok(Pfm { width, height, pixels })
}

/// Size with the long edge at `edge`, keeping the aspect ratio.
pub fn fit_size(width: usize, height: usize, edge: usize) -> (usize, usize) {
    let scale = edge as f64 / width.max(height) as f64;
    (((width as f64 * scale).round() as usize).max(1), ((height as f64 * scale).round() as usize).max(1))
}

/// Box filter: every output pixel is the area-weighted average of the source pixels it covers.
pub fn box_downscale(src: &[[f32; 3]], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<[f32; 3]> {
    // Per output column or row: the source range it covers, with a weight for each source pixel.
    fn spans(src_len: usize, dst_len: usize) -> Vec<(usize, Vec<f32>)> {
        let scale = src_len as f64 / dst_len as f64;
        (0..dst_len)
            .map(|d| {
                let (a, b) = (d as f64 * scale, (d + 1) as f64 * scale);
                let first = a.floor() as usize;
                let last = (b.ceil() as usize).min(src_len);
                let weights = (first..last).map(|s| ((b.min(s as f64 + 1.0) - a.max(s as f64)) / scale) as f32).collect();
                (first, weights)
            })
            .collect()
    }
    let (cols, rows) = (spans(sw, dw), spans(sh, dh));
    // Horizontal pass, then vertical.
    let mut mid = vec![[0f32; 3]; dw * sh];
    for y in 0..sh {
        for (x, (first, weights)) in cols.iter().enumerate() {
            let mut sum = [0f32; 3];
            for (i, w) in weights.iter().enumerate() {
                let p = src[y * sw + first + i];
                for c in 0..3 {
                    sum[c] += p[c] * w;
                }
            }
            mid[y * dw + x] = sum;
        }
    }
    let mut out = vec![[0f32; 3]; dw * dh];
    for (y, (first, weights)) in rows.iter().enumerate() {
        for x in 0..dw {
            let mut sum = [0f32; 3];
            for (i, w) in weights.iter().enumerate() {
                let p = mid[(first + i) * dw + x];
                for c in 0..3 {
                    sum[c] += p[c] * w;
                }
            }
            out[y * dw + x] = sum;
        }
    }
    out
}

/// The 255 quantile levels, from 0.2% to 99.8%, as fractions.
pub fn quantile_levels() -> Vec<f64> {
    (0..255).map(|i| (0.2 + 99.6 * i as f64 / 254.0) / 100.0).collect()
}

/// Quantiles of `values` (sorted in place) at each level, interpolating between neighbors.
pub fn quantiles(values: &mut [f32], levels: &[f64]) -> Vec<f32> {
    values.sort_by(|a, b| a.total_cmp(b));
    let last = (values.len() - 1) as f64;
    levels
        .iter()
        .map(|q| {
            let at = q * last;
            let i = at.floor() as usize;
            let t = (at - i as f64) as f32;
            values[i] + (values[(i + 1).min(values.len() - 1)] - values[i]) * t
        })
        .collect()
}

/// A PNG's pixels as linear light.
pub fn load_linear_png(path: &Path) -> Result<Pfm> {
    let img = open_rgb(path)?;
    let lut: Vec<f32> = (0..256).map(|v| srgb_to_linear(v as f32 / 255.0)).collect();
    let pixels = img.pixels().map(|p| [lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]]).collect();
    Ok(Pfm { width: img.width() as usize, height: img.height() as usize, pixels })
}

/// Opens an image, going by its contents rather than its file extension (some of the
/// screenshots are WebP saved as .png).
pub fn open_rgb(path: &Path) -> Result<image::RgbImage> {
    Ok(image::ImageReader::open(path)?.with_guessed_format()?.decode()?.to_rgb8())
}
