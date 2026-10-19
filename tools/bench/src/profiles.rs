//! `bench profiles [raw...]`: camera profiles (`docs/camera-profiles.md`).
//!
//! 1. Checks the GPU `develop_profile` and `develop_profile_masks` shaders against the CPU
//!    reference (`Profile::eval_pixel`) across Adobe Standard, Camera Natural and Camera Vivid.
//! 2. Times the 4K Fit develop pass on the default path (`profile == 0`), Adobe Standard,
//!    Camera Natural and Camera Vivid (with and without masks).
//! 3. Compares Velochrome's renders of Camera Natural, Camera Vivid and Adobe Standard against
//!    Lightroom exports, Lightroom screenshots and the cameras' own JPEGs on OM-5 Mark II and
//!    Panasonic DC-S1 photos, and writes comparison strips to `renders/profiles`.

use std::path::{Path, PathBuf};

use windedit_core::color;
use windedit_core::decode::{self, Orientation, RawFrame};
use windedit_core::develop::{self, Edit};
use windedit_core::gpu::Gpu;
use windedit_core::masks::{Component, KIND_RADIAL, Mask, Masks, slider};
use windedit_core::mixer;
use windedit_core::pipeline::{
    Image, ImageMemory, MaskInput, Target, Zoom, read_rgba16f, read_target, read_tone_maps,
    render_image, render_image_masked, warm_up,
};
use windedit_core::preview;
use windedit_core::profile;
use windedit_core::tone;

const W: usize = 768;
const H: usize = 512;

fn srgb_u8(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let e = if c <= 0.0031308 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 };
    (e * 255.0).round() as u8
}

fn srgb_to_linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn scene_at(x: usize, y: usize) -> [f32; 3] {
    let (fx, fy) = ((x / 2) as f32 / (W / 2) as f32, (y / 2) as f32 / (H / 2) as f32);
    let stop = -9.0 + 9.0 * fx + 0.8 * (fy * 6.28).sin();
    let v = stop.exp2();
    let tint = [
        1.0 + 0.45 * (fy * 3.14).cos(),
        1.0,
        1.0 + 0.45 * (fy * 3.14).sin(),
    ];
    [0, 1, 2].map(|c| (v * tint[c]).clamp(0.0002, 0.98))
}

fn synthetic_frame(make: &str, model: &str, as_shot: [f32; 3], xyz_to_cam: color::Mat3) -> RawFrame {
    let mut data = vec![0u16; W * H];
    let white = 60000.0;
    for y in 0..H {
        for x in 0..W {
            let c = [[0usize, 1], [1, 2]][y & 1][x & 1];
            let s = scene_at(x, y)[c] / as_shot[c];
            data[y * W + x] = (s * white).round().clamp(0.0, white) as u16;
        }
    }
    RawFrame {
        make: make.to_string(),
        model: model.to_string(),
        data,
        width: W as u32,
        height: H as u32,
        cfa: [[0, 1], [1, 2]],
        black: [[0.0; 2]; 2],
        white,
        wb: as_shot,
        cam_to_working: color::camera_to_working(xyz_to_cam),
        xyz_to_cam,
        orientation: Orientation::Normal,
        lens: None,
        decode_time: Default::default(),
    }
}

/// The whole develop (pre-passes and all) timed between GPU timestamps, as `bench masks` does: an
/// earlier version re-dispatched the kernel with zeroed constants, so every thread returned at once
/// and the times were only overhead.
fn time_4k(gpu: &mut Gpu, image: &Image, cases: &[(&str, Edit, Masks)], runs: usize) -> Vec<f64> {
    crate::masks::time_4k(gpu, image, cases, runs)
}

/// Computes brightness quantile RMS in stops and mean OkLab ΔE between two RGBA/RGB images.
/// Uses the central 92% of each image so slight lens-crop border differences do not dominate.
fn compare_renders(a_rgba: &[u8], aw: u32, ah: u32, b_rgb: &image::RgbImage) -> (f64, f64, [f64; 3]) {
    let (r, de, ch, _) = compare_renders_signed(a_rgba, aw, ah, b_rgb);
    (r, de, ch)
}

/// Like [`compare_renders`], also giving the brightness difference (render minus reference, in
/// stops) at the 10th, 50th and 90th percentiles: which way the render is off, and where.
fn compare_renders_signed(a_rgba: &[u8], aw: u32, ah: u32, b_rgb: &image::RgbImage) -> (f64, f64, [f64; 3], [f64; 3]) {
    let (bw, bh) = b_rgb.dimensions();
    let gw = 320usize;
    let gh = 240usize;
    let sample_a = |fx: f32, fy: f32| -> [f32; 3] {
        let x = ((fx * aw as f32) as u32).min(aw - 1);
        let y = ((fy * ah as f32) as u32).min(ah - 1);
        let idx = ((y * aw + x) * 4) as usize;
        [
            srgb_to_linear(a_rgba[idx]),
            srgb_to_linear(a_rgba[idx + 1]),
            srgb_to_linear(a_rgba[idx + 2]),
        ]
    };
    let sample_b = |fx: f32, fy: f32| -> [f32; 3] {
        let x = ((fx * bw as f32) as u32).min(bw - 1);
        let y = ((fy * bh as f32) as u32).min(bh - 1);
        let p = b_rgb.get_pixel(x, y).0;
        [srgb_to_linear(p[0]), srgb_to_linear(p[1]), srgb_to_linear(p[2])]
    };

    let mut ya = Vec::with_capacity(gw * gh);
    let mut yb = Vec::with_capacity(gw * gh);
    let mut ra = [Vec::with_capacity(gw * gh), Vec::with_capacity(gw * gh), Vec::with_capacity(gw * gh)];
    let mut rb = [Vec::with_capacity(gw * gh), Vec::with_capacity(gw * gh), Vec::with_capacity(gw * gh)];
    let mut sum_de = 0.0f64;

    for gy in 0..gh {
        let fy = 0.04 + 0.92 * (gy as f32 + 0.5) / gh as f32;
        for gx in 0..gw {
            let fx = 0.04 + 0.92 * (gx as f32 + 0.5) / gw as f32;
            let pa = sample_a(fx, fy);
            let pb = sample_b(fx, fy);
            let la = (0.2126 * pa[0] + 0.7152 * pa[1] + 0.0722 * pa[2]).max(1e-6).log2();
            let lb = (0.2126 * pb[0] + 0.7152 * pb[1] + 0.0722 * pb[2]).max(1e-6).log2();
            ya.push(la);
            yb.push(lb);
            for c in 0..3 {
                ra[c].push(pa[c].max(1e-6).log2());
                rb[c].push(pb[c].max(1e-6).log2());
            }
            let lab_a = mixer::to_oklab(pa);
            let lab_b = mixer::to_oklab(pb);
            let dl = (lab_a[0] - lab_b[0]) as f64;
            let da = (lab_a[1] - lab_b[1]) as f64;
            let db = (lab_a[2] - lab_b[2]) as f64;
            sum_de += (dl * dl + da * da + db * db).sqrt();
        }
    }

    ya.sort_by(f32::total_cmp);
    yb.sort_by(f32::total_cmp);
    for c in 0..3 {
        ra[c].sort_by(f32::total_cmp);
        rb[c].sort_by(f32::total_cmp);
    }

    // Quantile RMS over 5th..98th percentiles (same range tonefit uses).
    let n = ya.len();
    let lo = n * 5 / 100;
    let hi = n * 98 / 100;
    let mut sq = 0.0f64;
    let mut ch_sq = [0.0f64; 3];
    for i in lo..hi {
        let d = (ya[i] - yb[i]) as f64;
        sq += d * d;
        for c in 0..3 {
            let dc = (ra[c][i] - rb[c][i]) as f64;
            ch_sq[c] += dc * dc;
        }
    }
    let count = (hi - lo) as f64;
    let rms_stops = (sq / count).sqrt();
    let ch_rms = ch_sq.map(|s| (s / count).sqrt());
    let mean_de = sum_de / (gw * gh) as f64;
    let at = |q: usize| (ya[n * q / 100] - yb[n * q / 100]) as f64;
    (rms_stops, mean_de, ch_rms, [at(10), at(50), at(90)])
}

fn write_strip(path: &Path, tiles: &[(&str, &[u8], u32, u32)]) {
    if tiles.is_empty() {
        return;
    }
    let h = tiles[0].3 as usize;
    let gap = 6usize;
    let total_w: usize = tiles.iter().map(|t| t.2 as usize).sum::<usize>() + gap * (tiles.len() - 1);
    let mut out = vec![30u8; total_w * h * 4];
    let mut x0 = 0usize;
    for &(_, px, w, th) in tiles {
        let w = w as usize;
        let th = (th as usize).min(h);
        for y in 0..th {
            out[(y * total_w + x0) * 4..(y * total_w + x0 + w) * 4]
                .copy_from_slice(&px[y * w * 4..(y + 1) * w * 4]);
        }
        x0 += w + gap;
    }
    image::save_buffer(path, &out, total_w as u32, h as u32, image::ExtendedColorType::Rgba8).unwrap();
}

pub fn bench_profiles(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {}", gpu.adapter_name);
    warm_up(&mut gpu).unwrap();
    let out_dir = std::env::current_exe().unwrap().parent().unwrap().join("renders").join("profiles");
    std::fs::create_dir_all(&out_dir).unwrap();

    // ---- 1. GPU vs CPU reference across profiles ----
    println!("\n1. GPU vs CPU reference on synthetic {W}x{H} photo (levels 0..255: mean, 99th percentile, max)");
    let om_profiles = profile::profiles_for_camera("OM Digital Solutions", "OM-5MarkII");
    let s1_profiles = profile::profiles_for_camera("Panasonic", "DC-S1");
    println!(
        "  Discovered {} profiles for OM-5 Mark II, {} profiles for Panasonic DC-S1",
        om_profiles.len(),
        s1_profiles.len()
    );

    // Get real xyz_to_cam from a sample profile so camera-to-ProPhoto is realistic.
    let sample_id = om_profiles.iter().find(|(_, n)| n == "Adobe Standard").unwrap().0;
    let sample_prof = profile::get("OM Digital Solutions", "OM-5MarkII", sample_id).unwrap();
    let xyz_to_cam = sample_prof.color_matrix2.or(sample_prof.color_matrix1).unwrap();
    let as_shot = develop::temperature_to_multipliers(&xyz_to_cam, 5500.0, 0.0);
    let frame = synthetic_frame("OM Digital Solutions", "OM-5MarkII", as_shot, xyz_to_cam);
    let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap();
    let target = Target::new(&gpu, W as u32, H as u32).unwrap();
    let linear_rgba = read_rgba16f(&mut gpu, &image.linear, 0, (0, 0, W as u32, H as u32)).unwrap();
    let scene: Vec<[f32; 3]> = linear_rgba.iter().map(|p| [p[0], p[1], p[2]]).collect();

    let mut plain = Edit::default();
    plain.set_detail(develop::Detail { amount: 0.0, color: 0.0, ..develop::DETAIL_DEFAULTS });

    for p_name in ["Adobe Standard", "Camera Natural", "Camera Vivid", "Camera Portrait", "Camera Landscape"] {
        let Some(&(pid, _)) = om_profiles.iter().find(|(_, n)| n == p_name) else { continue };
        let prof = profile::get("OM Digital Solutions", "OM-5MarkII", pid).unwrap();
        for (case_label, mut edit, use_mask) in [
            ("as-shot", Edit { profile: pid, ..plain }, false),
            (
                "3200K + sliders",
                Edit {
                    profile: pid,
                    custom_white_balance: 1,
                    temperature: 3200.0,
                    tint: 12.0,
                    exposure: 0.4,
                    contrast: 25.0,
                    highlights: -40.0,
                    shadows: 45.0,
                    whites: 20.0,
                    blacks: -15.0,
                    ..plain
                },
                false,
            ),
            (
                "with radial mask",
                Edit { profile: pid, exposure: 0.2, shadows: 30.0, ..plain },
                true,
            ),
        ] {
            let mut masks = Masks::default();
            if use_mask {
                let mut m = Mask::new();
                m.components[0] = Component {
                    kind: KIND_RADIAL,
                    mode: 0,
                    p: [0.5, 0.33, 0.25, 0.18, 0.0, 60.0, 0.0, 0.0],
                };
                m.count = 1;
                m.sliders[slider::EXPOSURE] = 0.6;
                m.sliders[slider::SHADOWS] = 40.0;
                masks.masks[0] = m;
                masks.count = 1;
            }
            edit.clean_detail();
            gpu.begin().unwrap();
            render_image_masked(
                &gpu,
                &image,
                &edit,
                &MaskInput { masks: &masks, overlay: None, brush: None },
                Zoom::Fit,
                &target,
            );
            gpu.submit().unwrap();
            let gpu_px = read_target(&mut gpu, &target).unwrap();
            let maps = read_tone_maps(&mut gpu, &image).unwrap();
            let (sw, sh) = (
                (W as u32).div_ceil(((W.max(H) as f32 / tone::BASE_EDGE).ceil() as u32).max(1)) as usize,
                (H as u32).div_ceil(((W.max(H) as f32 / tone::BASE_EDGE).ceil() as u32).max(1)) as usize,
            );

            let wb = develop::white_balance_change(&edit, &image.xyz_to_cam, image.as_shot);
            let temp_k = if edit.custom_white_balance != 0 { edit.temperature } else { image.as_shot_kelvin };
            let cam_to_pro = prof.cam_to_prophoto(temp_k, &wb, &image.xyz_to_cam);
            let hs_table = prof.interpolated_hue_sat(temp_k);
            let exposure_gain = image.camera_offset + prof.baseline_exposure_offset + edit.exposure;
            let gain = exposure_gain.exp2();
            let to_srgb = color::mul(&color::REC2020_TO_SRGB, &image.cam_to_working);
            let lum = [0, 1, 2].map(|c| {
                gain * wb[c] * (0.2126 * to_srgb[0][c] + 0.7152 * to_srgb[1][c] + 0.0722 * to_srgb[2][c])
            });
            let sl = tone::Sliders::from_edit(&edit);
            let mask_set = windedit_core::masks::GpuMaskSet::new(&masks, &image.xyz_to_cam);
            let stats = tone::Stats { top: maps.stats[0], dark: maps.stats[1] };

            let sample_ab = |u: f32, v: f32| -> [f32; 2] {
                let x = (u * sw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
                let y = (v * sh as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
                let (x0, y0) = (x as usize, y as usize);
                let (x1, y1) = ((x0 + 1).min(sw - 1), (y0 + 1).min(sh - 1));
                let (fx, fy) = (x - x0 as f32, y - y0 as f32);
                let c = |ix: usize, iy: usize| maps.coefficients[iy * sw + ix];
                let (c00, c10, c01, c11) = (c(x0, y0), c(x1, y0), c(x0, y1), c(x1, y1));
                [0, 1].map(|k| {
                    let top = c00[k] + (c10[k] - c00[k]) * fx;
                    let bot = c01[k] + (c11[k] - c01[k]) * fx;
                    top + (bot - top) * fy
                })
            };

            let mut diffs = Vec::with_capacity(W * H * 3);
            for y in 0..H {
                for x in 0..W {
                    let u = (x as f32 + 0.5) / W as f32;
                    let v = (y as f32 + 0.5) / H as f32;
                    let mut cam = scene[y * W + x];
                    let mut sl_px = sl;
                    let mut mask_exp = 0.0f32;
                    if masks.count != 0 {
                        let pm = [u, v * H as f32 / W as f32];
                        let ml = mask_set.local(pm);
                        for c in 0..3 {
                            cam[c] *= ml.wb_log2[c].exp2();
                        }
                        mask_exp = ml.exposure;
                        sl_px.contrast = (sl_px.contrast + ml.contrast).clamp(-1.0, 1.0);
                        sl_px.highlights = (sl_px.highlights + ml.highlights).clamp(-1.0, 1.0);
                        sl_px.shadows = (sl_px.shadows + ml.shadows).clamp(-1.0, 1.0);
                        sl_px.whites = (sl_px.whites + ml.whites).clamp(-1.0, 1.0);
                        sl_px.blacks = (sl_px.blacks + ml.blacks).clamp(-1.0, 1.0);
                    }
                    let y_lum = (lum[0] * cam[0] + lum[1] * cam[1] + lum[2] * cam[2]) * mask_exp.exp2();
                    let lx = y_lum.max(1e-9).log2();
                    let xg = tone::global(&tone::MODEL, lx, &sl_px);
                    let ab = sample_ab(u, v);
                    let base = ab[0] * xg + ab[1];
                    let shifted = xg + tone::local(&tone::MODEL, xg, base, &sl_px, &stats);
                    let tone_shift = shifted - lx;
                    let srgb = prof.eval_pixel(
                        cam,
                        &cam_to_pro,
                        &hs_table,
                        (exposure_gain + mask_exp).exp2(),
                        tone_shift.exp2(),
                    );
                    let idx = (y * W + x) * 4;
                    for c in 0..3 {
                        let cpu_b = srgb_u8(srgb[c]);
                        let gpu_b = gpu_px[idx + c];
                        diffs.push(cpu_b.abs_diff(gpu_b));
                    }
                }
            }
            diffs.sort_unstable();
            let mean = diffs.iter().map(|&d| d as f64).sum::<f64>() / diffs.len() as f64;
            let p99 = diffs[diffs.len() * 99 / 100];
            let max = *diffs.last().unwrap();
            println!("  {p_name:<18} {case_label:<18} mean {mean:5.2}  p99 {p99:3}  max {max:3}");
            assert!(p99 <= 1 && max <= 2, "GPU/CPU mismatch on {p_name} ({case_label}): p99={p99}, max={max}");
        }
    }

    // ---- 2. 4K Fit develop pass timings on real photos ----
    println!("\n2. 4K Fit develop pass timings (median of 60 dispatches)");
    for f in files {
        let frame = decode::decode_file(f).unwrap();
        let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap();
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let profs = profile::profiles_for_camera(&image.make, &image.model);
        let find_id = |label: &str| profs.iter().find(|(_, n)| n == label).map(|&(id, _)| id).unwrap_or(0);
        let id_adobe = find_id("Adobe Standard");
        let id_nat = find_id("Camera Natural");
        let id_viv = find_id("Camera Vivid");

        let mut mask1 = Masks::default();
        mask1.masks[0] = Mask::new();
        mask1.masks[0].components[0] = Component {
            kind: KIND_RADIAL,
            mode: 0,
            p: [0.5, 0.35, 0.25, 0.20, 0.0, 50.0, 0.0, 0.0],
        };
        mask1.masks[0].count = 1;
        mask1.masks[0].sliders[slider::EXPOSURE] = 0.5;
        mask1.count = 1;

        let cases = [
            ("Velochrome (default)", Edit::default(), Masks::default()),
            ("Adobe Standard", Edit { profile: id_adobe, ..Edit::default() }, Masks::default()),
            ("Camera Natural", Edit { profile: id_nat, ..Edit::default() }, Masks::default()),
            ("Camera Vivid", Edit { profile: id_viv, ..Edit::default() }, Masks::default()),
            ("Velochrome + 1 mask", Edit::default(), mask1),
            ("Camera Natural + 1 mask", Edit { profile: id_nat, ..Edit::default() }, mask1),
        ];
        let times = time_4k(&mut gpu, &image, &cases, 60);
        println!("  {name} ({} {}):", image.make, image.model);
        for ((label, _, _), ms) in cases.iter().zip(times.iter()) {
            let delta = ms - times[0];
            println!("    {label:<26} {ms:6.3} ms ({delta:+6.3} ms)");
        }
    }

    // ---- 3. Comparison with camera JPEGs and Lightroom on OM-5 Mark II & DC-S1 ----
    println!("\n3. Comparison against camera JPEGs and Lightroom references");
    println!("   (Quantile RMS in stops over 5th..98th percentiles; mean OkLab ΔE)");
    for f in files {
        let frame = decode::decode_file(f).unwrap();
        let (image, _) = Image::open(&mut gpu, &mut ImageMemory::new(), &frame).unwrap();
        let stem = f.file_stem().unwrap().to_string_lossy().to_string();
        let profs = profile::profiles_for_camera(&image.make, &image.model);
        let find_id = |label: &str| profs.iter().find(|(_, n)| n == label).map(|&(id, _)| id).unwrap_or(0);

        let (ow, oh) = image.oriented_size();
        let tw = 1200u32;
        let th = (tw * oh / ow).max(1);
        let target = Target::new(&gpu, tw, th).unwrap();

        let render_prof = |gpu: &mut Gpu, pid: u32, base_edit: &Edit| -> Vec<u8> {
            let e = Edit { profile: pid, ..*base_edit };
            gpu.begin().unwrap();
            render_image(gpu, &image, &e, Zoom::Fit, &target);
            gpu.submit().unwrap();
            read_target(gpu, &target).unwrap()
        };

        // Load embedded camera JPEG (or companion .JPG) for comparison.
        let companion = f.with_extension("JPG");
        let photo = windedit_core::library::Photo {
            name: stem.clone(),
            raw: Some(f.clone()),
            jpeg: companion.exists().then_some(companion),
        };
        let cam_jpeg = preview::load(&photo, preview::Want::Cover(1200)).ok().map(|p| {
            let mut rgb = image::RgbImage::new(p.pixels.width, p.pixels.height);
            for (i, px) in p.pixels.bgra.chunks_exact(4).enumerate() {
                let x = (i as u32) % p.pixels.width;
                let y = (i as u32) / p.pixels.width;
                rgb.put_pixel(x, y, image::Rgb([px[2], px[1], px[0]]));
            }
            match p.orientation {
                Orientation::Rotate90 => image::imageops::rotate90(&rgb),
                Orientation::Rotate180 => image::imageops::rotate180(&rgb),
                Orientation::Rotate270 => image::imageops::rotate270(&rgb),
                _ => rgb,
            }
        });

        println!("\n  Photo: {stem} ({} {})", image.make, image.model);
        let mut strip_tiles: Vec<(&str, Vec<u8>, u32, u32)> = Vec::new();
        for p_label in ["Velochrome", "Adobe Standard", "Camera Natural", "Camera Vivid"] {
            let pid = if p_label == "Velochrome" { 0 } else { find_id(p_label) };
            let px = render_prof(&mut gpu, pid, &Edit::default());
            if let Some(ref cj) = cam_jpeg {
                let (rms, de, ch, bias) = compare_renders_signed(&px, tw, th, cj);
                println!(
                    "    vs camera JPEG  | {p_label:<16} | lum RMS: {rms:.3} stops (R {:.2} G {:.2} B {:.2}) | OkLab ΔE: {de:.4} | bias p10 {:+.2} p50 {:+.2} p90 {:+.2}",
                    ch[0], ch[1], ch[2], bias[0], bias[1], bias[2]
                );
            }
            strip_tiles.push((p_label, px, tw, th));
        }

        // Check if there is a Lightroom crop in target/tonefit/crops for this photo.
        let lr_crop_path = match stem.as_str() {
            "PA150285" => Some(PathBuf::from(r"target\tonefit\crops\1_lightroom_none_0.png")),
            "P1020780" | "P1020781" => Some(PathBuf::from(r"target\tonefit\crops\2_lightroom-library_none_0.png")),
            "P6140518" => Some(PathBuf::from(r"target\tonefit\crops\3_lightroom-library_none_0.png")),
            _ => None,
        };
        if let Some(p) = lr_crop_path.filter(|p| p.exists())
            && let Ok(lr_img) = image::open(&p)
        {
            let lr_rgb = lr_img.to_rgb8();
            for (p_label, px, _, _) in &strip_tiles {
                let (rms, de, ch) = compare_renders(px, tw, th, &lr_rgb);
                println!(
                    "    vs LR default   | {p_label:<16} | lum RMS: {rms:.3} stops (R {:.2} G {:.2} B {:.2}) | OkLab ΔE: {de:.4}",
                    ch[0], ch[1], ch[2]
                );
            }
        }

        // Also check if there is a Lightroom .xmp sidecar beside the file (e.g. in Lightroom Imports).
        let xmp_path = f.with_extension("xmp");
        if xmp_path.exists() {
            let meta = windedit_core::xmp::read(&xmp_path);
            if meta.edit.profile != 0 {
                let pname = profile::name_of(meta.edit.profile).unwrap_or_default();
                println!("    Lightroom sidecar profile: {pname} (id={})", meta.edit.profile);
            }
        }

        let refs: Vec<(&str, &[u8], u32, u32)> =
            strip_tiles.iter().map(|(l, px, w, h)| (*l, px.as_slice(), *w, *h)).collect();
        let out_path = out_dir.join(format!("{stem}-profiles.png"));
        write_strip(&out_path, &refs);
        println!("    wrote {}", out_path.display());
    }
}
