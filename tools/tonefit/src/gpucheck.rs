//! Checks that the GPU develop pipeline matches the CPU tone model (core/src/tone.rs): the raw
//! is opened and drawn through the engine, and compared with `tone::render` on the same raw's
//! scene-linear image, for the default settings and each tone slider at its extremes, and for masks
//! (a gradient and an ellipse) carrying the tone sliders, Exposure and Clarity.

use std::path::Path;

use windedit_core::decode::{self, Orientation};
use windedit_core::gpu::Gpu;
use windedit_core::masks::{self, ADD, Component, GpuMaskSet, KIND_LINEAR, KIND_RADIAL, Local, Mask, Masks, SUBTRACT, slider};
use windedit_core::pipeline::{Edit, Image, ImageMemory, MaskInput, Target, Zoom, read_target, render_image_masked, warm_up};
use windedit_core::tone::{self, MODEL};

use crate::linear::{display_code_quantiles, scene_linear};
use crate::util::*;

/// Masks for the checks: a linear gradient over the top carrying `top`, and a feathered ellipse in
/// the middle carrying `middle`, with a notch taken out of it, each as (slider, value).
fn two_masks(top: &[(usize, f32)], middle: &[(usize, f32)]) -> Masks {
    let mut a = Mask::new();
    a.components[0] = Component { kind: KIND_LINEAR, mode: ADD, p: [0.5, 0.05, 0.5, 0.45, 0.0, 0.0, 0.0, 0.0] };
    a.count = 1;
    for &(i, v) in top {
        a.sliders[i] = v;
    }
    let mut b = Mask::new();
    b.amount = 80.0;
    b.components[0] = Component { kind: KIND_RADIAL, mode: ADD, p: [0.5, 0.35, 0.3, 0.18, 0.5, 70.0, 0.0, 0.0] };
    b.components[1] = Component { kind: KIND_RADIAL, mode: SUBTRACT, p: [0.62, 0.35, 0.08, 0.08, 0.0, 30.0, 0.0, 0.0] };
    b.count = 2;
    for &(i, v) in middle {
        b.sliders[i] = v;
    }
    let mut m = Masks::default();
    m.masks[0] = a;
    m.masks[1] = b;
    m.count = 2;
    m
}

fn mask_settings(d: Edit) -> Vec<(&'static str, Edit, Masks)> {
    use slider::*;
    vec![
        ("mask exposure", d, two_masks(&[(EXPOSURE, -1.5)], &[(EXPOSURE, 1.0)])),
        ("mask contrast", d, two_masks(&[(CONTRAST, 80.0)], &[(CONTRAST, -60.0)])),
        ("mask shadows+hl", d, two_masks(&[(SHADOWS, 100.0), (HIGHLIGHTS, -70.0)], &[(SHADOWS, -80.0)])),
        ("mask whites+blacks", d, two_masks(&[(WHITES, -80.0), (BLACKS, 50.0)], &[(WHITES, 90.0), (BLACKS, -40.0)])),
        ("mask clarity", d, two_masks(&[(CLARITY, 100.0)], &[(CLARITY, -80.0)])),
        ("mask clarity+global", Edit { clarity: 40.0, shadows: 30.0, ..d }, two_masks(&[(CLARITY, 90.0), (SHADOWS, 80.0)], &[(CLARITY, -100.0), (CONTRAST, 50.0)])),
    ]
}

fn settings() -> Vec<(&'static str, Edit)> {
    // Detail processing is off: the CPU reference has no sharpening or noise reduction.
    let mut d = Edit::default();
    d.set_detail(windedit_core::develop::Detail { amount: 0.0, color: 0.0, ..windedit_core::develop::DETAIL_DEFAULTS });
    vec![
        ("default", d),
        ("exposure+1", Edit { exposure: 1.0, ..d }),
        ("exposure-1", Edit { exposure: -1.0, ..d }),
        ("contrast+100", Edit { contrast: 100.0, ..d }),
        ("contrast-100", Edit { contrast: -100.0, ..d }),
        ("highlights+100", Edit { highlights: 100.0, ..d }),
        ("highlights-100", Edit { highlights: -100.0, ..d }),
        ("shadows+100", Edit { shadows: 100.0, ..d }),
        ("shadows-100", Edit { shadows: -100.0, ..d }),
        ("whites+100", Edit { whites: 100.0, ..d }),
        ("whites-100", Edit { whites: -100.0, ..d }),
        ("blacks+100", Edit { blacks: 100.0, ..d }),
        ("blacks-100", Edit { blacks: -100.0, ..d }),
        ("clarity+100", Edit { clarity: 100.0, ..d }),
        ("clarity-100", Edit { clarity: -100.0, ..d }),
        ("clarity+60 shadows+50", Edit { clarity: 60.0, shadows: 50.0, contrast: 30.0, ..d }),
    ]
}

/// Where the sensor image's position is for pixel (i, j) of the oriented, downscaled scene.
fn sensor_uv(i: usize, j: usize, w: usize, h: usize, orientation: Orientation) -> [f32; 2] {
    let (a, b) = ((i as f32 + 0.5) / w as f32, (j as f32 + 0.5) / h as f32);
    match orientation {
        Orientation::Normal => [a, b],
        Orientation::Rotate90 => [b, 1.0 - a],
        Orientation::Rotate180 => [1.0 - a, 1.0 - b],
        _ => [1.0 - b, a],
    }
}

pub fn run(raw: &Path, out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let frame = decode::decode_file(raw)?;
    let stem = raw.file_stem().and_then(|s| s.to_str()).unwrap_or("raw");
    let (scene, w, h) = scene_linear(&frame, 512);
    let offset = tone::camera_offset(&frame.make);

    let mut gpu = Gpu::new()?;
    warm_up(&mut gpu)?;
    let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame)?;
    let target = Target::new(&gpu, w as u32, h as u32)?;
    println!("{} ({} {}): {}x{}, camera offset {offset}", raw.display(), frame.make, frame.model, w, h);
    println!("{:<16} {:>8} {:>8} {:>10}   (8-bit levels; luminance quantiles, then per pixel)", "setting", "median", "p95", "mean/pixel");

    let mut worst = 0f32;
    let none = Masks::default();
    let all: Vec<(&str, Edit, Masks)> =
        settings().into_iter().map(|(n, e)| (n, e, none)).chain(mask_settings(settings()[0].1)).collect();
    for (name, edit, mask_set) in all {
        // The reference, quantized to 8 bits as the GPU's output is.
        let set = GpuMaskSet::new(&mask_set, &frame.xyz_to_cam);
        let dims = (frame.width, frame.height);
        let locals: Vec<Local> = (0..w * h)
            .map(|k| set.local(masks::to_frame(sensor_uv(k % w, k / w, w, h, frame.orientation), dims)))
            .collect();
        let cpu = tone::render_masked(&MODEL, &scene, w, h, &edit, offset, (!mask_set.is_empty()).then_some(&locals[..]));
        let cpu_codes: Vec<[u8; 3]> = cpu.iter().map(|p| p.map(|v| (linear_to_srgb(v) * 255.0).round() as u8)).collect();

        gpu.begin()?;
        render_image_masked(&gpu, &image, &edit, &MaskInput { masks: &mask_set, overlay: None, brush: None }, Zoom::Fit, &target);
        gpu.submit()?;
        let rgba = read_target(&mut gpu, &target)?;
        if std::env::var_os("GPUCHECK_DEBUG").is_some() {
            debug(&mut gpu, &image, &scene, w, h, offset, &edit)?;
        }
        let gpu_codes: Vec<[u8; 3]> = rgba.as_chunks::<4>().0.iter().map(|p| [p[0], p[1], p[2]]).collect();

        let as_linear = |codes: &[[u8; 3]]| -> Vec<[f32; 3]> {
            codes.iter().map(|p| p.map(|v| srgb_to_linear(v as f32 / 255.0))).collect()
        };
        let (a, b) = (display_code_quantiles(&as_linear(&cpu_codes)), display_code_quantiles(&as_linear(&gpu_codes)));
        let mut diffs: Vec<f32> = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).collect();
        diffs.sort_by(|x, y| x.total_cmp(y));
        let (median, p95) = (diffs[diffs.len() / 2], diffs[diffs.len() * 95 / 100]);
        let luma = |p: &[u8; 3]| linear_to_srgb(luminance(p.map(|v| srgb_to_linear(v as f32 / 255.0)))) * 255.0;
        let mean = cpu_codes.iter().zip(&gpu_codes).map(|(x, y)| (luma(x) - luma(y)).abs()).sum::<f32>() / cpu_codes.len() as f32;
        worst = worst.max(p95);
        println!("{name:<16} {median:>8.2} {p95:>8.2} {mean:>10.2}");

        // CPU on the left, GPU on the right.
        let mut pair = image::RgbImage::new(w as u32 * 2, h as u32);
        for y in 0..h {
            for x in 0..w {
                pair.put_pixel(x as u32, y as u32, image::Rgb(cpu_codes[y * w + x]));
                pair.put_pixel((w + x) as u32, y as u32, image::Rgb(gpu_codes[y * w + x]));
            }
        }
        pair.save(out_dir.join(format!("{stem}_{name}.png")))?;
    }
    println!("worst 95th percentile: {worst:.2} levels; pairs (CPU left, GPU right) in {}", out_dir.display());
    Ok(())
}

/// Prints the GPU pre-pass results next to the CPU's for the same image (GPUCHECK_DEBUG=1).
fn debug(gpu: &mut Gpu, image: &Image, scene: &[[f32; 3]], w: usize, h: usize, offset: f32, edit: &Edit) -> Result<()> {
    let maps = windedit_core::pipeline::read_tone_maps(gpu, image)?;
    let s = tone::Sliders::from_edit(edit);
    let gain = (offset + edit.exposure).exp2();
    let xg: Vec<f32> = scene.iter().map(|p| tone::global(&MODEL, (tone::luminance(*p) * gain).max(1e-9).log2(), &s)).collect();
    let mut sorted = xg.clone();
    sorted.sort_by(f32::total_cmp);
    let at = |q: f32| sorted[((sorted.len() - 1) as f32 * q) as usize];
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    let a: Vec<f32> = maps.coefficients.iter().map(|c| c[0]).collect();
    let b: Vec<f32> = maps.coefficients.iter().map(|c| c[1]).collect();
    let factor = (w.max(h) as f32 / tone::BASE_EDGE).ceil() as usize;
    let radius = ((w.max(h) as f32 * MODEL.base_radius / factor as f32).ceil() as usize).max(1);
    let base = tone::base_layer(&xg, w, h, factor, radius, MODEL.base_eps);
    let (sw, sh) = (image.width.div_ceil(image.width.max(image.height).div_ceil(128)) as usize, image.height.div_ceil(image.width.max(image.height).div_ceil(128)) as usize);
    let f = image.width.max(image.height).div_ceil(128) as f32;
    let sample = |v: &[f32], i: usize, j: usize| {
        let fx = ((i as f32 + 0.5) / w as f32 * image.width as f32 / f - 0.5).clamp(0.0, (sw - 1) as f32);
        let fy = ((j as f32 + 0.5) / h as f32 * image.height as f32 / f - 0.5).clamp(0.0, (sh - 1) as f32);
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(sw - 1), (y0 + 1).min(sh - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let row = |y: usize| v[y * sw + x0] * (1.0 - tx) + v[y * sw + x1] * tx;
        row(y0) * (1.0 - ty) + row(y1) * ty
    };
    let gpu_base: Vec<f32> = (0..w * h).map(|k| sample(&a, k % w, k / w) * xg[k] + sample(&b, k % w, k / w)).collect();
    let diffs: Vec<f32> = gpu_base.iter().zip(&base).map(|(p, q)| (p - q).abs()).collect();
    let mut sd = diffs.clone();
    sd.sort_by(f32::total_cmp);
    println!("  base difference GPU vs CPU (stops): mean {:.3}, median {:.3}, p95 {:.3}, max {:.3}", mean(&diffs), sd[sd.len() / 2], sd[sd.len() * 95 / 100], sd[sd.len() - 1]);
    println!(
        "  GPU stats top {:.3} dark {:.3} | CPU top {:.3} dark {:.3}; small mean {:.3} min {:.3} max {:.3}; a mean {:.3}, b mean {:.3}; CPU base mean {:.3}, xg mean {:.3}",
        maps.stats[0], maps.stats[1], at(0.995), at(0.25), mean(&maps.small),
        maps.small.iter().cloned().fold(f32::MAX, f32::min), maps.small.iter().cloned().fold(f32::MIN, f32::max),
        mean(&a), mean(&b), mean(&base), mean(&xg)
    );
    Ok(())
}
