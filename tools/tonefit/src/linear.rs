//! Our engine's scene-linear image, computed on the CPU the way the GPU path does it, and the
//! check that putting it through the tone model at default settings reproduces what the app shows.

use std::path::Path;

use windedit_core::color::{self, REC2020_TO_SRGB};
use windedit_core::decode::{self, Orientation, RawFrame};
use windedit_core::develop::Edit;
use windedit_core::tone::{self, MODEL};

use crate::util::*;

/// Size of the raw on screen, after orientation.
pub fn oriented_size(frame: &RawFrame) -> (usize, usize) {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if frame.orientation.swaps_axes() { (h, w) } else { (w, h) }
}

/// Linear sRGB-primaries image, as shot, no tone curve: what the develop shader's tone() gets at
/// default settings. Superpixel demosaic, white balance, then the highlight clip of demosaic.hlsl.
fn develop_linear(frame: &RawFrame) -> (Vec<[f32; 3]>, usize, usize) {
    let (w, h) = (frame.width as usize / 2, frame.height as usize / 2);
    let to_display = color::mul(&REC2020_TO_SRGB, &frame.cam_to_working);
    let fw = frame.width as usize;
    let sample = |x: usize, y: usize| {
        let (r, c) = (y & 1, x & 1);
        let black = frame.black[r][c];
        let v = frame.data[y * fw + x] as f32;
        (v - black).max(0.0) / (frame.white - black) * frame.wb[frame.cfa[r][c] as usize]
    };
    let mut out = Vec::with_capacity(w * h);
    for sy in 0..h {
        for sx in 0..w {
            let mut cam = [0f32; 3];
            let mut greens = 0.0;
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let (x, y) = (sx * 2 + dx, sy * 2 + dy);
                let color = frame.cfa[y & 1][x & 1] as usize;
                cam[color] += sample(x, y);
                if color == 1 {
                    greens += 1.0;
                }
            }
            cam[1] /= greens;
            // Clip to the level where every channel is still measured.
            let cam = cam.map(|v| v.clamp(0.0, 1.0));
            out.push(to_display.map(|row| row[0] * cam[0] + row[1] * cam[1] + row[2] * cam[2]));
        }
    }
    (out, w, h)
}

/// Rotates clockwise by the orientation.
fn orient(src: Vec<[f32; 3]>, w: usize, h: usize, orientation: Orientation) -> (Vec<[f32; 3]>, usize, usize) {
    if orientation == Orientation::Normal {
        return (src, w, h);
    }
    let (ow, oh) = if orientation.swaps_axes() { (h, w) } else { (w, h) };
    let mut out = vec![[0f32; 3]; w * h];
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match orientation {
                Orientation::Rotate90 => (h - 1 - y, x),
                Orientation::Rotate180 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            out[ny * ow + nx] = src[y * w + x];
        }
    }
    (out, ow, oh)
}

/// The scene-linear image of a frame, oriented, with its long edge at `edge`.
pub fn scene_linear(frame: &RawFrame, edge: usize) -> (Vec<[f32; 3]>, usize, usize) {
    let (px, w, h) = develop_linear(frame);
    let (px, w, h) = orient(px, w, h, frame.orientation);
    let (dw, dh) = fit_size(w, h, edge);
    (box_downscale(&px, w, h, dw, dh), dw, dh)
}

pub fn linear(raw: &Path, out: &Path, edge: usize) -> Result<()> {
    let frame = decode::decode_file(raw)?;
    let (w, h) = oriented_size(&frame);
    let (pixels, dw, dh) = scene_linear(&frame, edge);
    write_pfm(out, &Pfm { width: dw, height: dh, pixels })?;
    println!("{}: {:?}, {}x{} -> {}x{}", raw.display(), frame.orientation, w, h, dw, dh);
    Ok(())
}

/// Luminance quantiles, as 8-bit levels, of the display values `pixels` (linear light).
pub fn display_code_quantiles(pixels: &[[f32; 3]]) -> Vec<f32> {
    let mut y: Vec<f32> = pixels.iter().map(|&p| luminance(p)).collect();
    quantiles(&mut y, &quantile_levels()).into_iter().map(|v| linear_to_srgb(v) * 255.0).collect()
}

/// Develops `linear_pfm` at default settings as the app does, compares its luminance quantiles
/// with the screenshot crop `reference`, and writes the developed image next to the PFM.
pub fn check(linear_pfm: &Path, reference: &Path, camera_offset: f32) -> Result<()> {
    let image = read_pfm(linear_pfm)?;
    // The tone model at default settings, sRGB encode, and 8 bits, as the shader's output is stored.
    let developed = tone::render(&MODEL, &image.pixels, image.width, image.height, &Edit::default(), camera_offset);
    let encoded: Vec<[u8; 3]> =
        developed.iter().map(|p| p.map(|v| (linear_to_srgb(v) * 255.0).round() as u8)).collect();
    let display: Vec<[f32; 3]> = encoded.iter().map(|p| p.map(|v| srgb_to_linear(v as f32 / 255.0))).collect();
    let png = linear_pfm.with_extension("check.png");
    let bytes: Vec<u8> = encoded.iter().flatten().copied().collect();
    image::RgbImage::from_raw(image.width as u32, image.height as u32, bytes).ok_or("size mismatch")?.save(&png)?;

    let ours = display_code_quantiles(&display);
    let theirs = display_code_quantiles(&load_linear_png(reference)?.pixels);
    let mut diffs: Vec<f32> = ours.iter().zip(&theirs).map(|(a, b)| (a - b).abs()).collect();
    let signed: f32 = ours.iter().zip(&theirs).map(|(a, b)| a - b).sum::<f32>() / ours.len() as f32;
    diffs.sort_by(|a, b| a.total_cmp(b));
    println!(
        "{} vs {}: median {:.2}, 95th percentile {:.2}, max {:.2} levels (mean signed {:+.2}, ours minus app); wrote {}",
        linear_pfm.display(),
        reference.display(),
        diffs[diffs.len() / 2],
        diffs[diffs.len() * 95 / 100],
        diffs[diffs.len() - 1],
        signed,
        png.display()
    );
    Ok(())
}
