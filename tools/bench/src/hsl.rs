//! `bench hsl [raw...]`: the colour mixer (docs/phase2c.md). Renders a synthetic hue/lightness
//! chart and the given raws with a few mixer settings, writes side-by-side PNGs (original | edited)
//! to `renders/hsl` beside the executable, measures what moved at the range centres, and times the
//! develop pass with the mixer on and off.

use std::path::{Path, PathBuf};

use windedit_core::color;
use windedit_core::decode::{self, Orientation, RawFrame};
use windedit_core::gpu::Gpu;
use windedit_core::mixer::{self, CENTRES, NAMES, RANGES};
use windedit_core::pipeline::{Edit, Image, ImageMemory, Target, Zoom, read_target, render_image, warm_up};

const W: usize = 1440;
const BAND: usize = 80;
/// Lightness (OkLab L) of the chart's coloured bands, top to bottom; the last band is a grey ramp.
const LIGHTNESS: [f32; 6] = [0.30, 0.45, 0.60, 0.72, 0.84, 0.92];
const CHART_CHROMA: f32 = 0.11;

/// A named mixer setting: slider index → value.
struct Setting {
    name: &'static str,
    file: &'static str,
    sliders: Vec<(usize, f32)>,
}

fn hue(r: usize) -> usize {
    r
}
fn sat(r: usize) -> usize {
    RANGES + r
}
fn lum(r: usize) -> usize {
    2 * RANGES + r
}

fn settings() -> Vec<Setting> {
    vec![
        Setting { name: "Blue Luminance -100", file: "blue-lum-100", sliders: vec![(lum(5), -100.0)] },
        Setting { name: "Orange Saturation +100", file: "orange-sat+100", sliders: vec![(sat(1), 100.0)] },
        Setting { name: "Green Hue +100", file: "green-hue+100", sliders: vec![(hue(3), 100.0)] },
        Setting {
            name: "Red all -100",
            file: "red-all-100",
            sliders: vec![(hue(0), -100.0), (sat(0), -100.0), (lum(0), -100.0)],
        },
    ]
}

fn edit_of(s: &Setting) -> Edit {
    let mut e = Edit::default();
    for &(i, v) in &s.sliders {
        e.mixer[i] = v;
    }
    e
}

fn srgb_to_linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// The colour with this OkLab lightness and hue, at `chroma` or the most the gamut allows (90%).
fn chart_colour(l: f32, h: f32, chroma: f32) -> [f32; 3] {
    let ok = |c: f32| mixer::from_lch(l, c, h).iter().all(|v| (0.0..=1.0).contains(v));
    if ok(chroma) {
        return mixer::from_lch(l, chroma, h);
    }
    let (mut lo, mut hi) = (0.0, chroma);
    for _ in 0..20 {
        let mid = 0.5 * (lo + hi);
        if ok(mid) { lo = mid } else { hi = mid }
    }
    mixer::from_lch(l, lo * 0.9, h)
}

/// The chart as a synthetic raw: hue (OkLCh, degrees = 360 · column / width) across, lightness
/// down, the last band neutral greys from black to white. Camera RGB is linear sRGB (so the
/// pipeline sees these colours as scene light), mosaicked RGGB at 16 bits.
fn chart_frame() -> RawFrame {
    let h = BAND * (LIGHTNESS.len() + 1);
    let mut rgb = vec![[0f32; 3]; W * h];
    for (b, &l) in LIGHTNESS.iter().enumerate() {
        for x in 0..W {
            let c = chart_colour(l, 360.0 * (x as f32 + 0.5) / W as f32, CHART_CHROMA);
            for y in 0..BAND {
                rgb[(b * BAND + y) * W + x] = c;
            }
        }
    }
    for x in 0..W {
        // Grey ramp, perceptually even.
        let v = (x as f32 / (W - 1) as f32).powi(3);
        for y in 0..BAND {
            rgb[(LIGHTNESS.len() * BAND + y) * W + x] = [v; 3];
        }
    }
    let mut data = vec![0u16; W * h];
    for y in 0..h {
        for x in 0..W {
            let ch = [[0, 1], [1, 2]][y % 2][x % 2];
            data[y * W + x] = (rgb[y * W + x][ch].clamp(0.0, 1.0) * 60000.0).round() as u16;
        }
    }
    RawFrame {
        make: String::new(),
        model: "chart".into(),
        data,
        width: W as u32,
        height: h as u32,
        cfa: [[0, 1], [1, 2]],
        black: [[0.0; 2]; 2],
        white: 60000.0,
        wb: [1.0; 3],
        cam_to_working: color::invert(&color::REC2020_TO_SRGB),
        xyz_to_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        orientation: Orientation::Normal,
        lens: None,
        decode_time: Default::default(),
    }
}

fn render(gpu: &mut Gpu, image: &Image, edit: &Edit, zoom: Zoom, target: &Target) -> Vec<u8> {
    gpu.begin().unwrap();
    render_image(gpu, image, edit, zoom, target);
    gpu.submit().unwrap();
    read_target(gpu, target).unwrap()
}

/// OkLCh of an RGBA8 pixel.
fn lch(px: &[u8], i: usize) -> [f32; 3] {
    mixer::to_lch([srgb_to_linear(px[i * 4]), srgb_to_linear(px[i * 4 + 1]), srgb_to_linear(px[i * 4 + 2])])
}

fn wrap(d: f32) -> f32 {
    (d + 180.0).rem_euclid(360.0) - 180.0
}

fn luminance(px: &[u8], i: usize) -> f32 {
    let c = |k: usize| srgb_to_linear(px[i * 4 + k]);
    0.2126 * c(0) + 0.7152 * c(1) + 0.0722 * c(2)
}

/// Original on the left, edited on the right, a 6-pixel white gap.
fn side_by_side(a: &[u8], b: &[u8], w: usize, h: usize) -> (usize, Vec<u8>) {
    let total = 2 * w + 6;
    let mut out = vec![255u8; total * h * 4];
    for y in 0..h {
        out[y * total * 4..(y * total + w) * 4].copy_from_slice(&a[y * w * 4..(y + 1) * w * 4]);
        out[(y * total + w + 6) * 4..(y * total + 2 * w + 6) * 4].copy_from_slice(&b[y * w * 4..(y + 1) * w * 4]);
    }
    (total, out)
}

fn save(path: &Path, px: &[u8], w: usize, h: usize) {
    image::save_buffer(path, px, w as u32, h as u32, image::ExtendedColorType::Rgba8).unwrap();
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

pub fn bench_hsl(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {}", gpu.adapter_name);
    warm_up(&mut gpu).unwrap();
    let out_dir = std::env::current_exe().unwrap().parent().unwrap().join("renders").join("hsl");
    std::fs::create_dir_all(&out_dir).unwrap();

    // ---- The chart ----
    let frame = chart_frame();
    let (cw, ch) = (frame.width as usize, frame.height as usize);
    let mut memory = ImageMemory::new();
    let (image, _) = Image::open(&mut gpu, &mut memory, &frame).unwrap();
    let target = Target::new(&gpu, cw as u32, ch as u32).unwrap();
    let base = render(&mut gpu, &image, &Edit::default(), Zoom::Fit, &target);
    save(&out_dir.join("chart-original.png"), &base, cw, ch);
    println!("\nchart {cw}x{ch}: columns are OkLCh hue 0..360, rows lightness {LIGHTNESS:?}, last band a grey ramp");

    // The column (in the band at lightness 0.60) whose rendered original hue is nearest each centre.
    let probe_y = 2 * BAND + BAND / 2;
    let column_at = |c: f32| -> usize {
        (0..cw).min_by(|&a, &b| wrap(lch(&base[probe_y * cw * 4..], a)[2] - c).abs().total_cmp(&wrap(lch(&base[probe_y * cw * 4..], b)[2] - c).abs())).unwrap()
    };
    let columns: Vec<usize> = CENTRES.iter().map(|&c| column_at(c)).collect();
    println!("range centres (OkLCh hue) and the chart column nearest each in the rendered original: {}", (0..RANGES).map(|r| format!("{} {:.1}° @{}", NAMES[r], CENTRES[r], columns[r])).collect::<Vec<_>>().join(", "));

    for s in settings() {
        let edit = edit_of(&s);
        let out = render(&mut gpu, &image, &edit, Zoom::Fit, &target);
        let (tw, px) = side_by_side(&base, &out, cw, ch);
        let file = format!("chart-{}.png", s.file);
        save(&out_dir.join(&file), &px, tw, ch);
        println!("\n{} -> {file}", s.name);
        println!("  at the range centres (mid lightness band): hue shift ° / chroma ratio / luminance stops");
        for r in 0..RANGES {
            let i = probe_y * cw + columns[r];
            let (a, b) = (lch(&base, i), lch(&out, i));
            let stops = (luminance(&out, i) / luminance(&base, i).max(1e-6)).log2();
            println!("    {:<8} {:+7.2}° {:6.3} {:+6.2}", NAMES[r], wrap(b[2] - a[2]), b[1] / a[1].max(1e-6), stops);
        }
        // The greys, over the whole ramp: the largest change in any channel.
        let grey_y = (LIGHTNESS.len() * BAND + BAND / 2) * cw;
        let grey = (0..cw).map(|x| (0..3).map(|k| (out[(grey_y + x) * 4 + k] as i32 - base[(grey_y + x) * 4 + k] as i32).abs()).max().unwrap()).max().unwrap();
        println!("  grey ramp: largest change {grey} levels");
        // How wide the effect is: the columns (mid band) where any channel moved by more than 2 levels.
        let moved: Vec<usize> = (0..cw).filter(|&x| (0..3).any(|k| (out[(probe_y * cw + x) * 4 + k] as i32 - base[(probe_y * cw + x) * 4 + k] as i32).abs() > 2)).collect();
        if let (Some(&lo), Some(&hi)) = (moved.first(), moved.last()) {
            let hue_of = |x: usize| lch(&base[probe_y * cw * 4..], x)[2];
            println!("  pixels moved by more than 2 levels: {:.1}% of the hues, columns {lo}..{hi} (original hue {:.0}° .. {:.0}°)", 100.0 * moved.len() as f32 / cw as f32, hue_of(lo), hue_of(hi));
        } else {
            println!("  nothing moved by more than 2 levels");
        }
    }

    // ---- The photos ----
    for f in files.iter().take(2) {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let frame = match decode::decode_file(f) {
            Ok(frame) => frame,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap();
        let (w, h) = image.oriented_size();
        let (tw, th) = if w >= h { (1200, 1200 * h / w) } else { (1200 * w / h, 1200) };
        let (tw, th) = (tw as usize, th as usize);
        let target = Target::new(&gpu, tw as u32, th as u32).unwrap();
        let base = render(&mut gpu, &image, &Edit::default(), Zoom::Fit, &target);
        println!("\n{name} ({}x{}), shown {tw}x{th}", frame.width, frame.height);
        for s in settings() {
            let out = render(&mut gpu, &image, &edit_of(&s), Zoom::Fit, &target);
            let (pw, px) = side_by_side(&base, &out, tw, th);
            save(&out_dir.join(format!("{name}-{}.png", s.file)), &px, pw, th);
            let changed = (0..tw * th).filter(|&i| (0..3).any(|k| (out[i * 4 + k] as i32 - base[i * 4 + k] as i32).abs() > 3)).count();
            println!("  {:<24} {:5.1}% of pixels changed by more than 3 levels", s.name, 100.0 * changed as f32 / (tw * th) as f32);
        }
        // Everything at once, the 24-slider worst case, and the timing.
        timing(&mut gpu, &image, &name);
    }
    if files.is_empty() {
        println!("\n(no raws given: only the chart)");
        timing(&mut gpu, &image, "chart");
    }
    println!("\nrenders in {}", out_dir.display());
}

/// The develop pass at 1440p with the mixer off, with one range moved, and with all 24 sliders.
fn timing(gpu: &mut Gpu, image: &Image, name: &str) {
    let target = Target::new(gpu, 2560, 1440).unwrap();
    let mut all = Edit::default();
    for (i, v) in all.mixer.iter_mut().enumerate() {
        *v = if i % 2 == 0 { 100.0 } else { -100.0 };
    }
    let one = edit_of(&settings()[1]);
    let cases = [("mixer off (all zeros)", Edit::default()), ("Orange Saturation +100", one), ("all 24 sliders at ±100", all)];
    for (view, zoom) in [("Fit 1440p", Zoom::Fit), ("100% 1440p", Zoom::Actual { x: 0.5, y: 0.5 })] {
        // Interleave the cases and repeat, so clocks and caches treat them alike.
        let mut times = vec![Vec::new(); cases.len()];
        for round in 0..80 {
            for (k, (_, edit)) in cases.iter().enumerate() {
                // Exposure changes each frame, as when dragging a slider (so nothing is cached).
                let edit = Edit { exposure: (round as f32 / 80.0) * 2.0 - 1.0, ..*edit };
                gpu.begin().unwrap();
                let a = gpu.timestamp();
                render_image(gpu, image, &edit, zoom, &target);
                let b = gpu.timestamp();
                let ts = gpu.submit().unwrap();
                times[k].push(ts[b as usize] - ts[a as usize]);
            }
        }
        let off = median(times[0].clone());
        for (k, (label, _)) in cases.iter().enumerate() {
            let m = median(times[k].clone());
            println!("  timing {name} {view}: {label:<26} {m:.3} ms{}", if k == 0 { String::new() } else { format!(" ({:+.3})", m - off) });
        }
    }
}
