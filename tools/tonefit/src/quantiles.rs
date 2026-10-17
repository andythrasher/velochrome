//! Geometry-free tone comparison: luminance quantiles of our linear image against those of
//! reference screenshots.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::util::*;

/// Quantiles of one image: luminance, and optionally each channel.
fn channel_quantiles(pixels: &[[f32; 3]], channels: bool) -> Vec<(&'static str, Vec<f32>)> {
    let levels = quantile_levels();
    let mut out = vec![("Y", quantiles(&mut pixels.iter().map(|&p| luminance(p)).collect::<Vec<_>>(), &levels))];
    if channels {
        for (c, name) in ["R", "G", "B"].into_iter().enumerate() {
            out.push((name, quantiles(&mut pixels.iter().map(|p| p[c]).collect::<Vec<_>>(), &levels)));
        }
    }
    out
}

fn log2_ev(v: f32) -> f32 {
    v.max(1e-9).log2()
}

pub fn run(linear: &Path, out: &Path, refs: &[String], channels: bool) -> Result<()> {
    let levels = quantile_levels();
    let scene = read_pfm(linear)?;
    let mut names = vec!["q".to_string()];
    let mut values = vec![levels.iter().map(|&q| (q * 100.0) as f32).collect::<Vec<_>>()];
    for (name, q) in channel_quantiles(&scene.pixels, channels) {
        names.push(if name == "Y" { "scene_ev".into() } else { format!("scene_ev_{name}") });
        values.push(q.into_iter().map(log2_ev).collect());
    }

    let mut stats = String::from("ref,clipped_fraction,black_fraction\n");
    for path in refs {
        let path = Path::new(path);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("ref").to_string();
        let img = open_rgb(path)?;
        let total = (img.width() * img.height()) as f64;
        let clipped = img.pixels().filter(|p| p.0.iter().any(|&v| v >= 254)).count() as f64 / total;
        let black = img.pixels().filter(|p| p.0.iter().all(|&v| v <= 1)).count() as f64 / total;
        stats += &format!("{stem},{clipped:.6},{black:.6}\n");
        for (name, q) in channel_quantiles(&load_linear_png(path)?.pixels, channels) {
            let tag = if name == "Y" { String::new() } else { format!("{name}_") };
            names.push(format!("{stem}_{tag}Y"));
            values.push(q.clone());
            names.push(format!("{stem}_{tag}code"));
            values.push(q.iter().map(|&v| linear_to_srgb(v) * 255.0).collect());
        }
    }

    let mut file = File::create(out)?;
    writeln!(file, "{}", names.join(","))?;
    for row in 0..levels.len() {
        let cells: Vec<String> = values.iter().map(|c| format!("{:.6}", c[row])).collect();
        writeln!(file, "{}", cells.join(","))?;
    }
    // The clipped and black fractions are one number per reference, so they go in a side file.
    std::fs::write(out.with_extension("stats.csv"), stats)?;
    Ok(())
}
