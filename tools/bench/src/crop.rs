//! `bench crop <raw>...`: checks the engine's crop, straighten and rotate on real files.
//!
//! 1. A quarter turn equals the unrotated render turned by 90° (pixel for pixel).
//! 2. A crop equals the same window cut from the uncropped render at the same scale.
//! 3. An angle of 10° matches a CPU bilinear rotation of the unrotated render about its centre.
//! 4. An export with rotation, angle and crop has the crop's pixel size and matches the screen's
//!    render at Fit, downscaled.

use std::path::PathBuf;

use image::imageops::{self, FilterType};
use image::{ImageBuffer, Rgb};
use windedit_core::decode;
use windedit_core::export::{self, TileRenderer};
use windedit_core::geometry::{self, Constrain};
use windedit_core::gpu::Gpu;
use windedit_core::pipeline::{Edit, Image, ImageMemory, Target, Zoom, read_target, warm_up};

pub fn bench_crop(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {}", gpu.adapter_name);
    warm_up(&mut gpu).unwrap();
    let mut failed = false;
    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let frame = decode::decode_file(f).unwrap();
        let mut memory = ImageMemory::new();
        let (image, _) = Image::open(&mut gpu, &mut memory, &frame).unwrap();
        let (ow, oh) = image.oriented_size();
        println!("\n{name} ({} {}): {ow}x{oh} after the camera's orientation ({:?})", frame.make, frame.model, frame.orientation);
        // Render at 1/k scale, k a divisor of both sides, so every window below lands on whole pixels.
        let k = [4u32, 2, 1].into_iter().find(|k| ow % k == 0 && oh % k == 0).unwrap();
        let s = 1.0 / k as f32;
        let (a, b) = (ow / k, oh / k);
        // A tone model that is the same for every view of this photo: the edit is shared.
        let edit = Edit { exposure: 0.3, contrast: 15.0, shadows: 30.0, vibrance: 20.0, ..Default::default() };
        macro_rules! render {
            ($edit:expr, $zoom:expr, $w:expr, $h:expr) => {{
                let target = Target::new(&gpu, $w, $h).unwrap();
                gpu.begin().unwrap();
                windedit_core::pipeline::render(&gpu, windedit_core::pipeline::Layer::Raw(&image), $edit, $zoom, (ow, oh), &target);
                gpu.submit().unwrap();
                read_target(&mut gpu, &target).unwrap()
            }};
        }
        let plain = render!(&edit, Zoom::Region { x: 0.0, y: 0.0, scale: s }, a, b);

        // 1. A quarter turn clockwise.
        let turned = render!(&Edit { rotation: 1, ..edit }, Zoom::Region { x: 0.0, y: 0.0, scale: s }, b, a);
        let mut diffs = Vec::new();
        for j in 0..a as usize {
            for i in 0..b as usize {
                let (x, y) = (j, b as usize - 1 - i);
                for c in 0..4 {
                    diffs.push(turned[(j * b as usize + i) * 4 + c].abs_diff(plain[(y * a as usize + x) * 4 + c]));
                }
            }
        }
        let (mean, max) = stats(&diffs);
        let over = diffs.iter().filter(|&&d| d > 1).count();
        failed |= !report("rotation 1 = unrotated turned 90°", format!("mean {mean:.4}, largest {max} levels, {over} of {} values off by more than 1", diffs.len()), max <= 3 && over * 10_000 < diffs.len());

        // 2. A crop, at pixel positions that are whole at this scale.
        let snap = |v: f32, size: u32| (v * size as f32 / k as f32).round() as u32 * k;
        let (x0, y0) = (snap(0.2, ow), snap(0.3, oh));
        let (x1, y1) = (snap(0.8, ow), snap(0.75, oh));
        let crop = [x0 as f32 / ow as f32, y0 as f32 / oh as f32, x1 as f32 / ow as f32, y1 as f32 / oh as f32];
        let (cw, ch) = ((x1 - x0) / k, (y1 - y0) / k);
        let cropped = render!(&Edit { crop, ..edit }, Zoom::Fit, cw, ch);
        let (ox, oy) = ((x0 / k) as usize, (y0 / k) as usize);
        let mut diffs = Vec::new();
        for y in 0..ch as usize {
            for x in 0..cw as usize {
                for c in 0..3 {
                    diffs.push(cropped[(y * cw as usize + x) * 4 + c].abs_diff(plain[((y + oy) * a as usize + x + ox) * 4 + c]));
                }
            }
        }
        let (mean, max) = stats(&diffs);
        failed |= !report("crop = window of the uncropped render", format!("{cw}x{ch}: mean {mean:.3}, max {max} levels"), max <= 2);

        // 3. Ten degrees against a CPU rotation of the unrotated render. The tone curve is
        // applied per pixel, so interpolating before it (the GPU) and after it (here) differ at
        // sharp edges; blurred a little, that goes away and what is left is position.
        let angle = 10.0f32;
        let tilted = render!(&Edit { angle, ..edit }, Zoom::Region { x: 0.0, y: 0.0, scale: s }, a, b);
        let blurred = |px: &[u8]| imageops::blur(&image::RgbaImage::from_raw(a, b, px.to_vec()).unwrap(), 2.0).into_raw();
        let (plain_blur, tilted_blur) = (blurred(&plain), blurred(&tilted));
        let (sin, cos) = (angle as f64).to_radians().sin_cos();
        let (cx, cy) = (a as f64 / 2.0, b as f64 / 2.0);
        let compare = |from: &[u8], to: &[u8]| -> Vec<u8> {
            let sample = |x: f64, y: f64, c: usize| -> f64 {
                // Bilinear, with pixel centres at half-integers.
                let (x, y) = (x - 0.5, y - 0.5);
                let (ix, iy) = (x.floor() as i64, y.floor() as i64);
                let (fx, fy) = (x - ix as f64, y - iy as f64);
                let p = |dx: i64, dy: i64| from[((iy + dy) as usize * a as usize + (ix + dx) as usize) * 4 + c] as f64;
                (p(0, 0) * (1.0 - fx) + p(1, 0) * fx) * (1.0 - fy) + (p(0, 1) * (1.0 - fx) + p(1, 1) * fx) * fy
            };
            let margin = 8.0;
            let mut diffs = Vec::new();
            for j in 0..b as usize {
                for i in 0..a as usize {
                    let (dx, dy) = (i as f64 + 0.5 - cx, j as f64 + 0.5 - cy);
                    let (qx, qy) = (cx + dx * cos + dy * sin, cy - dx * sin + dy * cos);
                    if qx < margin || qy < margin || qx > a as f64 - margin || qy > b as f64 - margin {
                        continue;
                    }
                    for c in 0..3 {
                        diffs.push((sample(qx, qy, c) - to[(j * a as usize + i) * 4 + c] as f64).abs().round() as u8);
                    }
                }
            }
            diffs.sort_unstable();
            diffs
        };
        let sharp = compare(&plain, &tilted);
        let (mean, max) = stats(&sharp);
        println!("       (unblurred, for reference: mean {mean:.2}, 99th percentile {}, max {max} levels)", sharp[sharp.len() * 99 / 100]);
        let diffs = compare(&plain_blur, &tilted_blur);
        let (mean, max) = stats(&diffs);
        let p99 = diffs[diffs.len() * 99 / 100];
        failed |= !report(
            "10° = CPU bilinear rotation",
            format!("{} values, blurred: mean {mean:.2}, 99th percentile {p99}, max {max} levels", diffs.len()),
            mean < 1.0 && p99 <= 4,
        );

        // 4. Export with rotation, angle and crop against the screen at Fit.
        let straight = Edit { rotation: 1, angle: 4.0, ..edit };
        let frame_size = geometry::frame_size((ow, oh), 1);
        let fsz = (frame_size.0 as f64, frame_size.1 as f64);
        let whole = geometry::constrain_crop(fsz, straight.angle as f64, [0.0, 0.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0], Constrain::Shrink);
        let (mx, my) = ((whole[0] + whole[2]) / 2.0, (whole[1] + whole[3]) / 2.0);
        let (hw, hh) = ((whole[2] - whole[0]) * 0.4, (whole[3] - whole[1]) * 0.4);
        let crop = [mx - hw, my - hh, mx + hw, my + hh].map(|v| v as f32);
        let edit4 = Edit { crop, ..straight };
        let window = export::window((ow, oh), &edit4);
        let expected = ((hw * 2.0 * fsz.0).round() as i64, (hh * 2.0 * fsz.1).round() as i64);
        let size_ok = (window.width as i64 - expected.0).abs() <= 1 && (window.height as i64 - expected.1).abs() <= 1;
        let mut renderer = TileRenderer::new(&gpu, window, &edit).unwrap();
        let mut rgb = Vec::new();
        while !renderer.done() {
            if let Some(strip) = renderer.step(&mut gpu, &image, &edit4).unwrap() {
                rgb.extend_from_slice(&strip.bgr[..strip.rows as usize * window.width as usize * 3]);
            }
        }
        let got = (rgb.len() / 3 / window.width as usize) as u32;
        failed |= !report("export size", format!("{}x{} (the crop is {}x{}), {} rows written", window.width, window.height, expected.0, expected.1, got), size_ok && got == window.height);
        // BGR → RGB image.
        let mut pixels = Vec::with_capacity(rgb.len());
        rgb.as_chunks::<3>().0.iter().for_each(|p| pixels.extend_from_slice(&[p[2], p[1], p[0]]));
        let full: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_raw(window.width, window.height, pixels).unwrap();
        let (fw, fh) = if window.width >= window.height {
            (1024, (1024.0 * window.height as f32 / window.width as f32).round() as u32)
        } else {
            ((1024.0 * window.width as f32 / window.height as f32).round() as u32, 1024)
        };
        let screen = render!(&edit4, Zoom::Fit, fw, fh);
        let small = imageops::resize(&full, fw, fh, FilterType::Triangle);
        let mut diffs: Vec<u8> = Vec::new();
        for (p, q) in small.pixels().zip(screen.as_chunks::<4>().0) {
            diffs.extend(p.0.iter().zip(q).map(|(x, y)| x.abs_diff(*y)));
        }
        diffs.sort_unstable();
        let (mean, max) = stats(&diffs);
        let within = 100.0 * diffs.partition_point(|&d| d <= 4) as f64 / diffs.len() as f64;
        println!("       (pixel by pixel, for reference: mean {mean:.2}, {within:.1}% within 4 levels, 99th percentile {}, max {max})", diffs[diffs.len() * 99 / 100]);
        // With fine detail averaged away (8x8 blocks), where the downscale filters do not
        // matter and only position, tone and color do.
        let mut blocks = Vec::new();
        let mut worst_at = (0, 0, 0u8);
        for by in 0..fh as usize / 8 {
            for bx in 0..fw as usize / 8 {
                for c in 0..3 {
                    let (mut p, mut q) = (0u32, 0u32);
                    for y in 0..8 {
                        for x in 0..8 {
                            let at = (by * 8 + y) * fw as usize + bx * 8 + x;
                            p += small.as_raw()[at * 3 + c] as u32;
                            q += screen[at * 4 + c] as u32;
                        }
                    }
                    let d = ((p as f64 - q as f64).abs() / 64.0).round() as u8;
                    if d > worst_at.2 {
                        worst_at = (bx * 8, by * 8, d);
                    }
                    blocks.push(d);
                }
            }
        }
        blocks.sort_unstable();
        let (mean, max) = stats(&blocks);
        let p99 = blocks[blocks.len() * 99 / 100];
        let p90 = blocks[blocks.len() * 90 / 100];
        failed |= !report("export vs the screen at Fit", format!("{fw}x{fh}, 8x8-block averages: mean {mean:.2}, max {max} levels, 90th percentile {p90}, 99th {p99}, worst at {},{}", worst_at.0, worst_at.1), mean < 1.5 && p90 <= 4);
        if let Some(dir) = std::env::var_os("CROP_PNG_DIR") {
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            full.save(dir.join(format!("{name}-export.png"))).unwrap();
            image::save_buffer(dir.join(format!("{name}-screen.png")), &screen, fw, fh, image::ExtendedColorType::Rgba8).unwrap();
            image::save_buffer(dir.join(format!("{name}-tilted.png")), &tilted, a, b, image::ExtendedColorType::Rgba8).unwrap();
        }
    }
    if failed {
        println!("\nFAILED");
        std::process::exit(1);
    }
}

fn stats(diffs: &[u8]) -> (f64, u8) {
    (diffs.iter().map(|&d| d as f64).sum::<f64>() / diffs.len().max(1) as f64, diffs.iter().copied().max().unwrap_or(0))
}

fn report(what: &str, detail: String, ok: bool) -> bool {
    println!("  {:<4} {what}: {detail}", if ok { "ok" } else { "FAIL" });
    ok
}
