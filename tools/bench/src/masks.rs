//! `bench masks [raw...]`: the masks (docs/phase2d-masks.md).
//!
//! 1. With no masks the develop pass must give exactly what it did before masks existed: prints a
//!    hash of a few busy renders (compare across builds) and its 4K time.
//! 2. The GPU against the CPU reference (`tone::render_masked`) on a synthetic photo, per pixel, for
//!    each slider a mask carries; and a mask covering everything against the same global slider.
//! 3. The develop pass at 4K with 0 and with 8 radial masks of two shapes each.
//! 4. Renders to `renders/masks` beside the executable: the red overlay, and before | after.

use std::path::PathBuf;

use windedit_core::color;
use windedit_core::decode::{self, Orientation, RawFrame};
use windedit_core::export;
use windedit_core::develop::{self, temperature_to_multipliers};
use windedit_core::gpu::Gpu;
use windedit_core::masks::{self, ADD, Component, GpuMaskSet, KIND_LINEAR, KIND_RADIAL, Local, Mask, Masks, SUBTRACT, slider};
use windedit_core::pipeline::{Edit, Image, ImageMemory, Layer, MaskInput, Target, Zoom, read_target, render_image, render_image_masked, render_masked, warm_up};
use windedit_core::tone::{self, MODEL};

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// A busy edit that touches every per-pixel term the masks share.
pub fn busy() -> Edit {
    let mut e = Edit { exposure: 0.3, contrast: 20.0, highlights: -40.0, shadows: 50.0, whites: 15.0, blacks: -10.0, saturation: 12.0, vibrance: 20.0, clarity: 35.0, ..Default::default() };
    e.mixer[3] = 40.0;
    e.mixer[9] = -30.0;
    e
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Median develop times at 4K (Fit) for several cases, interleaved so clocks and caches treat them
/// alike, with Exposure changing each frame so nothing is cached.
pub fn time_4k(gpu: &mut Gpu, image: &Image, cases: &[(&str, Edit, Masks)], rounds: usize) -> Vec<f64> {
    let target = Target::new(gpu, 3840, 2160).unwrap();
    let mut times = vec![Vec::new(); cases.len()];
    for round in 0..rounds {
        for (k, (_, edit, masks)) in cases.iter().enumerate() {
            let edit = Edit { exposure: edit.exposure + (round as f32 / rounds as f32) * 0.5, ..*edit };
            gpu.begin().unwrap();
            let a = gpu.timestamp();
            render_image_masked(gpu, image, &edit, &MaskInput { masks, overlay: None }, Zoom::Fit, &target);
            let b = gpu.timestamp();
            let ts = gpu.submit().unwrap();
            times[k].push(ts[b as usize] - ts[a as usize]);
        }
    }
    times.into_iter().map(median).collect()
}

// ---- Shapes to test with ----

fn linear(a: [f32; 2], b: [f32; 2], mode: u32) -> Component {
    Component { kind: KIND_LINEAR, mode, p: [a[0], a[1], b[0], b[1], 0.0, 0.0, 0.0, 0.0] }
}

fn radial(c: [f32; 2], r: [f32; 2], angle: f32, feather: f32, mode: u32) -> Component {
    Component { kind: KIND_RADIAL, mode, p: [c[0], c[1], r[0], r[1], angle, feather, 0.0, 0.0] }
}

fn mask_of(components: &[Component], sliders: &[(usize, f32)], amount: f32) -> Mask {
    let mut m = Mask::new();
    m.amount = amount;
    for (i, c) in components.iter().enumerate() {
        m.components[i] = *c;
    }
    m.count = components.len() as u32;
    for &(i, v) in sliders {
        m.sliders[i] = v;
    }
    m
}

fn masks_of(list: &[Mask]) -> Masks {
    let mut m = Masks::default();
    m.masks[..list.len()].copy_from_slice(list);
    m.count = list.len() as u32;
    m
}

/// A mask that weighs 1 everywhere.
fn everywhere(sliders: &[(usize, f32)]) -> Masks {
    masks_of(&[mask_of(&[linear([5.0, 5.0], [6.0, 5.0], ADD)], sliders, 100.0)])
}

// ---- The synthetic photo ----

const W: usize = 768;
const H: usize = 512;

/// Smooth colours and tones in linear sRGB (the camera's own space here), so demosaicing it loses nothing worth
/// counting: a diagonal gradient, a bright sky at the top, a dark foreground, a few soft blobs.
fn scene_at(x: usize, y: usize) -> [f32; 3] {
    let (u, v) = (x as f32 / W as f32, y as f32 / H as f32);
    let sky = (1.0 - v * 2.2).clamp(0.0, 1.0);
    let blob = |cx: f32, cy: f32, r: f32| (-(((u - cx) * 1.5).powi(2) + (v - cy).powi(2)) / (r * r)).exp();
    let l = 0.02 + 0.5 * sky * sky + 0.25 * blob(0.3, 0.6, 0.12) + 0.4 * blob(0.72, 0.5, 0.08) + 0.04 * u;
    [l * (0.8 + 0.4 * u), l * (0.9 + 0.1 * v), l * (1.3 - 0.5 * u)]
}

/// The synthetic photo as a raw: RGGB, 16 bits, camera = linear sRGB, white balance as shot `wb`.
fn synthetic_frame(wb: [f32; 3]) -> RawFrame {
    let white = 60000.0;
    let mut data = vec![0u16; W * H];
    for y in 0..H {
        for x in 0..W {
            let ch = [[0, 1], [1, 2]][y % 2][x % 2];
            data[y * W + x] = (scene_at(x, y)[ch] / wb[ch] * white).clamp(0.0, white).round() as u16;
        }
    }
    RawFrame {
        make: String::new(),
        model: "synthetic".into(),
        data,
        width: W as u32,
        height: H as u32,
        cfa: [[0, 1], [1, 2]],
        black: [[0.0; 2]; 2],
        white,
        wb,
        cam_to_working: color::invert(&color::REC2020_TO_SRGB),
        xyz_to_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        orientation: Orientation::Normal,
        lens: None,
        decode_time: Default::default(),
    }
}

fn render(gpu: &mut Gpu, image: &Image, edit: &Edit, masks: &Masks, overlay: Option<usize>, zoom: Zoom, target: &Target) -> Vec<u8> {
    gpu.begin().unwrap();
    render_image_masked(gpu, image, edit, &MaskInput { masks, overlay }, zoom, target);
    gpu.submit().unwrap();
    read_target(gpu, target).unwrap()
}

fn srgb_encode(v: f32) -> f32 {
    if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// Difference statistics between two RGBA8 renders, in levels: mean, 99th percentile, largest.
fn compare(a: &[u8], b: &[u8]) -> (f32, f32, f32) {
    let mut d: Vec<f32> = a.chunks(4).zip(b.chunks(4)).flat_map(|(p, q)| (0..3).map(move |c| (p[c] as f32 - q[c] as f32).abs())).collect();
    let mean = d.iter().sum::<f32>() / d.len() as f32;
    d.sort_by(f32::total_cmp);
    (mean, d[d.len() * 99 / 100], d[d.len() - 1])
}

/// The GPU against the CPU reference for one setting. Returns (mean, p99, max) in levels.
fn against_cpu(gpu: &mut Gpu, image: &Image, scene: &[[f32; 3]], edit: &Edit, mask_set: &Masks, target: &Target) -> (f32, f32, f32) {
    let set = GpuMaskSet::new(mask_set, &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    let locals: Vec<Local> = (0..W * H).map(|k| set.local(masks::to_frame([((k % W) as f32 + 0.5) / W as f32, ((k / W) as f32 + 0.5) / H as f32], (W as u32, H as u32)))).collect();
    let cpu = tone::render_masked(&MODEL, scene, W, H, edit, tone::camera_offset(""), (!mask_set.is_empty()).then_some(&locals[..]));
    let cpu8: Vec<u8> = cpu.iter().flat_map(|p| [p[0], p[1], p[2], 1.0].map(|v| (srgb_encode(v) * 255.0).round() as u8)).collect();
    let gpu8 = render(gpu, image, edit, mask_set, None, Zoom::Fit, target);
    compare(&cpu8, &gpu8)
}

fn save(path: &std::path::Path, px: &[u8], w: usize, h: usize) {
    image::save_buffer(path, px, w as u32, h as u32, image::ExtendedColorType::Rgba8).unwrap();
}

fn side_by_side(a: &[u8], b: &[u8], w: usize, h: usize) -> (usize, Vec<u8>) {
    let total = 2 * w + 6;
    let mut out = vec![255u8; total * h * 4];
    for y in 0..h {
        out[y * total * 4..(y * total + w) * 4].copy_from_slice(&a[y * w * 4..(y + 1) * w * 4]);
        out[(y * total + w + 6) * 4..(y * total + 2 * w + 6) * 4].copy_from_slice(&b[y * w * 4..(y + 1) * w * 4]);
    }
    (total, out)
}

pub fn bench_masks(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {}", gpu.adapter_name);
    warm_up(&mut gpu).unwrap();
    let out_dir = std::env::current_exe().unwrap().parent().unwrap().join("renders").join("masks");
    std::fs::create_dir_all(&out_dir).unwrap();

    // ---- 1. No masks: the pass is what it was ----
    println!("\n1. No masks: output hashes (same on every build that hasn't changed the develop pass) and 4K time");
    for f in files.iter().take(2) {
        let frame = decode::decode_file(f).unwrap();
        let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap();
        let target = Target::new(&gpu, 1600, 1066).unwrap();
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        for (label, edit) in [("default", Edit::default()), ("busy", busy()), ("clarity0", Edit { clarity: 0.0, ..busy() })] {
            for (view, zoom) in [("fit", Zoom::Fit), ("100%", Zoom::Actual { x: 0.4, y: 0.6 })] {
                gpu.begin().unwrap();
                render_image(&gpu, &image, &edit, zoom, &target);
                gpu.submit().unwrap();
                let px = read_target(&mut gpu, &target).unwrap();
                println!("  {name} {label} {view}: {:016x}", fnv(&px));
            }
        }
        for (label, edit) in [("default", Edit::default()), ("busy", busy())] {
            let ms = time_4k(&mut gpu, &image, &[(label, edit, Masks::default())], 60)[0];
            println!("  {name} {label} 4K fit develop: {ms:.3} ms");
        }
    }

    // ---- 2. The GPU against the CPU ----
    println!("\n2. GPU against the CPU reference on a synthetic {W}x{H} photo (levels: mean, 99th percentile, largest)");
    let as_shot = temperature_to_multipliers(&[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], 5500.0, 0.0);
    let frame = synthetic_frame(as_shot);
    let scene: Vec<[f32; 3]> = (0..W * H).map(|k| scene_at(k % W, k / W)).collect();
    let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap();
    let target = Target::new(&gpu, W as u32, H as u32).unwrap();
    // The CPU reference has no sharpening or noise reduction.
    let mut plain = Edit::default();
    plain.set_detail(develop::Detail { amount: 0.0, color: 0.0, ..develop::DETAIL_DEFAULTS });
    use slider::*;
    let top = |s: &[(usize, f32)]| s.to_vec();
    let case = |name: &'static str, edit: Edit, m: Masks| (name, edit, m);
    let cases = vec![
        case("no masks", plain, Masks::default()),
        case("exposure", plain, masks_of(&[mask_of(&[linear([0.5, 0.05], [0.5, 0.45], ADD)], &top(&[(EXPOSURE, -1.5)]), 100.0), mask_of(&[radial([0.5, 0.35], [0.3, 0.18], 0.5, 70.0, ADD), radial([0.62, 0.35], [0.08, 0.08], 0.0, 30.0, SUBTRACT)], &[(EXPOSURE, 1.0)], 80.0)])),
        case("contrast", plain, masks_of(&[mask_of(&[linear([0.5, 0.05], [0.5, 0.45], ADD)], &[(CONTRAST, 80.0)], 100.0), mask_of(&[radial([0.5, 0.35], [0.3, 0.18], 0.5, 70.0, ADD)], &[(CONTRAST, -60.0)], 80.0)])),
        case("highlights+shadows", plain, masks_of(&[mask_of(&[linear([0.5, 0.05], [0.5, 0.45], ADD)], &[(SHADOWS, 100.0), (HIGHLIGHTS, -70.0)], 100.0), mask_of(&[radial([0.4, 0.5], [0.3, 0.2], 0.0, 100.0, ADD)], &[(SHADOWS, -80.0), (HIGHLIGHTS, 60.0)], 100.0)])),
        case("whites+blacks", plain, masks_of(&[mask_of(&[linear([0.5, 0.05], [0.5, 0.45], ADD)], &[(WHITES, -80.0), (BLACKS, 50.0)], 100.0), mask_of(&[radial([0.7, 0.5], [0.2, 0.2], 0.0, 60.0, ADD)], &[(WHITES, 90.0), (BLACKS, -40.0)], 100.0)])),
        case("clarity", plain, masks_of(&[mask_of(&[linear([0.5, 0.05], [0.5, 0.95], ADD)], &[(CLARITY, 100.0)], 100.0)])),
        case("clarity + global", Edit { clarity: 40.0, shadows: 30.0, ..plain }, masks_of(&[mask_of(&[linear([0.5, 0.05], [0.5, 0.95], ADD)], &[(CLARITY, 90.0), (SHADOWS, 80.0)], 100.0)])),
        case("temperature+tint", plain, masks_of(&[mask_of(&[radial([0.5, 0.4], [0.4, 0.3], 0.0, 100.0, ADD)], &[(TEMPERATURE, 80.0), (TINT, -40.0)], 100.0)])),
        case("everything, over globals", Edit { exposure: 0.4, contrast: 25.0, highlights: -30.0, shadows: 40.0, whites: 20.0, blacks: -15.0, clarity: 30.0, ..plain }, masks_of(&[
            mask_of(&[linear([0.5, 0.05], [0.5, 0.6], ADD)], &[(EXPOSURE, -0.8), (CONTRAST, 40.0), (SHADOWS, 50.0), (TEMPERATURE, -50.0), (CLARITY, 60.0)], 90.0),
            mask_of(&[radial([0.3, 0.6], [0.2, 0.25], 1.0, 40.0, ADD), linear([0.9, 0.0], [0.9, 1.0], SUBTRACT)], &[(EXPOSURE, 0.7), (HIGHLIGHTS, -60.0), (WHITES, 30.0), (BLACKS, 20.0), (TINT, 30.0)], 100.0),
        ])),
    ];
    let mut worst = 0f32;
    for (name, edit, m) in &cases {
        let (mean, p99, max) = against_cpu(&mut gpu, &image, &scene, edit, m, &target);
        worst = worst.max(p99);
        println!("  {name:<26} mean {mean:5.2}  p99 {p99:5.1}  max {max:5.0}");
    }
    println!("  worst 99th percentile: {worst:.1} levels");

    // A mask that covers everything against the same global slider (saturation has no CPU reference).
    println!("\n   a mask covering everything against the same global slider (levels: mean, p99, max)");
    let equal = [
        ("saturation +50", Edit { saturation: 50.0, ..plain }, everywhere(&[(SATURATION, 50.0)])),
        ("saturation -70", Edit { saturation: -70.0, ..plain }, everywhere(&[(SATURATION, -70.0)])),
        ("exposure +1.2", Edit { exposure: 1.2, ..plain }, everywhere(&[(EXPOSURE, 1.2)])),
        ("contrast +60", Edit { contrast: 60.0, ..plain }, everywhere(&[(CONTRAST, 60.0)])),
        ("highlights -50, shadows +70", Edit { highlights: -50.0, shadows: 70.0, ..plain }, everywhere(&[(HIGHLIGHTS, -50.0), (SHADOWS, 70.0)])),
        ("whites +40, blacks -30", Edit { whites: 40.0, blacks: -30.0, ..plain }, everywhere(&[(WHITES, 40.0), (BLACKS, -30.0)])),
        ("clarity +70", Edit { clarity: 70.0, ..plain }, everywhere(&[(CLARITY, 70.0)])),
        ("temperature +100 (7500 K)", Edit { custom_white_balance: 1, temperature: 7500.0, tint: 0.0, ..plain }, everywhere(&[(TEMPERATURE, 100.0)])),
        ("tint +40 (global +20)", Edit { custom_white_balance: 1, temperature: 5500.0, tint: 20.0, ..plain }, everywhere(&[(TINT, 40.0)])),
    ];
    for (name, edit, m) in &equal {
        let global = render(&mut gpu, &image, edit, &Masks::default(), None, Zoom::Fit, &target);
        let masked = render(&mut gpu, &image, &plain, m, None, Zoom::Fit, &target);
        let (mean, p99, max) = compare(&global, &masked);
        println!("  {name:<26} mean {mean:5.2}  p99 {p99:5.1}  max {max:5.0}");
    }

    // ---- 3. Timing ----
    println!("\n3. Develop pass at 4K (Fit), median of 60");
    let eight: Vec<Mask> = (0..8)
        .map(|i| {
            let x = 0.1 + 0.11 * i as f32;
            mask_of(
                &[radial([x, 0.4], [0.25, 0.15], i as f32 * 0.4, 50.0, ADD), radial([x + 0.05, 0.4], [0.05, 0.05], 0.0, 30.0, SUBTRACT)],
                &[(EXPOSURE, 0.3), (CONTRAST, 20.0), (HIGHLIGHTS, -20.0), (SHADOWS, 30.0), (WHITES, 10.0), (BLACKS, -10.0), (SATURATION, 15.0), (TEMPERATURE, 10.0), (TINT, 5.0)],
                100.0,
            )
        })
        .collect();
    let with_clarity: Vec<Mask> = eight.iter().map(|m| { let mut m = *m; m.sliders[CLARITY] = 25.0; m }).collect();
    let timed: Vec<(&str, Edit, Masks)> = vec![
        ("0 masks", busy(), Masks::default()),
        ("1 radial mask", busy(), masks_of(&eight[..1])),
        ("8 radial masks (2 shapes each)", busy(), masks_of(&eight)),
        ("8 radial masks, with Clarity", busy(), masks_of(&with_clarity)),
    ];
    let subjects: Vec<(String, Image)> = if files.is_empty() {
        vec![("synthetic".into(), Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap().0)]
    } else {
        files.iter().take(1).map(|f| {
            let frame = decode::decode_file(f).unwrap();
            (f.file_stem().unwrap().to_string_lossy().to_string(), Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap().0)
        }).collect()
    };
    for (name, image) in &subjects {
        for _ in 0..2 {
            let t = time_4k(&mut gpu, image, &timed, 60);
            for (k, (label, _, _)) in timed.iter().enumerate() {
                println!("  {name} {label:<32} {:.3} ms{}", t[k], if k == 0 { String::new() } else { format!(" ({:+.3})", t[k] - t[0]) });
            }
        }
    }

    // ---- 4. Pictures ----
    for f in files.iter().take(2) {
        let frame = decode::decode_file(f).unwrap();
        let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap();
        let (w, h) = image.oriented_size();
        let (tw, th) = if w >= h { (1200, 1200 * h / w) } else { (1200 * w / h, 1200) };
        let (tw, th) = (tw as usize, th as usize);
        let target = Target::new(&gpu, tw as u32, th as u32).unwrap();
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let dims = (frame.width, frame.height);
        // A graduated filter over the top and a spotlight in the middle (frame coordinates).
        let sky = mask_of(&[linear(masks::to_frame([0.5, 0.05], dims), masks::to_frame([0.5, 0.5], dims), ADD)], &[(EXPOSURE, -1.2), (HIGHLIGHTS, -40.0), (TEMPERATURE, -30.0)], 100.0);
        let spot = mask_of(&[radial(masks::to_frame([0.5, 0.6], dims), [0.2, 0.14], 0.0, 60.0, ADD)], &[(EXPOSURE, 0.8), (SHADOWS, 40.0), (SATURATION, 30.0), (CLARITY, 40.0)], 100.0);
        let both = masks_of(&[sky, spot]);
        let base = render(&mut gpu, &image, &Edit::default(), &Masks::default(), None, Zoom::Fit, &target);
        let after = render(&mut gpu, &image, &Edit::default(), &both, None, Zoom::Fit, &target);
        let (pw, px) = side_by_side(&base, &after, tw, th);
        save(&out_dir.join(format!("{name}-effect.png")), &px, pw, th);
        let overlay0 = render(&mut gpu, &image, &Edit::default(), &both, Some(0), Zoom::Fit, &target);
        let overlay1 = render(&mut gpu, &image, &Edit::default(), &both, Some(1), Zoom::Fit, &target);
        let (pw, px) = side_by_side(&overlay0, &overlay1, tw, th);
        save(&out_dir.join(format!("{name}-overlays.png")), &px, pw, th);
        println!("\n{name}: wrote {name}-effect.png (before | after) and {name}-overlays.png (mask 1 | mask 2) in {}", out_dir.display());
        export_check(&mut gpu, &image, &Masks::default(), &format!("{name} (no masks)"));
        export_check(&mut gpu, &image, &both, &name);
    }
}

/// The export's tiles (2048 pixels, `TileRenderer::step_masked`) against the same window drawn in
/// one piece: masks are in frame coordinates, so the tiles must join without a seam.
fn export_check(gpu: &mut Gpu, image: &Image, masks: &Masks, name: &str) {
    // Without sharpening and noise reduction, whose tiles differ a little from a whole render with or without masks.
    let mut edit = Edit { clarity: 20.0, shadows: 25.0, ..Edit::default() };
    edit.set_detail(develop::Detail { amount: 0.0, color: 0.0, ..develop::DETAIL_DEFAULTS });
    let window = export::window(image.oriented_size(), &edit);
    let mut renderer = export::TileRenderer::new(gpu, window, &edit).unwrap();
    let mut tiled = Vec::new();
    while !renderer.done() {
        if let Some(strip) = renderer.step_masked(gpu, image, &edit, masks).unwrap() {
            tiled.extend_from_slice(&strip.bgr);
        }
    }
    let whole = Target::new(gpu, window.width, window.height).unwrap();
    gpu.begin().unwrap();
    let zoom = Zoom::Region { x: window.x as f32, y: window.y as f32, scale: 1.0 };
    render_masked(gpu, Layer::Raw(image), &edit, &MaskInput { masks, overlay: None }, zoom, image.oriented_size(), &whole);
    gpu.submit().unwrap();
    let px = read_target(gpu, &whole).unwrap();
    let (mut differ, mut worst) = (0usize, 0i32);
    for (i, t) in tiled.chunks(3).enumerate() {
        let p = &px[i * 4..i * 4 + 3];
        let d = (0..3).map(|c| (t[2 - c] as i32 - p[c] as i32).abs()).max().unwrap();
        differ += (d > 0) as usize;
        worst = worst.max(d);
    }
    println!("  {name} export: {}x{} in {} tiles against one piece: {differ} of {} pixels differ, by at most {worst} levels", window.width, window.height, renderer.tiles(), tiled.len() / 3);
}
