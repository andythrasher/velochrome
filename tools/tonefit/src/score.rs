//! Scores the tone model against Lightroom: renders each Lightroom shot's settings with
//! `windedit_core::tone` from the raw's linear image, and compares luminance quantiles with the
//! screenshot's, in stops. `fit` adjusts the model's numbers to bring the two closer.

use std::collections::HashMap;
use std::path::Path;

use windedit_core::develop::Edit;
use windedit_core::tone::{self, Model, MODEL};

use crate::extract::read_manifest;
use crate::util::{load_linear_png, luminance, quantile_levels, quantiles, read_pfm, Pfm, Result};

/// One Lightroom shot, ready to compare: the settings, the raw's linear image and offset, and
/// the screenshot's luminance quantiles.
struct Shot {
    name: String,
    slider: String,
    edit: Edit,
    image: usize,
    offset: f32,
    lightroom: Vec<f32>,
}

struct Set {
    shots: Vec<Shot>,
    images: Vec<Pfm>,
}

fn load(manifest: &Path, dir: &Path) -> Result<Set> {
    let levels = quantile_levels();
    let (mut shots, mut images) = (Vec::new(), Vec::new());
    let mut known: HashMap<String, (usize, f32)> = HashMap::new();
    for row in read_manifest(manifest)? {
        if !row.app.starts_with("lightroom") {
            continue;
        }
        let (image, offset) = match known.get(&row.raw) {
            Some(k) => *k,
            None => {
                let stem = Path::new(&row.raw).file_stem().unwrap().to_string_lossy().to_string();
                images.push(read_pfm(&dir.join(format!("{stem}.pfm")))?);
                let frame = windedit_core::decode::decode_file(Path::new(&row.raw))?;
                let k = (images.len() - 1, tone::camera_offset(&frame.make));
                known.insert(row.raw.clone(), k);
                k
            }
        };
        let mut edit = Edit::default();
        let value: f32 = row.value.parse()?;
        match row.slider.as_str() {
            "exposure" => edit.exposure = value,
            "contrast" => edit.contrast = value,
            "highlights" => edit.highlights = value,
            "shadows" => edit.shadows = value,
            "whites" => edit.whites = value,
            "blacks" => edit.blacks = value,
            _ => {}
        }
        let name = format!("{}_{}_{}_{}", row.folder, row.app, row.slider, row.value);
        let shot = load_linear_png(&dir.join("crops").join(format!("{name}.png")))?;
        let lightroom = quantiles(&mut shot.pixels.iter().map(|p| luminance(*p)).collect::<Vec<_>>(), &levels);
        shots.push(Shot { name, slider: row.slider, edit, image, offset, lightroom });
    }
    Ok(Set { shots, images })
}

/// (rms, mean) of model − Lightroom in stops, where both are measurable: above the 8-bit floor
/// and below clipping.
fn compare(m: &Model, set: &Set, shot: &Shot) -> (f64, f64) {
    let img = &set.images[shot.image];
    let out = tone::render(m, &img.pixels, img.width, img.height, &shot.edit, shot.offset);
    let model = quantiles(&mut out.iter().map(|p| luminance(*p)).collect::<Vec<_>>(), &quantile_levels());
    // TONEFIT_DUMP=<part of a shot name> prints that shot's quantiles side by side, as 8-bit codes.
    let dump = std::env::var("TONEFIT_DUMP").unwrap_or_default();
    if !dump.is_empty() && shot.name.contains(&dump) {
        let code = |y: f32| 255.0 * crate::util::linear_to_srgb(y);
        println!("{}: quantile, model, Lightroom", shot.name);
        for i in (2..model.len()).step_by(12) {
            println!("  {:5.1}% {:6.1} {:6.1}", 0.2 + 99.6 * i as f32 / 254.0, code(model[i]), code(shot.lightroom[i]));
        }
    }
    let diffs: Vec<f64> = model
        .iter()
        .zip(&shot.lightroom)
        .filter(|(m, l)| **l > 0.004 && **l < 0.985 && **m > 0.002)
        .map(|(m, l)| (m.log2() - l.log2()) as f64)
        .collect();
    if diffs.is_empty() {
        return (0.0, 0.0);
    }
    let rms = (diffs.iter().map(|d| d * d).sum::<f64>() / diffs.len() as f64).sqrt();
    (rms, diffs.iter().sum::<f64>() / diffs.len() as f64)
}

/// The mean of squared rms over the shots of `slider` (all shots when None).
fn loss(m: &Model, set: &Set, slider: Option<&str>) -> f64 {
    let shots: Vec<&Shot> = set.shots.iter().filter(|s| slider.is_none_or(|k| s.slider == k)).collect();
    shots.iter().map(|s| compare(m, set, s).0.powi(2)).sum::<f64>() / shots.len() as f64
}

pub fn score(manifest: &Path, dir: &Path) -> Result<()> {
    let set = load(manifest, dir)?;
    report(&model_from_env()?, &set);
    Ok(())
}

fn report(m: &Model, set: &Set) {
    println!("{:<36} {:>6} {:>7}  (stops: rms, mean of model − Lightroom)", "shot", "rms", "mean");
    let mut per: HashMap<&str, (f64, usize)> = HashMap::new();
    for shot in &set.shots {
        let (rms, mean) = compare(m, set, shot);
        println!("{:<36} {rms:>6.3} {mean:>+7.3}", shot.name);
        let e = per.entry(&shot.slider).or_default();
        e.0 += rms * rms;
        e.1 += 1;
    }
    let mut keys: Vec<_> = per.keys().copied().collect();
    keys.sort();
    println!();
    for k in keys {
        let (sum, n) = per[k];
        println!("{k:<12} rms {:.3} over {n} shots", (sum / n as f64).sqrt());
    }
}

type Param = (&'static str, &'static str, fn(&mut Model) -> &mut f32, f32);

/// The model's numbers, which slider's shots decide each, and a sensible first step.
fn params() -> Vec<Param> {
    vec![
        ("contrast", "contrast", |m| &mut m.contrast, 0.05),
        ("blacks", "blacks_down", |m| &mut m.blacks_down, 0.004),
        ("blacks", "blacks_up", |m| &mut m.blacks_up, 0.004),
        ("blacks", "blacks_up_growth", |m| &mut m.blacks_up_growth, 0.05),
        ("blacks", "blacks_power", |m| &mut m.blacks_power, 0.1),
        ("highlights", "highlights_floor", |m| &mut m.highlights_floor, 0.05),
        ("highlights", "highlights_amount", |m| &mut m.highlights_amount, 0.1),
        ("highlights", "highlights_from", |m| &mut m.highlights_from, 0.25),
        ("highlights", "highlights_to", |m| &mut m.highlights_to, 0.25),
        ("highlights", "highlights_up", |m| &mut m.highlights_up, 0.05),
        ("shadows", "shadows_slope", |m| &mut m.shadows_slope, 0.05),
        ("shadows", "shadows_knee", |m| &mut m.shadows_knee, 0.2),
        ("shadows", "shadows_soft", |m| &mut m.shadows_soft, 0.1),
        ("shadows", "shadows_reach", |m| &mut m.shadows_reach, 0.25),
        ("shadows", "shadows_target", |m| &mut m.shadows_target, 0.25),
        ("shadows", "base_radius", |m| &mut m.base_radius, 0.01),
        ("shadows", "base_eps", |m| &mut m.base_eps, 0.1),
        ("whites", "whites_down", |m| &mut m.whites_down, 0.05),
        ("whites", "whites_down_from", |m| &mut m.whites_down_from, 0.25),
        ("whites", "whites_down_to", |m| &mut m.whites_down_to, 0.25),
        ("whites", "whites_down_power", |m| &mut m.whites_down_power, 0.1),
        ("whites", "whites_up", |m| &mut m.whites_up, 0.05),
        ("whites", "whites_up_from", |m| &mut m.whites_up_from, 0.25),
        ("whites", "whites_up_to", |m| &mut m.whites_up_to, 0.25),
        ("whites", "whites_up_power", |m| &mut m.whites_up_power, 0.1),
        ("whites", "whites_stretch_floor", |m| &mut m.whites_stretch_floor, 0.05),
        ("whites", "whites_stretch_power", |m| &mut m.whites_stretch_power, 0.1),
    ]
}

/// The model, with any numbers TONEFIT_SET="name=value,…" changes.
fn model_from_env() -> Result<Model> {
    let mut m = MODEL;
    for pair in std::env::var("TONEFIT_SET").unwrap_or_default().split(',').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').ok_or("TONEFIT_SET wants name=value")?;
        let p = params().into_iter().find(|p| p.1 == name).ok_or(format!("no number called {name}"))?;
        *p.2(&mut m) = value.parse()?;
    }
    Ok(m)
}

/// Coordinate descent: tries each number a step up and down, keeps what lowers the loss of its
/// slider's shots, and halves the steps once nothing helps. Each slider is fitted on its own
/// shots only, so they can't trade errors with each other.
///
/// Environment variables steer it: TONEFIT_SET="name=value,…" starts from other numbers,
/// TONEFIT_FIX="name,…" keeps those as they are, and TONEFIT_ONLY="slider" fits just one
/// slider's numbers.
pub fn fit(manifest: &Path, dir: &Path) -> Result<()> {
    let set = load(manifest, dir)?;
    let mut m = model_from_env()?;
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let (fixed, only) = (env("TONEFIT_FIX"), env("TONEFIT_ONLY"));
    let frozen = |p: &Param| fixed.split(',').any(|f| f == p.1) || (!only.is_empty() && p.0 != only);
    let mut steps: Vec<f32> = params().iter().map(|p| if frozen(p) { 0.0 } else { p.3 }).collect();
    let rounds: usize = env("TONEFIT_ROUNDS").parse().unwrap_or(6);
    for round in 0..rounds {
        let mut improved = true;
        while improved {
            improved = false;
            for (i, (slider, _, get, _)) in params().into_iter().enumerate() {
                if steps[i] == 0.0 {
                    continue;
                }
                let now = loss(&m, &set, Some(slider));
                for sign in [1.0, -1.0] {
                    let mut trial = m;
                    *get(&mut trial) += sign * steps[i];
                    if loss(&trial, &set, Some(slider)) < now - 1e-6 {
                        m = trial;
                        improved = true;
                        break;
                    }
                }
            }
        }
        steps.iter_mut().for_each(|s| *s /= 2.0);
        eprintln!("round {round}: loss {:.4}", loss(&m, &set, None));
    }
    report(&m, &set);
    println!("\nFitted model:");
    for (_, name, get, _) in params() {
        let mut copy = m;
        println!("    {name}: {:.4},", *get(&mut copy));
    }
    Ok(())
}

/// For each photo: the base the darkest pixels get, against their own brightness. Large areas of
/// shadow keep a dark base; small dark details in bright surroundings get a brighter one.
pub fn basestat(manifest: &Path, dir: &Path) -> Result<()> {
    let set = load(manifest, dir)?;
    let m = model_from_env()?;
    let mut seen = std::collections::HashSet::new();
    for shot in &set.shots {
        if !seen.insert(shot.image) {
            continue;
        }
        let img = &set.images[shot.image];
        let gain = shot.offset.exp2();
        let x: Vec<f32> = img.pixels.iter().map(|p| (luminance(*p) * gain).max(1e-9).log2()).collect();
        let edge = img.width.max(img.height) as f32;
        let factor = ((edge / tone::BASE_EDGE).ceil() as usize).max(1);
        let radius = ((edge * m.base_radius / factor as f32).round() as usize).max(1);
        let base = tone::base_layer(&x, img.width, img.height, factor, radius, m.base_eps);
        print!("{:<28}", shot.name.split('_').next().unwrap().to_string() + " (pixel → mean base)");
        for lo in [-6.0f32, -5.0, -4.0, -3.0, -2.0, -1.0] {
            let (mut sum, mut n) = (0.0, 0);
            for (v, b) in x.iter().zip(&base) {
                if *v - tone::MIDDLE >= lo && *v - tone::MIDDLE < lo + 1.0 {
                    sum += b - tone::MIDDLE;
                    n += 1;
                }
            }
            if n > 0 {
                print!("  {:+.1}:{:+.2} ({:.0}%)", lo + 0.5, sum / n as f32, 100.0 * n as f32 / x.len() as f32);
            }
        }
        println!();
    }
    Ok(())
}

/// Writes each Lightroom shot beside the model's render of the same settings (model left), to
/// `dir/pairs`, for checking by eye what quantiles can't show, like halos.
pub fn pairs(manifest: &Path, dir: &Path) -> Result<()> {
    let set = load(manifest, dir)?;
    let m = model_from_env()?;
    let out_dir = dir.join("pairs");
    std::fs::create_dir_all(&out_dir)?;
    for shot in &set.shots {
        let img = &set.images[shot.image];
        let model = tone::render(&m, &img.pixels, img.width, img.height, &shot.edit, shot.offset);
        let lr = crate::util::open_rgb(&dir.join("crops").join(format!("{}.png", shot.name)))?;
        let lr = image::imageops::resize(&lr, img.width as u32, img.height as u32, image::imageops::FilterType::Triangle);
        let mut pair = image::RgbImage::new(img.width as u32 * 2 + 8, img.height as u32);
        for (k, p) in model.iter().enumerate() {
            let c = p.map(|v| (255.0 * crate::util::linear_to_srgb(v)).round() as u8);
            pair.put_pixel((k % img.width) as u32, (k / img.width) as u32, image::Rgb(c));
        }
        image::imageops::overlay(&mut pair, &lr, img.width as i64 + 8, 0);
        pair.save(out_dir.join(format!("{}.png", shot.name)))?;
    }
    Ok(())
}
