//! `bench lens <raw>`: checks lens correction against the camera's own JPEG, which the camera
//! has already corrected. Renders the raw at Fit about 1024 px wide, with and without the
//! correction, finds the scale and shift that best align each with the embedded JPEG (edge
//! correlation), and reports what misalignment is left at the centre, edges and corners.
//! Also `bench lensscan <folder>...`: focal lengths and lens data of photos, from their headers.

use std::io::Read;
use std::path::{Path, PathBuf};

use image::imageops::{self, FilterType};
use image::{ImageBuffer, Rgb, RgbImage, RgbaImage};
use windedit_core::decode::{self, Orientation};
use windedit_core::gpu::Gpu;
use windedit_core::pipeline::{Edit, Image, ImageMemory, Target, Zoom, read_target, render_image, warm_up};

fn width() -> u32 {
    std::env::var("LENS_WIDTH").ok().and_then(|w| w.parse().ok()).unwrap_or(1024)
}

struct Gray {
    w: usize,
    h: usize,
    v: Vec<f32>,
}

impl Gray {
    fn at(&self, x: f32, y: f32) -> f32 {
        let (x, y) = (x.clamp(0.0, self.w as f32 - 1.001), y.clamp(0.0, self.h as f32 - 1.001));
        let (ix, iy) = (x as usize, y as usize);
        let (fx, fy) = (x - ix as f32, y - iy as f32);
        let p = |dx: usize, dy: usize| self.v[(iy + dy) * self.w + ix + dx];
        (p(0, 0) * (1.0 - fx) + p(1, 0) * fx) * (1.0 - fy) + (p(0, 1) * (1.0 - fx) + p(1, 1) * fx) * fy
    }
}

fn luma(img: &RgbaImage) -> Gray {
    let v = img.pixels().map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32).collect();
    Gray { w: img.width() as usize, h: img.height() as usize, v }
}

/// Edge strength: Sobel magnitude of a lightly blurred copy.
fn edges(g: &Gray) -> Gray {
    let blur = |src: &Gray| {
        let mut out = vec![0.0; src.v.len()];
        for y in 0..src.h {
            for x in 0..src.w {
                let mut sum = 0.0;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let (xx, yy) = ((x as i32 + dx).clamp(0, src.w as i32 - 1), (y as i32 + dy).clamp(0, src.h as i32 - 1));
                        sum += src.v[yy as usize * src.w + xx as usize];
                    }
                }
                out[y * src.w + x] = sum / 9.0;
            }
        }
        Gray { w: src.w, h: src.h, v: out }
    };
    let b = blur(&blur(g));
    let mut out = vec![0.0; b.v.len()];
    for y in 1..b.h - 1 {
        for x in 1..b.w - 1 {
            let p = |dx: usize, dy: usize| b.v[(y + dy - 1) * b.w + x + dx - 1];
            let gx = p(2, 0) + 2.0 * p(2, 1) + p(2, 2) - p(0, 0) - 2.0 * p(0, 1) - p(0, 2);
            let gy = p(0, 2) + 2.0 * p(1, 2) + p(2, 2) - p(0, 0) - 2.0 * p(1, 0) - p(2, 0);
            out[y * b.w + x] = gx.hypot(gy);
        }
    }
    Gray { w: b.w, h: b.h, v: out }
}

/// Maps a reference pixel to the render: render = (ref − c − t) / s + c.
#[derive(Clone, Copy)]
struct Fit {
    s: f32,
    tx: f32,
    ty: f32,
}

impl Fit {
    fn map(&self, w: usize, h: usize, x: f32, y: f32) -> (f32, f32) {
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        ((x - cx - self.tx) / self.s + cx, (y - cy - self.ty) / self.s + cy)
    }
}

/// Normalized correlation between the reference's edges and the render's, over a window of
/// reference pixels, with the render sampled through `fit` and then shifted by `(dx, dy)`.
fn correlation(r: &Gray, rf: &Gray, fit: &Fit, win: (usize, usize, usize, usize), shift: (f32, f32), stride: usize) -> f32 {
    let (x0, y0, x1, y1) = win;
    let (mut sa, mut sb, mut saa, mut sbb, mut sab, mut n) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for y in (y0..y1).step_by(stride) {
        for x in (x0..x1).step_by(stride) {
            let (u, v) = fit.map(r.w, r.h, x as f32, y as f32);
            let (u, v) = (u - shift.0, v - shift.1);
            if u < 0.0 || v < 0.0 || u > r.w as f32 - 1.0 || v > r.h as f32 - 1.0 {
                continue;
            }
            let (a, b) = (rf.v[y * rf.w + x] as f64, r.at(u, v) as f64);
            sa += a;
            sb += b;
            saa += a * a;
            sbb += b * b;
            sab += a * b;
            n += 1.0;
        }
    }
    if n < 100.0 {
        return -1.0;
    }
    let (va, vb) = (saa - sa * sa / n, sbb - sb * sb / n);
    if va <= 0.0 || vb <= 0.0 { -1.0 } else { ((sab - sa * sb / n) / (va * vb).sqrt()) as f32 }
}

/// The scale and shift that best align the render with the reference.
fn global_fit(render: &Gray, reference: &Gray) -> (Fit, f32) {
    let win = (width() as usize / 20, reference.h / 20, reference.w - width() as usize / 20, reference.h - reference.h / 20);
    let score = |f: &Fit| correlation(render, reference, f, win, (0.0, 0.0), 3);
    let mut best = Fit { s: 1.0, tx: 0.0, ty: 0.0 };
    let mut best_score = score(&best);
    let mut step = [0.02f32, 6.0, 6.0];
    for _ in 0..40 {
        let mut improved = false;
        for p in 0..3 {
            for dir in [-1.0f32, 1.0] {
                let mut f = best;
                match p {
                    0 => f.s += dir * step[0],
                    1 => f.tx += dir * step[1],
                    _ => f.ty += dir * step[2],
                }
                let sc = score(&f);
                if sc > best_score {
                    (best, best_score, improved) = (f, sc, true);
                }
            }
        }
        if !improved {
            if step[1] < 0.03 {
                break;
            }
            step.iter_mut().for_each(|s| *s *= 0.5);
        }
    }
    (best, best_score)
}

/// How far the render's content sits from the reference's inside a window, after the global
/// fit: a search over shifts of up to ±RANGE pixels, refined to a fraction of a pixel.
fn local_shift(render: &Gray, reference: &Gray, fit: &Fit, centre: (usize, usize)) -> Option<(f32, f32, f32)> {
    const RANGE: i32 = 24;
    const HALF: usize = 96;
    let win = (
        centre.0.saturating_sub(HALF).max(2),
        centre.1.saturating_sub(HALF).max(2),
        (centre.0 + HALF).min(reference.w - 2),
        (centre.1 + HALF).min(reference.h - 2),
    );
    let mut scores = vec![-1.0f32; ((2 * RANGE + 1) * (2 * RANGE + 1)) as usize];
    let idx = |dx: i32, dy: i32| ((dy + RANGE) * (2 * RANGE + 1) + dx + RANGE) as usize;
    let (mut best, mut best_at) = (-1.0f32, (0, 0));
    for dy in -RANGE..=RANGE {
        for dx in -RANGE..=RANGE {
            let s = correlation(render, reference, fit, win, (dx as f32, dy as f32), 2);
            scores[idx(dx, dy)] = s;
            if s > best {
                (best, best_at) = (s, (dx, dy));
            }
        }
    }
    if best < 0.2 || best_at.0.abs() == RANGE || best_at.1.abs() == RANGE {
        return None;
    }
    // A parabola through the neighbours gives the fraction.
    let sub = |m: f32, c: f32, p: f32| {
        let d = m - 2.0 * c + p;
        if d.abs() < 1e-9 { 0.0 } else { 0.5 * (m - p) / d }
    };
    let (bx, by) = best_at;
    let fx = sub(scores[idx(bx - 1, by)], best, scores[idx(bx + 1, by)]);
    let fy = sub(scores[idx(bx, by - 1)], best, scores[idx(bx, by + 1)]);
    Some((bx as f32 + fx, by as f32 + fy, best))
}

fn render(gpu: &mut Gpu, frame: &decode::RawFrame, size: (u32, u32)) -> RgbaImage {
    let mut memory = ImageMemory::new();
    let (image, _) = Image::open(gpu, &mut memory, frame).unwrap();
    let target = Target::new(gpu, size.0, size.1).unwrap();
    gpu.begin().unwrap();
    render_image(gpu, &image, &Edit::default(), Zoom::Fit, &target);
    gpu.submit().unwrap();
    let pixels = read_target(gpu, &target).unwrap();
    RgbaImage::from_raw(size.0, size.1, pixels).unwrap()
}

/// The camera's JPEG at the render's size, turned the way the raw is shown.
fn reference(bytes: &[u8], orientation: Orientation, size: (u32, u32)) -> Option<RgbaImage> {
    let jpeg = decode::largest_embedded_jpeg(bytes)?;
    let end = decode::jpeg_end(&bytes[jpeg.offset..], 0)?;
    let px = windedit_core::jpeg::decode(&bytes[jpeg.offset..jpeg.offset + end], 2048).ok()?;
    let mut rgba = px.bgra.clone();
    rgba.chunks_mut(4).for_each(|p| {
        p.swap(0, 2);
        p[3] = 255;
    });
    let img = RgbaImage::from_raw(px.width, px.height, rgba)?;
    let img = match orientation {
        Orientation::Normal => img,
        Orientation::Rotate90 => imageops::rotate90(&img),
        Orientation::Rotate180 => imageops::rotate180(&img),
        Orientation::Rotate270 => imageops::rotate270(&img),
    };
    let aspect = (img.width() as f32 / img.height() as f32) / (size.0 as f32 / size.1 as f32);
    if (aspect - 1.0).abs() > 0.01 {
        println!("  note: the JPEG's aspect differs from the raw's by {:.1}%", (aspect - 1.0) * 100.0);
    }
    Some(imageops::resize(&img, size.0, size.1, FilterType::Triangle))
}

fn report(name: &str, label: &str, render_img: &RgbaImage, reference_img: &RgbaImage, out_dir: &Path) {
    let (r, rf) = (luma(render_img), luma(reference_img));
    let (re, rfe) = (edges(&r), edges(&rf));
    let (fit, score) = global_fit(&re, &rfe);
    println!("  {label}: scale {:.4}, shift ({:+.1}, {:+.1}) px, edge correlation {:.3}", fit.s, fit.tx, fit.ty, score);
    let (w, h) = (rf.w, rf.h);
    let spots = [
        ("centre", (w / 2, h / 2)),
        ("top-left", (w / 8, h / 8)),
        ("top-right", (w - w / 8, h / 8)),
        ("bottom-left", (w / 8, h - h / 8)),
        ("bottom-right", (w - w / 8, h - h / 8)),
        ("mid-left", (w / 8, h / 2)),
        ("mid-right", (w - w / 8, h / 2)),
        ("mid-top", (w / 2, h / 8)),
        ("mid-bottom", (w / 2, h - h / 8)),
    ];
    let mut radial_corners = Vec::new();
    for (spot, c) in spots {
        match local_shift(&re, &rfe, &fit, c) {
            Some((dx, dy, s)) => {
                // Outward from the centre: positive means the render's detail is further out.
                let (ox, oy) = (c.0 as f32 - w as f32 / 2.0, c.1 as f32 - h as f32 / 2.0);
                let len = ox.hypot(oy).max(1.0);
                let radial = -(dx * ox + dy * oy) / len;
                println!(
                    "    {spot:<13} off ({:+5.1}, {:+5.1}) = {:4.1} px, radial {:+5.1}   (corr {:.2})",
                    -dx,
                    -dy,
                    dx.hypot(dy),
                    radial,
                    s
                );
                if spot.contains('-') && !spot.starts_with("mid") {
                    radial_corners.push(dx.hypot(dy));
                }
            }
            None => println!("    {spot:<13} no reliable match (too little detail, or off by over 24 px)"),
        }
    }
    if !radial_corners.is_empty() {
        println!("    worst corner {:.1} px", radial_corners.iter().cloned().fold(0.0, f32::max));
    }

    // Side by side: the render, the camera's JPEG. Then the edges of both on top of each other:
    // red is the render's, cyan the camera's, so a misaligned edge shows as a colored pair.
    let mut sheet: RgbImage = ImageBuffer::new(w as u32 * 2, h as u32);
    for y in 0..h {
        for x in 0..w {
            let a = render_img.get_pixel(x as u32, y as u32);
            let b = reference_img.get_pixel(x as u32, y as u32);
            sheet.put_pixel(x as u32, y as u32, Rgb([a[0], a[1], a[2]]));
            sheet.put_pixel((x + w) as u32, y as u32, Rgb([b[0], b[1], b[2]]));
        }
    }
    let side = out_dir.join(format!("{name}.{label}.png"));
    sheet.save(&side).unwrap();
    let mut diff: RgbImage = ImageBuffer::new(w as u32, h as u32);
    let scale = |v: f32| (v / 60.0).clamp(0.0, 1.0) * 255.0;
    for y in 0..h {
        for x in 0..w {
            let (u, v) = fit.map(w, h, x as f32, y as f32);
            let a = re.at(u, v);
            let b = rfe.v[y * w + x];
            diff.put_pixel(x as u32, y as u32, Rgb([scale(a) as u8, scale(b) as u8, scale(b) as u8]));
        }
    }
    let diff_path = out_dir.join(format!("{name}.{label}.diff.png"));
    diff.save(&diff_path).unwrap();
    println!("    → {} and {}", side.display(), diff_path.file_name().unwrap().to_string_lossy());
}

pub fn bench_lens(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    warm_up(&mut gpu).unwrap();
    let out_dir = std::env::var("LENS_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_exe().unwrap().parent().unwrap().join("renders").join("lens"));
    std::fs::create_dir_all(&out_dir).unwrap();
    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(f).unwrap();
        let mut frame = match decode::decode_file(f) {
            Ok(frame) => frame,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        let (ow, oh) = if frame.orientation.swaps_axes() { (frame.height, frame.width) } else { (frame.width, frame.height) };
        println!(
            "{name} ({} {}, {}x{}, {} mm)",
            frame.make,
            frame.model,
            frame.width,
            frame.height,
            decode_focal(&bytes)
        );
        let size = (width(), (width() as f32 * oh as f32 / ow as f32).round() as u32);
        let Some(reference_img) = reference(&bytes, frame.orientation, size) else {
            println!("  no embedded JPEG");
            continue;
        };
        match &frame.lens {
            Some(l) => println!(
                "  lens data: {:?} scale {:.5}, k {:.6?}, zoom {:.4}{}",
                l.model,
                l.scale,
                l.k,
                l.zoom,
                if l.has_ca() { format!(", CA red {:.6?} blue {:.6?}", l.ca_red, l.ca_blue) } else { String::new() }
            ),
            None => println!("  no lens data in this file"),
        }
        let corrected = frame.lens.is_some().then(|| render(&mut gpu, &frame, size));
        if let (Some(c), Some(l)) = (&corrected, frame.lens)
            && l.has_ca()
        {
            ca_report("with CA correction", c);
            frame.lens = Some(windedit_core::lens::LensCorrection { ca_red: [0.0; 3], ca_blue: [0.0; 3], ..l });
            ca_report("distortion only   ", &render(&mut gpu, &frame, size));
        }
        frame.lens = None;
        let plain = render(&mut gpu, &frame, size);
        if let Some(c) = &corrected {
            report(&name, "corrected", c, &reference_img, &out_dir);
        }
        report(&name, "uncorrected", &plain, &reference_img, &out_dir);
    }
}

fn decode_focal(bytes: &[u8]) -> String {
    windedit_core::lens::focal_length(bytes).map_or("?".into(), |f| format!("{f:.0}"))
}

/// Focal length and model of photos from their first bytes only, for picking test files:
/// prints one line per file, `<focal mm> <model> <path>`. Folders are searched (not recursively).
pub fn lens_scan(args: &[String]) {
    let mut files = Vec::new();
    for a in args {
        let p = PathBuf::from(a);
        if p.is_dir() {
            let mut sub: Vec<_> = std::fs::read_dir(&p).unwrap().flatten().map(|e| e.path()).collect();
            sub.sort();
            files.extend(sub);
        } else {
            files.push(p);
        }
    }
    for f in files {
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if ext != "orf" && ext != "rw2" {
            continue;
        }
        let mut head = vec![0u8; 24 * 1024];
        let Ok(n) = std::fs::File::open(&f).and_then(|mut h| h.read(&mut head)) else { continue };
        head.truncate(n);
        let model = windedit_core::lens::camera_model(&head).unwrap_or_default();
        if let Some(focal) = windedit_core::lens::focal_length(&head) {
            println!("{focal:5.1} {model} {}", f.display());
        }
    }
}

fn channel(img: &RgbaImage, c: usize) -> Gray {
    Gray { w: img.width() as usize, h: img.height() as usize, v: img.pixels().map(|p| p[c] as f32).collect() }
}

/// How far red and blue sit from green, radially, at the corners and edge middles: positive
/// means further out. A good chromatic aberration correction brings these to about zero.
fn ca_report(label: &str, img: &RgbaImage) {
    let (g, r, b) = (edges(&channel(img, 1)), edges(&channel(img, 0)), edges(&channel(img, 2)));
    let (w, h) = (g.w, g.h);
    let id = Fit { s: 1.0, tx: 0.0, ty: 0.0 };
    let spots = [(w / 8, h / 8), (w - w / 8, h / 8), (w / 8, h - h / 8), (w - w / 8, h - h / 8), (w / 8, h / 2), (w - w / 8, h / 2)];
    let radial = |other: &Gray| -> Vec<String> {
        spots
            .iter()
            .map(|&c| match local_shift(other, &g, &id, c) {
                Some((dx, dy, _)) => {
                    let (ox, oy) = (c.0 as f32 - w as f32 / 2.0, c.1 as f32 - h as f32 / 2.0);
                    format!("{:+.2}", -(dx * ox + dy * oy) / ox.hypot(oy))
                }
                None => "  ?  ".into(),
            })
            .collect()
    };
    println!("  CA {label}: red−green radial px at TL TR BL BR ML MR: {}", radial(&r).join(" "));
    println!("  CA {label}: blue−green                               : {}", radial(&b).join(" "));
}

/// `bench lenstags <raw>`: the tags of a file, or all values of one (TAG=0x1103; SUB=2020 picks the
/// Olympus CameraSettings directory).
pub fn lens_tags(files: &[PathBuf]) {
    for f in files {
        let bytes = std::fs::read(f).unwrap();
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let tags = windedit_core::lens::dump_tags(&bytes, ext == "orf");
        let want = std::env::var("TAG").ok().and_then(|t| u16::from_str_radix(t.trim_start_matches("0x"), 16).ok());
        for (tag, kind, count, v) in tags {
            match want {
                Some(w) if w == tag => println!("{tag:04x} kind {kind} count {count}\n{v:?}"),
                Some(_) => {}
                None => println!("{tag:04x} kind {kind} count {count} {:?}", &v[..v.len().min(8)]),
            }
        }
    }
}

fn srgb_to_linear(v: f32) -> f32 {
    let c = v / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// `bench vignette <raw>...`: what the vignetting match (vignette.rs, a diagnostic only: the engine
/// does not use it) would find for each photo,
/// and the profile of how much darker the render is than the camera's
/// JPEG at the centre, edges and corners (in stops, negative is darker).
pub fn bench_vignette(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    warm_up(&mut gpu).unwrap();
    let out_dir = std::env::var("LENS_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_exe().unwrap().parent().unwrap().join("renders").join("lens"));
    std::fs::create_dir_all(&out_dir).unwrap();
    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(f).unwrap();
        let frame = decode::decode_file(f).unwrap();
        let (ow, oh) = if frame.orientation.swaps_axes() { (frame.height, frame.width) } else { (frame.width, frame.height) };
        let size = (width(), (width() as f32 * oh as f32 / ow as f32).round() as u32);
        let Some(reference_img) = reference(&bytes, frame.orientation, size) else { continue };
        println!("{name} ({} {}, {} mm), by radius 0.0..1.0, in stops:", frame.make, frame.model, decode_focal(&bytes));
        let started = std::time::Instant::now();
        let analysis = crate::vignette::analyse(&frame, &bytes);
        let took = started.elapsed();
        let rings: Vec<String> = analysis.rings.iter().map(|r| format!("{:+.2}", r.0)).collect();
        println!("  camera JPEG brighter than the raw by ({:.0} ms): {}", took.as_secs_f64() * 1000.0, rings.join(" "));
        match analysis.gain {
            Some(g) => println!("  would apply gain 1 + {:.3}·r² + {:.3}·r⁴ ({:+.2} stops at the corner, scatter {:.3})", g[0], g[1], analysis.outer_stops, analysis.scatter),
            None => println!("  would not correct: {} (fit {:+.2} stops at the corner, scatter {:.3})", analysis.why_not.unwrap_or("?"), analysis.outer_stops, analysis.scatter),
        }
        let img = render(&mut gpu, &frame, size);
        let profile = vignette_profile(&img, &reference_img);
        println!("  render minus camera JPEG: {}", profile.iter().map(|g| format!("{g:+.2}")).collect::<Vec<_>>().join(" "));
        let (w, h) = (img.width(), img.height());
        let mut sheet: RgbImage = ImageBuffer::new(w * 2, h);
        for y in 0..h {
            for x in 0..w {
                let (a, b) = (img.get_pixel(x, y), reference_img.get_pixel(x, y));
                sheet.put_pixel(x, y, Rgb([a[0], a[1], a[2]]));
                sheet.put_pixel(x + w, y, Rgb([b[0], b[1], b[2]]));
            }
        }
        sheet.save(out_dir.join(format!("{name}.vignette.png"))).unwrap();
    }
}

/// The same comparison as a radial profile: the render's median brightness against the
/// camera JPEG's (stops, negative is darker) in rings of radius, in half-diagonals. All angles
/// together, so local differences in tone rendering average out; the tone curves are matched
/// inside r < 0.4.
fn vignette_profile(render_img: &RgbaImage, reference_img: &RgbaImage) -> Vec<f32> {
    let (w, h) = (render_img.width() as usize, render_img.height() as usize);
    let (lr, lj) = (luma(render_img), luma(reference_img));
    let half_diag = 0.5 * (w as f32).hypot(h as f32);
    let radius = |x: usize, y: usize| (x as f32 - w as f32 / 2.0).hypot(y as f32 - h as f32 / 2.0) / half_diag;
    const BINS: usize = 40;
    let bin_of = |l: f32| (((l.log2() + 10.0) / 10.0 * BINS as f32) as isize).clamp(0, BINS as isize - 1) as usize;
    let mut tone: Vec<Vec<f32>> = vec![Vec::new(); BINS];
    for y in 0..h {
        for x in 0..w {
            if radius(x, y) < 0.4 {
                let (a, b) = (srgb_to_linear(lr.v[y * w + x]), srgb_to_linear(lj.v[y * w + x]));
                if (0.004..0.97).contains(&a) && (0.004..0.97).contains(&b) {
                    tone[bin_of(b)].push(a.log2());
                }
            }
        }
    }
    let curve: Vec<Option<f32>> = tone
        .iter_mut()
        .map(|v| {
            (v.len() > 40).then(|| {
                v.sort_by(|a, b| a.total_cmp(b));
                v[v.len() / 2]
            })
        })
        .collect();
    let mapped = |l: f32| -> Option<f32> {
        let b = bin_of(l);
        (0..BINS).filter_map(|d| curve.get(b.wrapping_sub(d)).copied().flatten().or_else(|| curve.get(b + d).copied().flatten())).next()
    };
    let mut rings: Vec<Vec<f32>> = vec![Vec::new(); 10];
    for y in 0..h {
        for x in 0..w {
            let (a, b) = (srgb_to_linear(lr.v[y * w + x]), srgb_to_linear(lj.v[y * w + x]));
            let ring = (radius(x, y) * 10.0) as usize;
            if ring < 10
                && (0.004..0.97).contains(&a)
                && (0.004..0.97).contains(&b)
                && let Some(m) = mapped(b)
            {
                rings[ring].push(a.log2() - m);
            }
        }
    }
    rings
        .iter_mut()
        .map(|v| {
            if v.len() < 500 {
                return f32::NAN;
            }
            v.sort_by(|a, b| a.total_cmp(b));
            v[v.len() / 2]
        })
        .collect()
}
