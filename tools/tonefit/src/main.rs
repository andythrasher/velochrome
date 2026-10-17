//! Measures how Lightroom's tone rendering differs from ours, from screenshots of both.
//!
//! - `tonefit extract <manifest> <outdir>` crops the photo out of each screenshot.
//! - `tonefit linear <raw> <out.pfm> [edge=512]` writes our engine's scene-linear image.
//! - `tonefit check <linear.pfm> <ref.png> [camera offset in stops]` compares the default develop of that image with a
//!   screenshot crop of our app.
//! - `tonefit gpucheck <raw> [outdir]` draws the raw through the GPU pipeline and compares it with
//!   the tone model on the CPU, for the default and each tone slider at its extremes.
//! - `tonefit quantiles <linear.pfm> <out.csv> <ref.png>... [--channels]` compares luminance
//!   quantiles of the linear image with those of each reference.
//! - `tonefit score <manifest> <dir>` scores the tone model (core/src/tone.rs) against every
//!   Lightroom shot, using the linear images and crops in `dir`. `tonefit fit` (same arguments)
//!   adjusts the model's numbers to match Lightroom better and prints them.

mod extract;
mod gpucheck;
mod linear;
mod quantiles;
mod score;
mod util;

use std::path::Path;

fn usage() -> util::Result<()> {
    Err("usage:\n  tonefit extract <manifest> <outdir>\n  tonefit linear <raw> <out.pfm> [edge=512]\n  tonefit check <linear.pfm> <ref.png> [camera offset in stops]\n  tonefit quantiles <linear.pfm> <out.csv> <ref.png>... [--channels]".into())
}

fn main() -> util::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["extract", manifest, outdir] => extract::run(Path::new(manifest), Path::new(outdir)),
        ["linear", raw, out] => linear::linear(Path::new(raw), Path::new(out), 512),
        ["linear", raw, out, edge] => {
            let edge = edge.trim_start_matches("edge=").parse()?;
            linear::linear(Path::new(raw), Path::new(out), edge)
        }
        ["check", pfm, reference] => linear::check(Path::new(pfm), Path::new(reference), 0.0),
        ["check", pfm, reference, offset] => linear::check(Path::new(pfm), Path::new(reference), offset.parse()?),
        ["gpucheck", raw] => gpucheck::run(Path::new(raw), Path::new("target/gpucheck")),
        ["gpucheck", raw, out] => gpucheck::run(Path::new(raw), Path::new(out)),
        ["quantiles", pfm, out, rest @ ..] => {
            let channels = rest.contains(&"--channels");
            let refs: Vec<String> = rest.iter().filter(|a| **a != "--channels").map(|s| s.to_string()).collect();
            quantiles::run(Path::new(pfm), Path::new(out), &refs, channels)
        }
        ["score", manifest, dir] => score::score(Path::new(manifest), Path::new(dir)),
        ["fit", manifest, dir] => score::fit(Path::new(manifest), Path::new(dir)),
        ["pairs", manifest, dir] => score::pairs(Path::new(manifest), Path::new(dir)),
        ["basestat", manifest, dir] => score::basestat(Path::new(manifest), Path::new(dir)),
        _ => usage(),
    }
}
