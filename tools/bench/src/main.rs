//! Phase 0 benchmark: how fast can we decode, preview and develop raw files on this machine?
//!
//!     bench decode <file or folder>...   decode times, one at a time and on all cores
//!     bench preview <file or folder>...  embedded JPEG preview times
//!     bench gpu <file or folder>...      open and develop times on the GPU; writes a PNG of
//!                                        each photo's fitted view next to the benchmark

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use windedit_core::decode;
use windedit_core::gpu::Gpu;
use windedit_core::pipeline::{Edit, Image, ImageMemory, OpenTimings, Target, Zoom, read_target, render, warm_up};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("usage: bench <decode|preview> <file or folder>...");
        std::process::exit(2);
    };
    let files = collect_files(rest);
    match cmd.as_str() {
        "decode" => bench_decode(&files),
        "preview" => bench_preview(&files),
        "gpu" => bench_gpu(&files),
        _ => {
            eprintln!("unknown command {cmd}");
            std::process::exit(2);
        }
    }
}

/// Raw files named on the command line, or found in the named folders (not recursive).
fn collect_files(args: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for arg in args {
        let path = PathBuf::from(arg);
        if path.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| is_raw(p))
                .collect();
            found.sort();
            files.extend(found);
        } else {
            files.push(path);
        }
    }
    files
}

fn is_raw(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "orf" | "rw2")
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn bench_decode(files: &[PathBuf]) {
    // Warm the file cache so we time decoding, not the disk.
    for f in files {
        let _ = std::fs::read(f);
    }
    let mut times = Vec::new();
    for f in files.iter().take(10) {
        match decode::decode_file(f) {
            Ok(frame) => {
                println!(
                    "{}: {} {} {}x{} cfa {:?} black {:?} white {} wb {:?} orient {:?} in {:.1} ms",
                    f.file_name().unwrap().to_string_lossy(),
                    frame.make,
                    frame.model,
                    frame.width,
                    frame.height,
                    frame.cfa,
                    frame.black,
                    frame.white,
                    frame.wb,
                    frame.orientation,
                    ms(frame.decode_time)
                );
                times.push(ms(frame.decode_time));
            }
            Err(e) => println!("{}: {e}", f.display()),
        }
    }
    if times.is_empty() {
        return;
    }
    println!("one at a time: median {:.1} ms", median(times));

    let start = Instant::now();
    let ok = files.par_iter().filter(|f| decode::decode_file(f).is_ok()).count();
    let total = start.elapsed();
    println!(
        "all cores ({} threads): {ok} files in {:.0} ms = {:.1} files/s",
        rayon::current_num_threads(),
        ms(total),
        ok as f64 / total.as_secs_f64()
    );
}

fn bench_preview(files: &[PathBuf]) {
    for f in files {
        let _ = std::fs::read(f);
    }
    let mut times = Vec::new();
    for f in files.iter().take(10) {
        match decode::decode_preview(f) {
            Ok(p) => {
                println!(
                    "{}: {}x{} in {:.1} ms",
                    f.file_name().unwrap().to_string_lossy(),
                    p.width,
                    p.height,
                    ms(p.time)
                );
                times.push(ms(p.time));
            }
            Err(e) => println!("{}: {e}", f.display()),
        }
    }
    if !times.is_empty() {
        println!("one at a time: median {:.1} ms", median(times));
    }
}

/// One view size the develop pass is timed at.
struct ViewCase {
    name: &'static str,
    width: u32,
    height: u32,
    zoom: fn(&Image) -> Zoom,
}

fn bench_gpu(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {} ({})", gpu.adapter_name, if gpu.uma { "shared memory" } else { "own memory" });
    println!("warm-up (prepares the GPU kernels): {:.1} ms", warm_up(&mut gpu).unwrap());
    let cases = [
        ViewCase { name: "fit 1920x1080", width: 1920, height: 1080, zoom: |_| Zoom::Fit },
        ViewCase { name: "fit 2560x1440", width: 2560, height: 1440, zoom: |_| Zoom::Fit },
        ViewCase { name: "fit 3840x2160", width: 3840, height: 2160, zoom: |_| Zoom::Fit },
        ViewCase {
            name: "100% 2560x1440",
            width: 2560,
            height: 1440,
            zoom: |img| {
                let (w, h) = img.oriented_size();
                Zoom::Actual { x: w as f32 / 2.0, y: h as f32 / 2.0 }
            },
        },
    ];
    let out_dir = std::env::current_exe().unwrap().parent().unwrap().join("renders");
    std::fs::create_dir_all(&out_dir).unwrap();

    for f in files.iter().take(3) {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let frame = match decode::decode_file(f) {
            Ok(frame) => frame,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        println!(
            "{name} ({} {}, {}x{}): decode {:.1} ms",
            frame.make,
            frame.model,
            frame.width,
            frame.height,
            ms(frame.decode_time)
        );
        // New memory for each file, so the first open shows what a session's first photo
        // (or first photo of a new size) costs; the second reuses it, like every photo after.
        let mut memory = ImageMemory::new();
        let (_, first) = Image::open(&mut gpu, &mut memory, &frame).unwrap();
        print_open("first open (new memory)", &first);
        let (image, t) = Image::open(&mut gpu, &mut memory, &frame).unwrap();
        print_open("next open (reused)     ", &t);

        for case in &cases {
            let target = Target::new(&gpu, case.width, case.height).unwrap();
            let zoom = (case.zoom)(&image);
            let (mut gpu_ms, mut wall_ms) = (Vec::new(), Vec::new());
            for i in 0..60 {
                // Every frame is a different exposure, as when dragging the slider.
                let edit = Edit { exposure: (i as f32 / 60.0) * 2.0 - 1.0 };
                let start = Instant::now();
                gpu.begin().unwrap();
                let a = gpu.timestamp();
                render(&gpu, &image, &edit, zoom, &target);
                let b = gpu.timestamp();
                let ts = gpu.submit().unwrap();
                wall_ms.push(ms(start.elapsed()));
                gpu_ms.push(ts[b as usize] - ts[a as usize]);
            }
            println!(
                "  develop {:<15} GPU {:.3} ms, slider-to-done {:.2} ms (median of 60)",
                case.name,
                median(gpu_ms),
                median(wall_ms)
            );
            if case.name == "fit 2560x1440" {
                render_png(&mut gpu, &image, &target, zoom, &out_dir.join(format!("{name}.png")));
            }
        }

        // Export size: every sensor pixel developed once.
        let (w, h) = image.oriented_size();
        let target = Target::new(&gpu, w, h).unwrap();
        gpu.begin().unwrap();
        let a = gpu.timestamp();
        render(&gpu, &image, &Edit::default(), Zoom::Fit, &target);
        let b = gpu.timestamp();
        let ts = gpu.submit().unwrap();
        println!("  develop full size {w}x{h}: GPU {:.2} ms", ts[b as usize] - ts[a as usize]);
    }
    println!("renders in {}", out_dir.display());
}

fn render_png(gpu: &mut Gpu, image: &Image, target: &Target, zoom: Zoom, path: &Path) {
    gpu.begin().unwrap();
    render(gpu, image, &Edit::default(), zoom, target);
    gpu.submit().unwrap();
    let pixels = read_target(gpu, target).unwrap();
    image::save_buffer(path, &pixels, target.width, target.height, image::ExtendedColorType::Rgba8).unwrap();
}

fn print_open(label: &str, t: &OpenTimings) {
    println!(
        "  {label} {:.1} ms = alloc {:.1} + upload {:.1} + copy {:.2} + demosaic {:.2} + pyramid {:.2} (GPU)",
        t.total_ms, t.alloc_ms, t.upload_ms, t.copy_ms, t.demosaic_ms, t.pyramid_ms
    );
}
