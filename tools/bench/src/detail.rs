//! `bench detail`: the checks for the detail pass (docs/phase2b.md, "As built, part A"), and
//! `bench iso`, which finds high-ISO photos to run them on.

use std::path::{Path, PathBuf};
use std::time::Instant;

use windedit_core::decode;
use windedit_core::detail;
use windedit_core::develop::{DETAIL_DEFAULTS, Detail, Edit};
use windedit_core::export::{self, TileRenderer};
use windedit_core::gpu::Gpu;
use windedit_core::pipeline::{Image, ImageMemory, Layer, Target, Zoom, f16_to_f32, read_rgba16f, read_target, render, render_image, warm_up};

/// `bench iso <folder or sidecar JPEG>...`: the ISO of each photo in the folders (read from the
/// camera JPEG beside the raw), highest first.
pub fn bench_iso(args: &[String]) {
    let mut found = Vec::new();
    for arg in args {
        let path = PathBuf::from(arg);
        let mut files: Vec<PathBuf> = if path.is_dir() {
            std::fs::read_dir(&path).into_iter().flatten().flatten().map(|e| e.path()).collect()
        } else {
            vec![path]
        };
        files.sort();
        for f in files {
            let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if ext != "jpg" {
                continue;
            }
            if let Some((iso, model)) = jpeg_iso(&f) {
                let raw = ["ORF", "RW2"].iter().map(|e| f.with_extension(e)).find(|p| p.exists());
                if let Some(raw) = raw {
                    found.push((iso, model, raw));
                }
            }
        }
    }
    found.sort_by_key(|(iso, _, _)| std::cmp::Reverse(*iso));
    for (iso, model, raw) in found.iter().take(100000) {
        println!("ISO {iso:>6}  {model:<24} {}", raw.display());
    }
}

fn jpeg_iso(path: &Path) -> Option<(u32, String)> {
    let bytes = std::fs::read(path).ok()?;
    let head = &bytes[..bytes.len().min(131072)];
    let at = head.windows(6).position(|w| w == b"Exif\0\0")? + 6;
    let t = &head[at..];
    let le = &t[..2] == b"II";
    let u16_at = |o: usize| -> Option<u32> {
        let b = t.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) } as u32)
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = t.get(o..o + 4)?;
        Some(if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) })
    };
    let mut iso = None;
    let mut model = String::new();
    let mut ifds = vec![u32_at(4)? as usize];
    while let Some(ifd) = ifds.pop() {
        let n = u16_at(ifd)? as usize;
        for i in 0..n {
            let e = ifd + 2 + i * 12;
            let (tag, kind, count, value) = (u16_at(e)?, u16_at(e + 2)?, u32_at(e + 4)?, u32_at(e + 8)?);
            match tag {
                0x8769 => ifds.push(value as usize),
                0x8827 | 0x8832 if iso.is_none() || tag == 0x8827 => {
                    iso = Some(if kind == 3 { u16_at(e + 8)? } else { value });
                }
                0x0110 if kind == 2 => {
                    let o = if count > 4 { value as usize } else { e + 8 };
                    model = String::from_utf8_lossy(t.get(o..o + count as usize)?).trim_end_matches('\0').to_string();
                }
                _ => {}
            }
        }
    }
    iso.map(|i| (i, model))
}

// ---- Measuring ----

/// A flat patch of the picture: the lowest-variance 48×48 block (on a 24-pixel grid) of an RGBA
/// image's luminance, away from the borders, skipping blocks with a clipped or crushed pixel.
struct Patch {
    x: usize,
    y: usize,
}

const BLOCK: usize = 48;

fn luma(px: &[u8], w: usize, x: usize, y: usize) -> f32 {
    let p = &px[(y * w + x) * 4..];
    0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32
}

fn find_flat(px: &[u8], w: usize, h: usize) -> Patch {
    let mut best = (f32::MAX, 0, 0);
    let mut y = BLOCK;
    while y + 2 * BLOCK < h {
        let mut x = BLOCK;
        while x + 2 * BLOCK < w {
            let (mut s, mut s2, mut lo, mut hi) = (0.0f32, 0.0f32, 255.0f32, 0.0f32);
            for j in 0..BLOCK {
                for i in 0..BLOCK {
                    let v = luma(px, w, x + i, y + j);
                    s += v;
                    s2 += v * v;
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            let n = (BLOCK * BLOCK) as f32;
            let mean = s / n;
            let var = s2 / n - mean * mean;
            // Mid-tones only: not near black or white, where the sRGB curve or clipping hides noise.
            if (40.0..215.0).contains(&mean) && lo > 8.0 && hi < 250.0 && var < best.0 {
                best = (var, x, y);
            }
            x += 24;
        }
        y += 24;
    }
    Patch { x: best.1, y: best.2 }
}

/// Standard deviation of luminance in a block (display levels, 0–255), after removing a plane
/// (so a gentle gradient doesn't count as noise).
fn patch_noise(px: &[u8], w: usize, p: &Patch) -> f32 {
    let n = (BLOCK * BLOCK) as f64;
    let (mut s, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64);
    let c = (BLOCK as f64 - 1.0) / 2.0;
    for j in 0..BLOCK {
        for i in 0..BLOCK {
            let v = luma(px, w, p.x + i, p.y + j) as f64;
            s += v;
            sx += v * (i as f64 - c);
            sy += v * (j as f64 - c);
        }
    }
    let mean = s / n;
    let sxx: f64 = (0..BLOCK).map(|i| (i as f64 - c).powi(2)).sum::<f64>() * BLOCK as f64;
    let (ax, ay) = (sx / sxx, sy / sxx);
    let mut ss = 0.0f64;
    for j in 0..BLOCK {
        for i in 0..BLOCK {
            let v = luma(px, w, p.x + i, p.y + j) as f64;
            let fit = mean + ax * (i as f64 - c) + ay * (j as f64 - c);
            ss += (v - fit).powi(2);
        }
    }
    (ss / n).sqrt() as f32
}

/// The finest-scale noise of a block: the standard deviation of the difference between each
/// pixel and its B3 blur (which structure at larger scales barely touches), over 0.89, the factor
/// that makes white noise of sigma 1 read 1. For luminance, and for chroma (R-G and B-G together).
fn fine_noise(px: &[u8], w: usize, p: &Patch, chroma: bool) -> f32 {
    let value = |x: usize, y: usize| -> [f32; 2] {
        let q = &px[(y * w + x) * 4..];
        if chroma { [q[0] as f32 - q[1] as f32, q[2] as f32 - q[1] as f32] } else { [luma(px, w, x, y), 0.0] }
    };
    const K: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
    let (mut ss, mut n) = (0.0f64, 0.0f64);
    for j in 2..BLOCK - 2 {
        for i in 2..BLOCK - 2 {
            let mut blur = [0.0f32; 2];
            for (a, ka) in K.iter().enumerate() {
                for (b, kb) in K.iter().enumerate() {
                    let v = value(p.x + i + b - 2, p.y + j + a - 2);
                    blur[0] += ka * kb * v[0];
                    blur[1] += ka * kb * v[1];
                }
            }
            let v = value(p.x + i, p.y + j);
            ss += ((v[0] - blur[0]).powi(2) + (v[1] - blur[1]).powi(2)) as f64;
            n += 1.0;
        }
    }
    ((ss / n).sqrt() / 0.89) as f32
}


fn render_rgba(gpu: &mut Gpu, image: &Image, edit: &Edit, zoom: Zoom, target: &Target) -> Vec<u8> {
    gpu.begin().unwrap();
    render_image(gpu, image, edit, zoom, target);
    gpu.submit().unwrap();
    read_target(gpu, target).unwrap()
}

fn crop(px: &[u8], w: usize, x: usize, y: usize, cw: usize, ch: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(cw * ch * 4);
    for j in 0..ch {
        out.extend_from_slice(&px[((y + j) * w + x) * 4..((y + j) * w + x + cw) * 4]);
    }
    out
}

/// Side-by-side crops, each `cw` wide, with a 6-pixel gap.
fn side_by_side(crops: &[Vec<u8>], cw: usize, ch: usize) -> (usize, Vec<u8>) {
    let gap = 6;
    let total = crops.len() * cw + (crops.len() - 1) * gap;
    let mut out = vec![255u8; total * ch * 4];
    for (k, c) in crops.iter().enumerate() {
        for j in 0..ch {
            let dst = (j * total + k * (cw + gap)) * 4;
            out[dst..dst + cw * 4].copy_from_slice(&c[j * cw * 4..(j + 1) * cw * 4]);
        }
    }
    (total, out)
}

fn save(path: &Path, px: &[u8], w: usize, h: usize) {
    image::save_buffer(path, px, w as u32, h as u32, image::ExtendedColorType::Rgba8).unwrap();
}

fn med(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Mean absolute difference over two RGBA images of the same size (RGB only), and the largest.
fn diff(a: &[u8], b: &[u8]) -> (f64, u8) {
    let mut sum = 0u64;
    let mut worst = 0u8;
    let mut n = 0u64;
    for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for c in 0..3 {
            let d = p[c].abs_diff(q[c]);
            sum += d as u64;
            worst = worst.max(d);
            n += 1;
        }
    }
    (sum as f64 / n as f64, worst)
}

pub fn bench_detail(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {} ({})", gpu.adapter_name, if gpu.uma { "shared memory" } else { "own memory" });
    warm_up(&mut gpu).unwrap();
    let out_dir = std::env::current_exe().unwrap().parent().unwrap().join("detail");
    std::fs::create_dir_all(&out_dir).unwrap();

    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let frame = match decode::decode_file(f) {
            Ok(frame) => frame,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        println!("\n=== {name} ({} {}, {}x{}) ===", frame.make, frame.model, frame.width, frame.height);
        let mut memory = ImageMemory::new();
        let (image, _) = Image::open(&mut gpu, &mut memory, &frame).unwrap();
        let noise = image.ensure_noise(&mut gpu).unwrap();
        println!("noise estimate at level 0 (stabilised sigma Y, A, B): {:.5} {:.5} {:.5}", noise[0], noise[1], noise[2]);
        let (w, h) = image.oriented_size();

        // ---- Noise reduction at 100% ----
        let (vw, vh) = (1600usize, 1000usize);
        let target = Target::new(&gpu, vw as u32, vh as u32).unwrap();
        let zoom = Zoom::Actual { x: 0.5, y: 0.5 };
        let neutral = Detail { amount: 0.0, color: 0.0, luminance: 0.0, ..DETAIL_DEFAULTS };
        let with = |d: Detail| {
            let mut e = Edit::default();
            e.set_detail(d);
            e
        };
        // The first frame that runs the pass makes its memory (once per session).
        let start = Instant::now();
        render_rgba(&mut gpu, &image, &with(DETAIL_DEFAULTS), zoom, &target);
        println!("first frame with the detail pass (allocates its memory): {:.1} ms wall", start.elapsed().as_secs_f64() * 1000.0);
        let off = render_rgba(&mut gpu, &image, &with(neutral), zoom, &target);
        // The flat patch is found in the picture with no detail processing, then measured everywhere.
        let patch = find_flat(&off, vw, vh);
        println!("flat patch at ({}, {}) of the {vw}x{vh} view at 100% (the lowest-variance {BLOCK}x{BLOCK} block)", patch.x, patch.y);
        println!(
            "  no detail processing           : luminance sigma {:.2} (finest scale; all scales {:.2}), chroma sigma {:.2} (display levels)",
            fine_noise(&off, vw, &patch, false),
            patch_noise(&off, vw, &patch),
            fine_noise(&off, vw, &patch, true)
        );
        let base_noise = fine_noise(&off, vw, &patch, false);
        let base_total = patch_noise(&off, vw, &patch);
        let base_chroma = fine_noise(&off, vw, &patch, true);
        for (label, d) in [("color 25 only", Detail { amount: 0.0, ..DETAIL_DEFAULTS }), ("sharpening 40 only", Detail { color: 0.0, ..DETAIL_DEFAULTS }), ("default", DETAIL_DEFAULTS), ("luminance 60", Detail { luminance: 60.0, ..DETAIL_DEFAULTS })] {
            let px = render_rgba(&mut gpu, &image, &with(d), zoom, &target);
            println!("  mean brightness of the flat patch, {label}: {:.2} (off {:.2})", patch_mean(&px, vw, &patch), patch_mean(&off, vw, &patch));
        }
        for lum in [0.0, 25.0, 50.0, 75.0, 100.0] {
            let px = render_rgba(&mut gpu, &image, &with(Detail { luminance: lum, color: 0.0, amount: 0.0, ..DETAIL_DEFAULTS }), zoom, &target);
            let s = fine_noise(&px, vw, &patch, false);
            let total = patch_noise(&px, vw, &patch);
            println!("  Luminance {lum:>3.0} (Color 0, no sharpening): finest-scale sigma {s:.2} ({:.0}% of unprocessed); all scales sigma {total:.2} ({:.0}%)", 100.0 * s / base_noise, 100.0 * total / base_total);
        }
        for color in [0.0, 25.0, 50.0, 100.0] {
            let px = render_rgba(&mut gpu, &image, &with(Detail { color, luminance: 0.0, amount: 0.0, ..DETAIL_DEFAULTS }), zoom, &target);
            let s = fine_noise(&px, vw, &patch, true);
            println!("  Color {color:>3.0} (Luminance 0, no sharpening): chroma sigma {s:.2} ({:.0}% of unprocessed)", 100.0 * s / base_chroma);
        }

        // ---- Crops ----
        // A busy-but-visible area around the flat patch, 300 x 220 at 100%.
        let (cw, ch) = (300usize, 220usize);
        let cx = patch.x.saturating_sub(cw / 2 - BLOCK / 2).min(vw - cw);
        let cy = patch.y.saturating_sub(ch / 2 - BLOCK / 2).min(vh - ch);
        let strong = Detail { luminance: 70.0, color: 60.0, amount: 0.0, ..DETAIL_DEFAULTS };
        let lum30 = Detail { luminance: 30.0, ..DETAIL_DEFAULTS };
        let lum60 = Detail { luminance: 60.0, ..DETAIL_DEFAULTS };
        let variants: Vec<(&str, Detail)> = vec![
            ("off", neutral),
            ("default", DETAIL_DEFAULTS),
            ("Luminance 30, Color 25", lum30),
            ("nr strong (lum 70, color 60)", strong),
            ("nr strong + sharpen 40", Detail { amount: 40.0, ..strong }),
        ];
        let crops: Vec<Vec<u8>> = variants
            .iter()
            .map(|(_, d)| crop(&render_rgba(&mut gpu, &image, &with(*d), zoom, &target), vw, cx, cy, cw, ch))
            .collect();
        let (tw, px) = side_by_side(&crops, cw, ch);
        save(&out_dir.join(format!("{name}-nr.png")), &px, tw, ch);
        println!("  crops (left to right: {}) -> {name}-nr.png", variants.iter().map(|v| v.0).collect::<Vec<_>>().join(" | "));

        // The same four looks at two places: the flat patch, and the busiest mid-tone block.
        let busy = find_busy(&off, vw, vh);
        let compare: [(&str, Detail); 4] = [("off", neutral), ("default", DETAIL_DEFAULTS), ("lum30", lum30), ("lum60", lum60)];
        let renders: Vec<Vec<u8>> = compare.iter().map(|(_, d)| render_rgba(&mut gpu, &image, &with(*d), zoom, &target)).collect();
        for (place, p) in [("flat", &patch), ("busy", &busy)] {
            let (bx, by) = (p.x.saturating_sub(cw / 2 - BLOCK / 2).min(vw - cw), p.y.saturating_sub(ch / 2 - BLOCK / 2).min(vh - ch));
            let crops: Vec<Vec<u8>> = renders.iter().map(|r| crop(r, vw, bx, by, cw, ch)).collect();
            let (tw, px) = side_by_side(&crops, cw, ch);
            save(&out_dir.join(format!("{name}-compare-{place}.png")), &px, tw, ch);
        }
        println!("  100% crops off | default | Luminance 30 | Luminance 60 -> {name}-compare-flat.png, {name}-compare-busy.png");

        // Sharpening 0 / 40 / 100 on the same area, and the mask.
        let sharp: Vec<Vec<u8>> = [0.0, 40.0, 100.0]
            .iter()
            .map(|&a| crop(&render_rgba(&mut gpu, &image, &with(Detail { amount: a, ..DETAIL_DEFAULTS }), zoom, &target), vw, cx, cy, cw, ch))
            .collect();
        let (tw, px) = side_by_side(&sharp, cw, ch);
        save(&out_dir.join(format!("{name}-sharpen.png")), &px, tw, ch);
        println!("  sharpening 0 / 40 / 100 -> {name}-sharpen.png");
        // The middle of the view, where there is usually something sharp to look at, doubled for the eye.
        let (mx, my) = (vw / 2 - 150, vh / 2 - 110);
        let mid: Vec<Vec<u8>> = [0.0, 40.0, 100.0]
            .iter()
            .map(|&a| crop(&render_rgba(&mut gpu, &image, &with(Detail { amount: a, ..DETAIL_DEFAULTS }), zoom, &target), vw, mx, my, 150, 110))
            .collect();
        let (tw, px) = side_by_side(&mid, 150, 110);
        let mut big = Vec::with_capacity(tw * 110 * 4 * 4);
        for y in 0..110 {
            for _ in 0..2 {
                for x in 0..tw {
                    for _ in 0..2 {
                        big.extend_from_slice(&px[(y * tw + x) * 4..(y * tw + x) * 4 + 4]);
                    }
                }
            }
        }
        save(&out_dir.join(format!("{name}-sharpen-centre.png")), &big, tw * 2, 220);

        // ---- Timing at 1440p ----
        println!("  detail pass timing (median of 20, GPU ms; the pass = frame that runs it − frame that reuses it):");
        let t1440 = Target::new(&gpu, 2560, 1440).unwrap();
        // The GPU clocks up under load; run a while before timing.
        for _ in 0..150 {
            gpu.begin().unwrap();
            render_image(&gpu, &image, &with(DETAIL_DEFAULTS), Zoom::Fit, &t1440);
            gpu.submit().unwrap();
            gpu.detail.borrow_mut().invalidate();
        }
        let cases: Vec<(&str, Zoom, Detail)> = vec![
            ("Fit, defaults", Zoom::Fit, DETAIL_DEFAULTS),
            ("Fit, strong NR", Zoom::Fit, Detail { luminance: 80.0, color: 80.0, ..DETAIL_DEFAULTS }),
            ("100%, defaults", Zoom::Actual { x: 0.5, y: 0.5 }, DETAIL_DEFAULTS),
            ("100%, strong NR", Zoom::Actual { x: 0.5, y: 0.5 }, Detail { luminance: 80.0, color: 80.0, ..DETAIL_DEFAULTS }),
            ("100%, sharpening only", Zoom::Actual { x: 0.5, y: 0.5 }, Detail { color: 0.0, ..DETAIL_DEFAULTS }),
        ];
        let frame_ms = |gpu: &mut Gpu, edit: &Edit, zoom: Zoom, rerun: bool| -> f64 {
            if rerun {
                gpu.detail.borrow_mut().invalidate();
            }
            gpu.begin().unwrap();
            let a = gpu.timestamp();
            render_image(gpu, &image, edit, zoom, &t1440);
            let b = gpu.timestamp();
            let ts = gpu.submit().unwrap();
            ts[b as usize] - ts[a as usize]
        };
        for (label, zoom, d) in &cases {
            let edit = with(*d);
            frame_ms(&mut gpu, &edit, *zoom, true);
            let full = med((0..20).map(|_| frame_ms(&mut gpu, &edit, *zoom, true)).collect());
            let reused = med((0..20).map(|_| frame_ms(&mut gpu, &edit, *zoom, false)).collect());
            println!("    {label:<24} frame {full:.3} ms, reusing the texture {reused:.3} ms: the pass {:.3} ms", full - reused);
        }
        for (label, zoom) in [("Fit", Zoom::Fit), ("100%", Zoom::Actual { x: 0.5, y: 0.5 })] {
            let neutral_ms = med((0..20).map(|_| frame_ms(&mut gpu, &with(neutral), zoom, false)).collect());
            println!("    develop pass alone, detail neutral, {label}: {neutral_ms:.3} ms");
        }

        // ---- A tone drag does not rerun the pass ----
        frame_ms(&mut gpu, &Edit::default(), Zoom::Actual { x: 0.5, y: 0.5 }, false);
        let before = gpu.detail.borrow().runs;
        for i in 0..30 {
            let e = Edit { exposure: i as f32 / 15.0 - 1.0, contrast: i as f32, shadows: 20.0, ..Default::default() };
            frame_ms(&mut gpu, &e, Zoom::Actual { x: 0.5, y: 0.5 }, false);
        }
        let after = gpu.detail.borrow().runs;
        {
            let t4k = Target::new(&gpu, 3840, 2160).unwrap();
            let mut ms = Vec::new();
            for _ in 0..8 {
                gpu.detail.borrow_mut().invalidate();
                gpu.begin().unwrap();
                let a = gpu.timestamp();
                render_image(&gpu, &image, &with(DETAIL_DEFAULTS), Zoom::Actual { x: 0.5, y: 0.5 }, &t4k);
                let b = gpu.timestamp();
                let ts = gpu.submit().unwrap();
                ms.push(ts[b as usize] - ts[a as usize]);
            }
            println!("  4K (3840x2160) at 100%, defaults: frame {:.2} ms with the pass; memory {:.1} MB", med(ms), gpu.detail.borrow().bytes() as f64 / 1048576.0);
        }
        println!("  GPU memory held by the detail pass now: {:.1} MB (scratch + the 1440p output)", gpu.detail.borrow().bytes() as f64 / 1048576.0);
        println!("  30 frames of exposure/contrast/shadows drag: detail pass ran {} time(s)", after - before);
        let before = gpu.detail.borrow().runs;
        for i in 0..10 {
            let mut e = Edit::default();
            e.set_detail(Detail { amount: 40.0 + i as f32 * 5.0, ..DETAIL_DEFAULTS });
            frame_ms(&mut gpu, &e, Zoom::Actual { x: 0.5, y: 0.5 }, false);
        }
        println!("  10 frames of a sharpening Amount drag: detail pass ran {} time(s)", gpu.detail.borrow().runs - before);
        frame_ms(&mut gpu, &Edit::default(), Zoom::Actual { x: 0.5, y: 0.5 }, false);
        let before = gpu.detail.borrow().runs;
        for i in 0..10 {
            let e = Edit { temperature: 4000.0 + 300.0 * i as f32, tint: 5.0, custom_white_balance: 1, ..Default::default() };
            frame_ms(&mut gpu, &e, Zoom::Actual { x: 0.5, y: 0.5 }, false);
        }
        println!("  10 frames of a white balance drag: detail pass ran {} time(s)", gpu.detail.borrow().runs - before);

        // ---- Tiles agree at the seam ----
        let edit = with(Detail { luminance: 50.0, color: 50.0, amount: 80.0, masking: 30.0, ..DETAIL_DEFAULTS });
        let region = |gpu: &mut Gpu, x: u32, y: u32, rw: u32, rh: u32| {
            let t = Target::new(gpu, rw, rh).unwrap();
            gpu.begin().unwrap();
            render_image(gpu, &image, &edit, Zoom::Region { x: x as f32, y: y as f32, scale: 1.0 }, &t);
            gpu.submit().unwrap();
            read_target(gpu, &t).unwrap()
        };
        let (sx, sy) = (w / 2 - 300, h / 2 - 300);
        let whole = region(&mut gpu, sx, sy, 600, 600);
        // Two overlapping tiles of 400 wide (the left one reaching x = 399, the right one starting at x = 200).
        let left = region(&mut gpu, sx, sy, 400, 600);
        let right = region(&mut gpu, sx + 200, sy, 400, 600);
        let mut worst = 0u8;
        let mut seam = 0u8;
        for y in 0..600usize {
            for x in 0..400usize {
                for c in 0..3 {
                    let l = left[(y * 400 + x) * 4 + c];
                    let a = whole[(y * 600 + x) * 4 + c];
                    worst = worst.max(l.abs_diff(a));
                    let r = right[(y * 400 + x) * 4 + c];
                    let a2 = whole[(y * 600 + x + 200) * 4 + c];
                    worst = worst.max(r.abs_diff(a2));
                    if x >= 200 {
                        seam = seam.max(l.abs_diff(right[(y * 400 + x - 200) * 4 + c]));
                    }
                }
            }
        }
        println!("  two overlapping tiles vs one: largest difference {worst} levels (overlap of the two tiles: {seam})");

        // A rotated tile pair.
        let edit_rot = Edit { angle: 7.0, ..edit };
        let region_rot = |gpu: &mut Gpu, x: u32, y: u32, rw: u32, rh: u32| {
            let t = Target::new(gpu, rw, rh).unwrap();
            gpu.begin().unwrap();
            render_image(gpu, &image, &edit_rot, Zoom::Region { x: x as f32, y: y as f32, scale: 1.0 }, &t);
            gpu.submit().unwrap();
            read_target(gpu, &t).unwrap()
        };
        let whole = region_rot(&mut gpu, sx, sy, 600, 600);
        let right = region_rot(&mut gpu, sx + 200, sy, 400, 600);
        let mut worst = 0u8;
        for y in 0..600usize {
            for x in 0..400usize {
                for c in 0..3 {
                    worst = worst.max(right[(y * 400 + x) * 4 + c].abs_diff(whole[(y * 600 + x + 200) * 4 + c]));
                }
            }
        }
        println!("  straightened 7°, tile vs whole: largest difference {worst} levels");

        // ---- The export, tiles and all, against the real thing ----
        let window = export::window(image.oriented_size(), &edit);
        let _ = (window, TileRenderer::new);

        // ---- Fit against the export downscaled ----
        let fit_edit = with(Detail { luminance: 40.0, color: 40.0, amount: 60.0, ..DETAIL_DEFAULTS });
        let (fw, fh) = (1500u32, (1500.0 * h as f64 / w as f64).round() as u32);
        let t_fit = Target::new(&gpu, fw, fh).unwrap();
        let fit = render_rgba(&mut gpu, &image, &fit_edit, Zoom::Fit, &t_fit);
        // The "export": the whole frame at 1:1 in tiles' worth (here one render when it fits the
        // detail memory, else strips), reduced to the fit size by a box filter.
        let scale = w as f64 / fw as f64;
        let mut full = vec![0f32; (fw * fh * 3) as usize];
        let mut count = vec![0f32; (fw * fh) as usize];
        let strip = 1024u32;
        let mut y0 = 0u32;
        while y0 < h {
            let hh = strip.min(h - y0);
            // Wide strips are done in column tiles to stay within the detail memory.
            let mut x0 = 0u32;
            while x0 < w {
                let ww = 2048.min(w - x0);
                let t = Target::new(&gpu, ww, hh).unwrap();
                gpu.begin().unwrap();
                render_image(&gpu, &image, &fit_edit, Zoom::Region { x: x0 as f32, y: y0 as f32, scale: 1.0 }, &t);
                gpu.submit().unwrap();
                let px = read_target(&mut gpu, &t).unwrap();
                for j in 0..hh {
                    let fy = (((y0 + j) as f64 + 0.5) / scale) as u32;
                    for i in 0..ww {
                        let fx = (((x0 + i) as f64 + 0.5) / scale) as u32;
                        if fx < fw && fy < fh {
                            count[(fy * fw + fx) as usize] += 1.0;
                            for c in 0..3 {
                                full[((fy * fw + fx) * 3) as usize + c] += px[((j * ww + i) * 4) as usize + c] as f32;
                            }
                        }
                    }
                }
                x0 += ww;
            }
            y0 += hh;
        }

        let down: Vec<u8> = full.as_chunks::<3>().0.iter().zip(&count).flat_map(|(p, n)| [(p[0] / n).round() as u8, (p[1] / n).round() as u8, (p[2] / n).round() as u8, 255]).collect();
        // Compare away from the borders (the box counts at the edge are partial).
        let m = 8usize;
        let inner = |px: &[u8]| crop(px, fw as usize, m, m, fw as usize - 2 * m, fh as usize - 2 * m);
        let (mean, worst) = diff(&inner(&fit), &inner(&down));
        let plain_edit = with(neutral);
        let fit_plain = render_rgba(&mut gpu, &image, &plain_edit, Zoom::Fit, &t_fit);
        let (mean0, _) = diff(&inner(&fit_plain), &inner(&fit));
        println!("  Fit {fw}x{fh} vs the 1:1 render box-filtered down: mean difference {mean:.2} levels (largest {worst}); detail off vs on at Fit: {mean0:.2}");
        save(&out_dir.join(format!("{name}-fit.png")), &fit, fw as usize, fh as usize);
        save(&out_dir.join(format!("{name}-fit-from-export.png")), &down, fw as usize, fh as usize);
        // Noise on the flat patch at Fit, to see NR carry across.
        let pf = find_flat(&fit_plain, fw as usize, fh as usize);
        println!(
            "  noise at Fit in the flattest block: off {:.2}, with the settings {:.2}, from the downscaled export {:.2}",
            fine_noise(&fit_plain, fw as usize, &pf, false),
            fine_noise(&fit, fw as usize, &pf, false),
            fine_noise(&down, fw as usize, &pf, false)
        );
        let _ = (Layer::Raw, render, read_rgba16f, f16_to_f32);
        let _ = Instant::now();
    }
    println!("\ncrops in {}", out_dir.display());
}

/// `bench noisescale <raw>...`: how the noise looks to the wavelet scales and the pyramid, to
/// calibrate `detail.rs`'s constants. For each level of the pyramid, the finest-scale noise
/// estimate of the central crop; for each wavelet scale, the standard deviation (from the median
/// absolute value of the quietest tenth of 32 x 32 blocks) against what white noise of the finest
/// scale's size would give.
pub fn bench_noisescale(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    warm_up(&mut gpu).unwrap();
    for f in files {
        let frame = decode::decode_file(f).unwrap();
        println!("\n=== {} ({} {}) ===", f.file_name().unwrap().to_string_lossy(), frame.make, frame.model);
        let mut memory = ImageMemory::new();
        let (image, _) = Image::open(&mut gpu, &mut memory, &frame).unwrap();
        let mut s0 = [0f32; 3];
        for level in 0..4u32 {
            let (lw, lh) = detail::level_size((image.width, image.height), level);
            let side = 512u32 >> level.min(1);
            let (w, h) = (side.min(lw), side.min(lh));
            let (x, y) = ((lw - w) / 2, (lh - h) / 2);
            let px = read_rgba16f(&mut gpu, &image.linear, level, (x, y, w, h)).unwrap();
            let rgb: Vec<[f32; 3]> = px.iter().map(|p| [p[0], p[1], p[2]]).collect();
            let n = detail::estimate_noise(&rgb, w as usize, h as usize);
            if level == 0 {
                s0 = n;
            }
            println!(
                "  level {level}: sigma Y {:.5} A {:.5} B {:.5}   relative to level 0: {:.3} {:.3} {:.3}   (white noise: {:.3})",
                n[0], n[1], n[2], n[0] / s0[0], n[1] / s0[1], n[2] / s0[2], 0.5f32.powi(level as i32)
            );
        }
        // Wavelet scales on the level-0 crop.
        let (w, h) = (512usize.min(image.width as usize), 512usize.min(image.height as usize));
        let (x, y) = ((image.width as usize - w) / 2, (image.height as usize - h) / 2);
        let px = read_rgba16f(&mut gpu, &image.linear, 0, (x as u32, y as u32, w as u32, h as u32)).unwrap();
        let opp: Vec<[f32; 3]> = px.iter().map(|p| detail::stabilise([p[0], p[1], p[2]])).collect();
        let k = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0f32];
        let at = |v: usize, i: usize, step: usize, n: usize| (v as isize + (i as isize - 2) * step as isize).clamp(0, n as isize - 1) as usize;
        let mut cur = opp;
        let mut first = [0f32; 3];
        for j in 0..detail::SCALES {
            let step = 1usize << j;
            let mut t = vec![[0f32; 3]; w * h];
            for yy in 0..h {
                for xx in 0..w {
                    for c in 0..3 {
                        t[yy * w + xx][c] = (0..5).map(|i| k[i] * cur[yy * w + at(xx, i, step, w)][c]).sum();
                    }
                }
            }
            let mut next = vec![[0f32; 3]; w * h];
            for yy in 0..h {
                for xx in 0..w {
                    for c in 0..3 {
                        next[yy * w + xx][c] = (0..5).map(|i| k[i] * t[at(yy, i, step, h) * w + xx][c]).sum();
                    }
                }
            }
            // Quietest tenth of the 32 x 32 blocks: the median |d| there.
            let mut sig = [0f32; 3];
            for c in 0..3 {
                let mut meds = Vec::new();
                for by in (0..h - 32).step_by(32) {
                    for bx in (0..w - 32).step_by(32) {
                        let mut v: Vec<f32> = (0..32).flat_map(|j| (0..32).map(move |i| (i, j))).map(|(i, jj)| (cur[(by + jj) * w + bx + i][c] - next[(by + jj) * w + bx + i][c]).abs()).collect();
                        v.sort_by(|a, b| a.total_cmp(b));
                        meds.push(v[v.len() / 2]);
                    }
                }
                meds.sort_by(|a, b| a.total_cmp(b));
                sig[c] = meds[meds.len() / 10] / 0.6745;
            }
            if j == 0 {
                first = sig;
            }
            let white = detail::SCALE_NOISE[j] / detail::SCALE_NOISE[0];
            println!(
                "  scale {j}: sigma Y {:.5} A {:.5} B {:.5}   vs white noise from scale 0: {:.2} {:.2} {:.2}x",
                sig[0], sig[1], sig[2], sig[0] / (first[0] * white), sig[1] / (first[1] * white), sig[2] / (first[2] * white)
            );
            cur = next;
        }
    }
}

/// The block with the most variance among mid-tone ones: real structure to judge sharpening and
/// noise reduction on.
fn find_busy(px: &[u8], w: usize, h: usize) -> Patch {
    let mut best = (0.0f32, BLOCK, BLOCK);
    let mut y = BLOCK;
    while y + 2 * BLOCK < h {
        let mut x = BLOCK;
        while x + 2 * BLOCK < w {
            let (mut s, mut s2) = (0.0f32, 0.0f32);
            for j in 0..BLOCK {
                for i in 0..BLOCK {
                    let v = luma(px, w, x + i, y + j);
                    s += v;
                    s2 += v * v;
                }
            }
            let n = (BLOCK * BLOCK) as f32;
            let mean = s / n;
            let var = s2 / n - mean * mean;
            if (60.0..200.0).contains(&mean) && var > best.0 {
                best = (var, x, y);
            }
            x += 24;
        }
        y += 24;
    }
    Patch { x: best.1, y: best.2 }
}

/// Mean display luminance of a block.
fn patch_mean(px: &[u8], w: usize, p: &Patch) -> f32 {
    let mut s = 0.0f32;
    for j in 0..BLOCK {
        for i in 0..BLOCK {
            s += luma(px, w, p.x + i, p.y + j);
        }
    }
    s / (BLOCK * BLOCK) as f32
}

/// `bench partb <raw>...`: Texture at 100% and Clarity at Fit (crops), their cost, and Fit against
/// the export with both on.
pub fn bench_partb(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {} ({})", gpu.adapter_name, if gpu.uma { "shared memory" } else { "own memory" });
    warm_up(&mut gpu).unwrap();
    let out_dir = std::env::current_exe().unwrap().parent().unwrap().join("detail");
    std::fs::create_dir_all(&out_dir).unwrap();
    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let frame = decode::decode_file(f).unwrap();
        println!("\n=== {name} ({} {}, {}x{}) ===", frame.make, frame.model, frame.width, frame.height);
        let mut memory = ImageMemory::new();
        let (image, _) = Image::open(&mut gpu, &mut memory, &frame).unwrap();
        image.ensure_noise(&mut gpu).unwrap();
        let (w, h) = image.oriented_size();
        let with = |texture: f32, clarity: f32| Edit { texture, clarity, ..Default::default() };

        // ---- Texture at 100%: -100 / 0 / +100 at the flattest block (skin, sky) and the busiest one ----
        let (vw, vh) = (1600usize, 1000usize);
        let target = Target::new(&gpu, vw as u32, vh as u32).unwrap();
        let zoom = Zoom::Actual { x: 0.5, y: 0.5 };
        let renders: Vec<Vec<u8>> = [-100.0, 0.0, 100.0].iter().map(|&t| render_rgba(&mut gpu, &image, &with(t, 0.0), zoom, &target)).collect();
        let flat = find_flat(&renders[1], vw, vh);
        let busy = find_busy(&renders[1], vw, vh);
        let (cw, ch) = (300usize, 220usize);
        for (place, p) in [("flat", &flat), ("busy", &busy)] {
            let (bx, by) = (p.x.saturating_sub(cw / 2 - BLOCK / 2).min(vw - cw), p.y.saturating_sub(ch / 2 - BLOCK / 2).min(vh - ch));
            let crops: Vec<Vec<u8>> = renders.iter().map(|r| crop(r, vw, bx, by, cw, ch)).collect();
            let (tw, px) = side_by_side(&crops, cw, ch);
            save(&out_dir.join(format!("{name}-texture-{place}.png")), &px, tw, ch);
        }
        for (t, r) in [-100.0, 0.0, 100.0].iter().zip(&renders) {
            println!("  Texture {t:>4.0}: flat block finest-scale sigma {:.2}, all scales {:.2}", fine_noise(r, vw, &flat, false), patch_noise(r, vw, &flat));
        }
        println!("  Texture crops (-100 | 0 | +100) -> {name}-texture-flat.png, {name}-texture-busy.png");

        // ---- Clarity at Fit: -100 / 0 / +100 ----
        let (fw, fh) = (1500u32, (1500.0 * h as f64 / w as f64).round() as u32);
        let t_fit = Target::new(&gpu, fw, fh).unwrap();
        let fits: Vec<Vec<u8>> = [-100.0, 0.0, 100.0].iter().map(|&c| render_rgba(&mut gpu, &image, &with(0.0, c), Zoom::Fit, &t_fit)).collect();
        let (qw, qh) = (500usize, 375usize.min(fh as usize));
        for (place, qx, qy) in [("topleft", 0usize, 0usize), ("centre", fw as usize / 2 - 250, fh as usize / 2 - 190)] {
            let crops: Vec<Vec<u8>> = fits.iter().map(|r| crop(r, fw as usize, qx, qy, qw, qh)).collect();
            let (tw, px) = side_by_side(&crops, qw, qh);
            save(&out_dir.join(format!("{name}-clarity-{place}.png")), &px, tw, qh);
        }
        for (c, r) in [-100, 0, 100].iter().zip(&fits) {
            save(&out_dir.join(format!("{name}-clarity{c}.png")), r, fw as usize, fh as usize);
        }
        {
            let (c0, cn, cp) = (local_contrast(&fits[1], &fits[1], fw as usize, fh as usize), local_contrast(&fits[0], &fits[1], fw as usize, fh as usize), local_contrast(&fits[2], &fits[1], fw as usize, fh as usize));
            println!("  (mean difference between the -100 and +100 renders: {:.2} levels)", diff(&fits[0], &fits[2]).0);
            println!("  local contrast at Clarity scale in the mid-tones (std of log2 Y minus the guided base): 0: {c0:.3}; -100: {cn:.3} ({:+.0}%); +100: {cp:.3} ({:+.0}%)", (cn / c0 - 1.0) * 100.0, (cp / c0 - 1.0) * 100.0);
        }
        println!("  Clarity crops at Fit (-100 | 0 | +100) -> {name}-clarity-topleft.png, {name}-clarity-centre.png, full frames {name}-clarity-100.png ...");

        // Halo check: 100% views along strong edges (column against sky, the awning's edge), Clarity -100 | 0 | +100.
        for (place, fx, fy) in [("edge-sky", 0.08f32, 0.30f32), ("edge-column", 0.30, 0.45), ("edge-roof", 0.12, 0.10)] {
            let z = Zoom::Actual { x: fx, y: fy };
            let r: Vec<Vec<u8>> = [-100.0, 0.0, 100.0].iter().map(|&c| render_rgba(&mut gpu, &image, &with(0.0, c), z, &target)).collect();
            let crops: Vec<Vec<u8>> = r.iter().map(|p| crop(p, vw, vw / 2 - 250, vh / 2 - 190, 500, 380)).collect();
            let (tw, px) = side_by_side(&crops, 500, 380);
            save(&out_dir.join(format!("{name}-clarity-{place}-100pct.png")), &px, tw, 380);
        }
        // ---- Timing at 1440p ----
        let t1440 = Target::new(&gpu, 2560, 1440).unwrap();
        for _ in 0..150 {
            gpu.begin().unwrap();
            render_image(&gpu, &image, &with(0.0, 0.0), Zoom::Fit, &t1440);
            gpu.submit().unwrap();
            gpu.detail.borrow_mut().invalidate();
        }
        let frame_ms = |gpu: &mut Gpu, edit: &Edit, zoom: Zoom, rerun_detail: bool, rerun_clarity: bool| -> f64 {
            if rerun_detail {
                gpu.detail.borrow_mut().invalidate();
            }
            if rerun_clarity {
                gpu.clarity_key.set(None);
            }
            gpu.begin().unwrap();
            let a = gpu.timestamp();
            render_image(gpu, &image, edit, zoom, &t1440);
            let b = gpu.timestamp();
            let ts = gpu.submit().unwrap();
            ts[b as usize] - ts[a as usize]
        };
        let at100 = Zoom::Actual { x: 0.5, y: 0.5 };
        let def = Edit::default();
        let measure = |gpu: &mut Gpu, e: &Edit, z: Zoom, rd: bool, rc: bool| {
            frame_ms(gpu, e, z, rd, rc);
            med((0..20).map(|_| frame_ms(gpu, e, z, rd, rc)).collect())
        };
        let reused_def = measure(&mut gpu, &def, at100, false, false);
        let ran_def = measure(&mut gpu, &def, at100, true, false);
        println!("  detail pass at 100%, 1440p, defaults: {:.2} ms", ran_def - reused_def);
        for t in [60.0, -60.0] {
            let e = with(t, 0.0);
            let reused = measure(&mut gpu, &e, at100, false, false);
            let ran = measure(&mut gpu, &e, at100, true, false);
            println!("  detail pass at 100%, 1440p, defaults + Texture {t:+.0}: {:.2} ms", ran - reused);
        }
        let plain = measure(&mut gpu, &def, Zoom::Fit, false, false);
        let clar = with(0.0, 50.0);
        let with_base = measure(&mut gpu, &clar, Zoom::Fit, false, true);
        let cached = measure(&mut gpu, &clar, Zoom::Fit, false, false);
        println!("  Clarity, 1440p Fit: no Clarity {plain:.3} ms; Clarity with the base rebuilt {with_base:.3} ms (+{:.3}); base reused (a Clarity drag) {cached:.3} ms (+{:.3})", with_base - plain, cached - plain);
        let plain100 = measure(&mut gpu, &def, at100, false, false);
        let clar100 = measure(&mut gpu, &clar, at100, false, false);
        println!("  at 100%: {clar100:.3} ms with Clarity (base reused) against {plain100:.3} ms without");

        // ---- Fit against the export downscaled, with both on ----
        let edit = Edit { texture: 40.0, clarity: 40.0, ..Default::default() };
        let fit = render_rgba(&mut gpu, &image, &edit, Zoom::Fit, &t_fit);
        let scale = w as f64 / fw as f64;
        let mut full = vec![0f32; (fw * fh * 3) as usize];
        let mut count = vec![0f32; (fw * fh) as usize];
        let mut y0 = 0u32;
        while y0 < h {
            let hh = 1024.min(h - y0);
            let mut x0 = 0u32;
            while x0 < w {
                let ww = 2048.min(w - x0);
                let t = Target::new(&gpu, ww, hh).unwrap();
                gpu.begin().unwrap();
                render_image(&gpu, &image, &edit, Zoom::Region { x: x0 as f32, y: y0 as f32, scale: 1.0 }, &t);
                gpu.submit().unwrap();
                let px = read_target(&mut gpu, &t).unwrap();
                for j in 0..hh {
                    let fy = (((y0 + j) as f64 + 0.5) / scale) as u32;
                    for i in 0..ww {
                        let fx = (((x0 + i) as f64 + 0.5) / scale) as u32;
                        if fx < fw && fy < fh {
                            count[(fy * fw + fx) as usize] += 1.0;
                            for c in 0..3 {
                                full[((fy * fw + fx) * 3) as usize + c] += px[((j * ww + i) * 4) as usize + c] as f32;
                            }
                        }
                    }
                }
                x0 += ww;
            }
            y0 += hh;
        }
        let down: Vec<u8> = full.as_chunks::<3>().0.iter().zip(&count).flat_map(|(p, n)| [(p[0] / n).round() as u8, (p[1] / n).round() as u8, (p[2] / n).round() as u8, 255]).collect();
        let m = 8usize;
        let inner = |px: &[u8]| crop(px, fw as usize, m, m, fw as usize - 2 * m, fh as usize - 2 * m);
        let (mean, worst) = diff(&inner(&fit), &inner(&down));
        let base = render_rgba(&mut gpu, &image, &Edit::default(), Zoom::Fit, &t_fit);
        let (mean0, _) = diff(&inner(&base), &inner(&fit));
        println!("  Fit {fw}x{fh} with Texture 40 + Clarity 40 vs the 1:1 render box-filtered down: mean difference {mean:.2} levels (largest {worst}); the two settings change Fit by {mean0:.2} on average");
        let region = |gpu: &mut Gpu, x: u32, y: u32, rw: u32, rh: u32| {
            let t = Target::new(gpu, rw, rh).unwrap();
            gpu.begin().unwrap();
            render_image(gpu, &image, &edit, Zoom::Region { x: x as f32, y: y as f32, scale: 1.0 }, &t);
            gpu.submit().unwrap();
            read_target(gpu, &t).unwrap()
        };
        let (sx, sy) = (w / 2 - 300, h / 2 - 300);
        let whole = region(&mut gpu, sx, sy, 600, 600);
        let right = region(&mut gpu, sx + 200, sy, 400, 600);
        let mut worst = 0u8;
        for y in 0..600usize {
            for x in 0..400usize {
                for c in 0..3 {
                    worst = worst.max(right[(y * 400 + x) * 4 + c].abs_diff(whole[(y * 600 + x + 200) * 4 + c]));
                }
            }
        }
        println!("  tile vs whole with both on: largest difference {worst} levels");
    }
}

/// How much local contrast an RGBA image has at Clarity's scale: the standard deviation of
/// x - base, with x = log2(Y + 0.01) and base the guided filter Clarity itself uses (same copy,
/// radius and epsilon), over the pixels whose display luma (in `mask_from`, the same pixels for
/// every setting) is between 0.1 and 0.6.
fn local_contrast(px: &[u8], mask_from: &[u8], w: usize, h: usize) -> f32 {
    // The base is always the unadjusted picture's (`mask_from`), so the settings are compared against the same base.
    use windedit_core::tone;
    let y = |i: usize| {
        let c = |v: u8| {
            let s = v as f32 / 255.0;
            if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
        };
        (0.2126 * c(px[i * 4]) + 0.7152 * c(px[i * 4 + 1]) + 0.0722 * c(px[i * 4 + 2]) + 0.01).log2()
    };
    let l: Vec<f32> = (0..w * h).map(y).collect();
    let edge = w.max(h) as f32;
    let factor = ((edge / tone::CLARITY_EDGE).ceil() as usize).max(1);
    let radius = ((edge * tone::CLARITY_RADIUS / factor as f32).round() as usize).max(1);
    let l0: Vec<f32> = (0..w * h).map(|i| { let c = |v: u8| { let s = v as f32 / 255.0; if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) } }; (0.2126 * c(mask_from[i * 4]) + 0.7152 * c(mask_from[i * 4 + 1]) + 0.0722 * c(mask_from[i * 4 + 2]) + 0.01).log2() }).collect();
    let (small, sw, sh) = tone::small_copy(&l0, w, h, factor);
    let base = tone::base_from_small(&small, sw, sh, &l0, w, h, factor, radius, tone::CLARITY_EPS);
    let (mut s, mut s2, mut n) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..w * h {
        let m = &mask_from[i * 4..];
        let luma = (0.299 * m[0] as f32 + 0.587 * m[1] as f32 + 0.114 * m[2] as f32) / 255.0;
        if (0.1..0.6).contains(&luma) {
            let d = (l[i] - base[i]) as f64;
            s += d;
            s2 += d * d;
            n += 1.0;
        }
    }
    ((s2 / n - (s / n).powi(2)).max(0.0)).sqrt() as f32
}
