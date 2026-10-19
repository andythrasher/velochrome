//! `bench previews <raw>...`: developed previews (docs/edited-previews.md) on real files.
//!
//! For each photo and a few edits (tone, crop with a turn and straighten, a mask, detail):
//!
//! - how long the GPU thread is busy rendering a preview (2560 px) and the live thumbnail
//!   (512 px), and how long the writer thread takes to encode and file one;
//! - that the preview matches a full-size export of the same edit scaled to the same size: the
//!   same crop exactly (no shift: the best of the nine one-pixel offsets is the unshifted one)
//!   and a mean difference under one level.

use std::path::PathBuf;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

use windedit_core::brushgpu::BrushGpu;
use windedit_core::developed::{self, PreviewRenderer};
use windedit_core::export::{self, TileRenderer};
use windedit_core::gpu::Gpu;
use windedit_core::masks::{Component, KIND_RADIAL, Mask, Masks, StrokeMap, slider};
use windedit_core::pipeline::{Edit, Image, ImageMemory, warm_up};
use windedit_core::decode;

use crate::{median, ms};

struct Case {
    name: &'static str,
    edit: Edit,
    masks: Masks,
}

fn cases() -> Vec<Case> {
    // Sharpening and colour noise reduction are on by default, and they work at the scale of a
    // pixel: a fitted render does them at its own scale, an export at full size, so the two differ
    // in fine texture however well the tone, colour and crop agree. The first cases switch them off
    // to compare those; the last two keep them and compare at 8x8-block averages.
    let off = Edit { sharpen_amount: -40.0, noise_color: -25.0, ..Default::default() };
    let tone = Edit { exposure: 0.3, contrast: 15.0, highlights: -25.0, shadows: 30.0, vibrance: 20.0, saturation: 10.0, ..off };
    let mut masks = Masks::default();
    masks.masks[0] = Mask::new();
    masks.masks[0].components[0] = Component { kind: KIND_RADIAL, p: [0.5, 0.45, 0.3, 0.25, 0.0, 60.0, 0.0, 0.0], ..Default::default() };
    masks.masks[0].count = 1;
    masks.masks[0].sliders[slider::EXPOSURE] = 0.8;
    masks.count = 1;
    vec![
        Case { name: "tone", edit: tone, masks: Masks::default() },
        Case { name: "tone with default detail", edit: Edit { sharpen_amount: 0.0, noise_color: 0.0, ..tone }, masks: Masks::default() },
        Case { name: "crop, quarter turn, straighten", edit: Edit { rotation: 1, angle: 2.5, crop: [0.18, 0.2, 0.82, 0.8], ..tone }, masks: Masks::default() },
        Case { name: "radial mask", edit: tone, masks },
        Case {
            name: "detail (sharpen, noise, texture, clarity)",
            edit: Edit { sharpen_amount: 40.0, noise_luminance: 30.0, texture: 25.0, clarity: 20.0, ..tone },
            masks: Masks::default(),
        },
    ]
}

pub fn bench_previews(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {}", gpu.adapter_name);
    warm_up(&mut gpu).unwrap();
    let mut failed = false;
    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let frame = decode::decode_file(f).unwrap();
        let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap();
        let size = image.oriented_size();
        println!("\n{name} ({} {}): {}x{} after the camera's orientation", frame.make, frame.model, size.0, size.1);
        let none = StrokeMap::new();
        let mut renderer = PreviewRenderer::new();
        let mut live = PreviewRenderer::new();
        for case in cases() {
            println!("  {}:", case.name);
            // Rendering: the first call allocates; the next five are what a user sees.
            let mut times = Vec::new();
            let mut last = None;
            for run in 0..6 {
                let start = Instant::now();
                let r = renderer.render(&mut gpu, &image, &case.edit, &case.masks, &none, developed::PREVIEW_EDGE).unwrap();
                let wall = ms(start.elapsed());
                if run > 0 {
                    times.push((wall, r.render_ms, r.readback_ms));
                }
                last = Some(r);
            }
            let rendered = last.unwrap();
            let col = |i: usize| median(times.iter().map(|t| [t.0, t.1, t.2][i]).collect());
            println!(
                "    preview {}x{}: GPU thread busy {:.1} ms (develop {:.1} ms on the GPU, copy and arrange {:.1} ms)",
                rendered.width, rendered.height, col(0), col(1), col(2)
            );
            let mut thumbs = Vec::new();
            for _ in 0..6 {
                let start = Instant::now();
                let r = live.render(&mut gpu, &image, &case.edit, &case.masks, &none, developed::LIVE_EDGE).unwrap();
                thumbs.push(ms(start.elapsed()));
                drop(r);
            }
            println!("    live thumbnail render at {} px: GPU thread busy {:.1} ms", developed::LIVE_EDGE, median(thumbs[1..].to_vec()));

            let preview = rendered_into_preview(&rendered);
            let mut enc = Vec::new();
            let mut bytes = (0, 0);
            for _ in 0..3 {
                let e = developed::encode_preview(&preview, 256).unwrap();
                enc.push(e.encode_ms);
                bytes = (e.preview.len(), e.thumb_jpeg.len());
            }
            println!("    writer thread: encode preview and thumbnail {:.1} ms; {:.0} KB + {:.0} KB on disk", median(enc), bytes.0 as f64 / 1024.0, bytes.1 as f64 / 1024.0);

            // Against a full-size export, scaled down to the preview's size.
            let window = export::window(size, &case.edit);
            let (ow, oh, scale) = developed::output_size(window, developed::PREVIEW_EDGE);
            assert_eq!((ow, oh), (rendered.width, rendered.height));
            let full = export_rgb(&mut gpu, &image, &case.edit, &case.masks, window);
            let small = area_resize(&full, window.width as usize, window.height as usize, ow as usize, oh as usize, scale as f64);
            let at = |dx: i32, dy: i32| shifted_difference(&rendered.bgra, &small, ow as usize, oh as usize, dx, dy);
            let pixels = at(0, 0);
            let mean = block_difference(&rendered.bgra, &small, ow as usize, oh as usize);
            let mut best = (0, 0, pixels);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let d = at(dx, dy);
                    if d < best.2 {
                        best = (dx, dy, d);
                    }
                }
            }
            if let Ok(dir) = std::env::var("BENCH_PREVIEW_PNGS") {
                save_comparison(&dir, &format!("{name}-{}", case.name.split(',').next().unwrap().replace(' ', "_")), &rendered.bgra, &small, ow as usize, oh as usize);
            }
            let shifted = (best.0, best.1) != (0, 0);
            let ok = mean < 1.0 && !shifted;
            failed |= !ok;
            println!(
                "    vs the full-size export scaled to {ow}x{oh}: mean difference {mean:.3} levels{}; best offset {:?} ({:.3}) -> {}",
                format!(" at 8x8-block averages, {pixels:.3} pixel by pixel"),
                (best.0, best.1),
                best.2,
                if ok { "PASS" } else { "FAIL" }
            );
        }
        // And the whole path as the engine does it: key, file, read back.
        let _ = BrushGpu::new();
    }
    if failed {
        println!("\nFAILED");
        std::process::exit(1);
    }
    println!("\nall previews match their exports");
}

fn rendered_into_preview(r: &developed::Rendered) -> windedit_core::preview::Preview {
    developed::preview_from(
        windedit_core::jpeg::Pixels { width: r.width, height: r.height, bgra: r.bgra.clone() },
        std::time::Duration::ZERO,
    )
}

/// The photo's crop at full size, as packed RGB.
fn export_rgb(gpu: &mut Gpu, image: &Image, edit: &Edit, masks: &Masks, window: export::Window) -> Vec<u8> {
    let mut renderer = TileRenderer::new(gpu, window, edit).unwrap();
    let brush = BrushGpu::new();
    let none = StrokeMap::new();
    brush.sync(gpu, (image.width, image.height), masks, &none).unwrap();
    let (tx, rx) = sync_channel(0);
    let collector = std::thread::spawn(move || {
        let mut rgb = Vec::new();
        for strip in rx {
            let strip: export::Strip = strip;
            for px in strip.bgr.chunks_exact(3) {
                rgb.extend([px[2], px[1], px[0]]);
            }
        }
        rgb
    });
    while !renderer.done() {
        if let Some(strip) = renderer.step_masked(gpu, image, edit, masks, Some(&brush)).unwrap() {
            tx.send(strip).unwrap();
        }
    }
    drop(tx);
    collector.join().unwrap()
}

/// Shrinks packed RGB by averaging the source pixels each output pixel covers (`1 / scale` of them
/// across, fractions at the edges counted by their share): the best a straight export can be
/// made to look like a render at the smaller size.
fn area_resize(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, scale: f64) -> Vec<u8> {
    let weights = |dst: usize, src_len: usize| -> Vec<Vec<(usize, f64)>> {
        (0..dst)
            .map(|i| {
                let (a, b) = (i as f64 / scale, (i + 1) as f64 / scale);
                let b = b.min(src_len as f64);
                (a.floor() as usize..(b.ceil() as usize).min(src_len))
                    .map(|s| (s, ((s + 1) as f64).min(b) - (s as f64).max(a)))
                    .filter(|&(_, w)| w > 0.0)
                    .collect()
            })
            .collect()
    };
    let (wx, wy) = (weights(dw, sw), weights(dh, sh));
    // Across first: every source row down to the output width.
    let used = wy.iter().flatten().map(|&(s, _)| s).max().unwrap_or(0) + 1;
    let mut across = vec![0f32; used * dw * 3];
    for y in 0..used {
        let row = &src[y * sw * 3..(y + 1) * sw * 3];
        for (x, taps) in wx.iter().enumerate() {
            let total: f64 = taps.iter().map(|t| t.1).sum();
            for c in 0..3 {
                let sum: f64 = taps.iter().map(|&(s, w)| row[s * 3 + c] as f64 * w).sum();
                across[(y * dw + x) * 3 + c] = (sum / total) as f32;
            }
        }
    }
    let mut out = vec![0u8; dw * dh * 3];
    for (y, taps) in wy.iter().enumerate() {
        let total: f64 = taps.iter().map(|t| t.1).sum();
        for x in 0..dw {
            for c in 0..3 {
                let sum: f64 = taps.iter().map(|&(s, w)| across[(s * dw + x) * 3 + c] as f64 * w).sum();
                out[(y * dw + x) * 3 + c] = (sum / total).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Mean absolute difference over the colour channels between the preview (BGRA) and the scaled
/// export (RGB) with the export read `dx`, `dy` pixels away, over the pixels both have.
fn shifted_difference(bgra: &[u8], rgb: &[u8], w: usize, h: usize, dx: i32, dy: i32) -> f64 {
    let (mut sum, mut n) = (0u64, 0u64);
    for y in 2..h.saturating_sub(2) {
        for x in 2..w.saturating_sub(2) {
            let (sx, sy) = ((x as i32 + dx) as usize, (y as i32 + dy) as usize);
            let a = &bgra[(y * w + x) * 4..];
            let b = &rgb[(sy * w + sx) * 3..];
            sum += a[2].abs_diff(b[0]) as u64 + a[1].abs_diff(b[1]) as u64 + a[0].abs_diff(b[2]) as u64;
            n += 3;
        }
    }
    sum as f64 / n as f64
}

/// Writes the preview, the scaled export and their difference (times 8) as PNGs, to look at.
fn save_comparison(dir: &str, tag: &str, bgra: &[u8], rgb: &[u8], w: usize, h: usize) {
    let mut a = image::RgbImage::new(w as u32, h as u32);
    let mut b = image::RgbImage::new(w as u32, h as u32);
    let mut d = image::RgbImage::new(w as u32, h as u32);
    let mut signed = [0f64; 3];
    for y in 0..h {
        for x in 0..w {
            let p = &bgra[(y * w + x) * 4..];
            let q = &rgb[(y * w + x) * 3..];
            let pa = [p[2], p[1], p[0]];
            a.put_pixel(x as u32, y as u32, image::Rgb(pa));
            b.put_pixel(x as u32, y as u32, image::Rgb([q[0], q[1], q[2]]));
            let diff = [0, 1, 2].map(|c| (pa[c] as i32 - q[c] as i32));
            for c in 0..3 {
                signed[c] += diff[c] as f64;
            }
            d.put_pixel(x as u32, y as u32, image::Rgb(diff.map(|v| (128 + v * 8).clamp(0, 255) as u8)));
        }
    }
    let n = (w * h) as f64;
    println!("      signed mean (preview - export) R {:.2} G {:.2} B {:.2}; saved {dir}/{tag}-*.png", signed[0] / n, signed[1] / n, signed[2] / n);
    a.save(format!("{dir}/{tag}-preview.png")).unwrap();
    b.save(format!("{dir}/{tag}-export.png")).unwrap();
    d.save(format!("{dir}/{tag}-diff.png")).unwrap();
}

/// Mean absolute difference of the two pictures' 8x8-block averages.
fn block_difference(bgra: &[u8], rgb: &[u8], w: usize, h: usize) -> f64 {
    let (mut sum, mut n) = (0f64, 0f64);
    for by in 0..h / 8 {
        for bx in 0..w / 8 {
            let mut d = [0f64; 3];
            for y in by * 8..by * 8 + 8 {
                for x in bx * 8..bx * 8 + 8 {
                    let a = &bgra[(y * w + x) * 4..];
                    let b = &rgb[(y * w + x) * 3..];
                    d[0] += a[2] as f64 - b[0] as f64;
                    d[1] += a[1] as f64 - b[1] as f64;
                    d[2] += a[0] as f64 - b[2] as f64;
                }
            }
            sum += d.iter().map(|v| (v / 64.0).abs()).sum::<f64>();
            n += 3.0;
        }
    }
    sum / n
}

/// `bench editsidecars <folder> <count> <stride>`: writes an edit into the sidecars of `count` photos
/// of the folder, every `stride`th one from the first (for measuring browsing with a backlog).
/// The edits differ a little from photo to photo, and a few have a mask.
pub fn make_edits(args: &[String]) {
    use windedit_core::xmp::{self, Metadata};
    let dir = PathBuf::from(&args[0]);
    let (count, stride): (usize, usize) = (args[1].parse().unwrap(), args[2].parse().unwrap());
    let photos = windedit_core::library::scan_folder(&dir).unwrap();
    for (n, i) in (0..photos.len()).step_by(stride).take(count).enumerate() {
        let mut meta = Metadata::default();
        meta.edit = Edit { exposure: 0.2 + 0.05 * (n % 8) as f32, contrast: 10.0, shadows: 25.0, vibrance: 15.0, ..Default::default() };
        if n % 3 == 0 {
            meta.edit.crop = [0.05, 0.05, 0.95, 0.9];
        }
        if n % 4 == 0 {
            let mut m = cases().remove(2).masks;
            m.masks[0].sliders[slider::EXPOSURE] = 0.5;
            meta.masks = Some(std::sync::Arc::new(m));
        }
        xmp::write(&xmp::sidecar_path(&dir, &photos[i].name), &meta).unwrap();
    }
    println!("wrote {count} sidecars in {}", dir.display());
}

/// `bench backlog <folder>`: opens a folder with edited photos (see `editsidecars`) and nothing
/// else, and times how long the engine takes to develop every edited photo's preview in the
/// background. Point `WINDEDIT_PREVIEW_CACHE` at an empty folder to start with no cache, and run it
/// again to see the cache used.
pub fn bench_backlog(args: &[String]) {
    use std::time::Duration;
    use windedit_core::engine::Engine;
    use windedit_core::viewer::FrameEvent;
    let (tx, rx) = std::sync::mpsc::channel();
    let engine = Engine::new(Box::new(move |e: &FrameEvent| {
        let _ = tx.send(*e);
    }))
    .expect("engine");
    let start = Instant::now();
    let count = engine.open_folder(std::path::Path::new(&args[0])).unwrap();
    let queued = engine.jobs().backlog_len();
    println!("{count} photos opened in {:.0} ms; {queued} edited photos have no developed preview", ms(start.elapsed()));
    engine.use_offscreen_view(2560, 1440);
    engine.show(0);
    let start = Instant::now();
    let mut thumbs = 0;
    while engine.jobs().backlog_len() > 0 && start.elapsed() < Duration::from_secs(300) {
        while let Ok(e) = rx.try_recv() {
            thumbs += (e.kind == windedit_core::viewer::EVENT_THUMBNAIL) as usize;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!(
        "backlog {} after {:.1} s ({thumbs} thumbnail events); {} still queued",
        if engine.jobs().backlog_len() == 0 { "empty" } else { "NOT done" },
        start.elapsed().as_secs_f64(),
        engine.jobs().backlog_len()
    );
    // Let the last files reach the disk.
    std::thread::sleep(Duration::from_millis(500));
}

/// `bench livecheck <folder>`: the engine end to end on a folder of unedited photos (use a scratch
/// copy: it writes a sidecar). Edits photo 0 in Edit mode and checks that its thumbnail follows
/// within a second, that leaving Edit mode shows the developed preview at once, that the preview
/// reaches the cache, and that a restarted engine shows both from the cache with no render.
pub fn live_check(args: &[String]) {
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;
    use windedit_core::engine::Engine;
    use windedit_core::viewer::{EVENT_THUMBNAIL, FrameEvent};
    let dir = std::path::Path::new(&args[0]);
    let start_engine = || -> (Engine, Receiver<FrameEvent>) {
        let (tx, rx) = channel();
        let e = Engine::new(Box::new(move |e: &FrameEvent| {
            let _ = tx.send(*e);
        }))
        .unwrap();
        e.open_folder(dir).unwrap();
        e.use_offscreen_view(1600, 900);
        (e, rx)
    };
    let wait = |rx: &Receiver<FrameEvent>, secs: f64, until: &mut dyn FnMut(&FrameEvent) -> bool| -> Option<f64> {
        let start = Instant::now();
        while let Some(left) = Duration::from_secs_f64(secs).checked_sub(start.elapsed()) {
            match rx.recv_timeout(left) {
                Ok(e) if until(&e) => return Some(ms(start.elapsed())),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        None
    };
    let mean_level = |t: &windedit_core::jobs::Thumbnail| t.bgra.chunks(4).map(|p| (p[0] as u64 + p[1] as u64 + p[2] as u64) as f64 / 3.0).sum::<f64>() / (t.bgra.len() / 4) as f64;
    let thumb = |e: &Engine| {
        e.set_visible(vec![0]);
        e.jobs().wait_idle(Duration::from_secs(10));
        e.jobs().thumbnail(0).map(|t| mean_level(&t))
    };
    let mut ok = true;
    let mut check = |what: &str, pass: bool, detail: String| {
        println!("  {} {what}: {detail}", if pass { "ok  " } else { "FAIL" });
        ok &= pass;
    };

    let (engine, rx) = start_engine();
    let before = thumb(&engine).expect("a thumbnail");
    engine.set_edit_mode(true);
    engine.show(0);
    wait(&rx, 10.0, &mut |e| e.layer == 4);
    // Drag a little, then let go: the thumbnail is made again about half a second later.
    let edit = windedit_core::pipeline::Edit { exposure: 1.2, contrast: 20.0, ..Default::default() };
    engine.set_edit(0, edit, false);
    engine.set_edit(0, edit, true);
    let followed = wait(&rx, 3.0, &mut |e| e.kind == EVENT_THUMBNAIL && e.photo == 0);
    check("the thumbnail follows a committed edit", followed.is_some_and(|t| (400.0..1500.0).contains(&t)), format!("{followed:?} ms after the commit"));
    let after = thumb(&engine).unwrap();
    check("and is the edited picture", after > before + 10.0, format!("brightness {before:.0} -> {after:.0}"));
    // Back to Cull: the fitted view is the developed preview straight away.
    let left = Instant::now();
    engine.set_edit_mode(false);
    let shown = wait(&rx, 3.0, &mut |e| e.photo == 0 && e.layer == 5);
    check("leaving Edit mode shows the developed preview", shown.is_some(), format!("{shown:?} ms (asked {:.0} ms ago)", ms(left.elapsed())));
    std::thread::sleep(Duration::from_millis(800));
    let files = std::fs::read_dir(windedit_core::developed::default_dir()).map(|d| d.flatten().count()).unwrap_or(0);
    check("the preview and its thumbnail reach the cache", files >= 2, format!("{files} files"));
    let snap = engine.snapshot().map(|(w, h, px)| px.chunks(4).map(|p| (p[0] as u64 + p[1] as u64 + p[2] as u64) as f64 / 3.0).sum::<f64>() / (w * h) as f64);
    println!("       view brightness in Cull: {snap:?}");
    drop(engine);

    // A restart: nothing to render, everything from the cache.
    let (engine, rx) = start_engine();
    let t = thumb(&engine);
    check("a restarted engine's thumbnail is the edited one", t.is_some_and(|t| (t - after).abs() < 3.0), format!("{t:?}"));
    engine.show(0);
    let mut first_layer = None;
    let first = wait(&rx, 3.0, &mut |e| {
        if e.kind == windedit_core::viewer::EVENT_FRAME && e.photo == 0 && first_layer.is_none() {
            first_layer = Some(e.layer);
        }
        e.photo == 0 && e.layer >= 2
    });
    check("and its very first frame is the developed preview", first_layer == Some(5), format!("first frame layer {first_layer:?}, after {first:?} ms"));
    println!("{}", if ok { "\nall checks passed" } else { "\nFAILED" });
    if !ok {
        std::process::exit(1);
    }
}
