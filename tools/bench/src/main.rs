//! Phase 0 benchmark: how fast can we decode, preview and develop raw files on this machine?
//!
//!     bench decode <file or folder>...   decode times, one at a time and on all cores
//!     bench preview <file or folder>...  embedded JPEG preview times
//!     bench cull <folder>               culling with the whole engine: stepping, holding a key,
//!                                        100% checks and a cold start (see bench_cull)
//!     bench gpu <file or folder>...      open and develop times on the GPU; writes a PNG of
//!                                        each photo's fitted view next to the benchmark

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use windedit_core::{decode, library, preview};
use windedit_core::gpu::Gpu;
use windedit_core::pipeline::{Edit, Image, ImageMemory, OpenTimings, Target, Zoom, read_target, render_image, warm_up};

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
        "jpegs" => list_jpegs(&files),
        "jpegtime" => jpeg_time(&files),
        "previewpng" => preview_png(&files),
        "snapshot" => snapshot(&files),
        "cull" => bench_cull(&rest.iter().map(PathBuf::from).collect::<Vec<_>>()),
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
    // Photos (RAW+JPEG pairs) for the files given; folders are scanned whole.
    let mut photos = Vec::new();
    for f in files {
        let dir = if f.is_dir() { f.clone() } else { f.parent().unwrap().to_path_buf() };
        let stem = f.file_stem().unwrap().to_string_lossy().to_lowercase();
        for p in library::scan_folder(&dir).unwrap() {
            if p.raw.is_some() && (f.is_dir() || p.name.to_lowercase() == stem) && !photos.iter().any(|q: &library::Photo| q.raw == p.raw) {
                photos.push(p);
            }
        }
    }
    for (label, want) in [
        ("fast", preview::Want::Fast),
        ("loupe 2560 px", preview::Want::Cover(2560)),
        ("largest (100%)", preview::Want::Largest),
    ] {
        let mut times = Vec::new();
        for p in photos.iter().take(10) {
            let _ = preview::load(p, want); // warm the file cache: this times decoding, not the disk
            match preview::load(p, want) {
                Ok(v) => {
                    println!(
                        "{}: {:?} {}x{} decoded at {}x{} in {:.1} ms",
                        p.name,
                        v.source,
                        v.source_width,
                        v.source_height,
                        v.pixels.width,
                        v.pixels.height,
                        ms(v.time)
                    );
                    times.push(ms(v.time));
                }
                Err(e) => println!("{}: {e}", p.name),
            }
        }
        if !times.is_empty() {
            println!("{label}: median {:.1} ms
", median(times));
        }
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
            zoom: |_| Zoom::Actual { x: 0.5, y: 0.5 },
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
                render_image(&gpu, &image, &edit, zoom, &target);
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
        render_image(&gpu, &image, &Edit::default(), Zoom::Fit, &target);
        let b = gpu.timestamp();
        let ts = gpu.submit().unwrap();
        println!("  develop full size {w}x{h}: GPU {:.2} ms", ts[b as usize] - ts[a as usize]);
    }
    println!("renders in {}", out_dir.display());
}

fn render_png(gpu: &mut Gpu, image: &Image, target: &Target, zoom: Zoom, path: &Path) {
    gpu.begin().unwrap();
    render_image(gpu, image, &Edit::default(), zoom, target);
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

/// Every JPEG embedded in each file: where it starts and its size (a diagnostic).
fn list_jpegs(files: &[PathBuf]) {
    for f in files.iter().take(3) {
        let bytes = std::fs::read(f).unwrap();
        println!("{} ({} bytes)", f.display(), bytes.len());
        for j in decode::embedded_jpegs(&bytes) {
            let end = decode::jpeg_end(&bytes[j.offset..], 0);
            let ok = end.map(|e| windedit_core::jpeg::decode(&bytes[j.offset..j.offset + e], 0).is_ok());
            println!("  at {:>10}: {}x{}, {} bytes long, decodes: {:?}", j.offset, j.width, j.height, end.unwrap_or(0), ok);
        }
    }
}

/// Time WIC decoding of whole JPEG files at full, 1/2 and 1/4 size (a diagnostic).
fn jpeg_time(files: &[PathBuf]) {
    for f in files {
        let bytes = std::fs::read(f).unwrap();
        for edge in [0, 3000, 1500] {
            let _ = windedit_core::jpeg::decode(&bytes, edge);
            let start = Instant::now();
            let p = windedit_core::jpeg::decode(&bytes, edge).unwrap();
            println!("{}: {}x{} in {:.1} ms", f.display(), p.width, p.height, ms(start.elapsed()));
        }
    }
}

/// Culling with the whole engine (decode workers, GPU thread, drawing to an offscreen
/// 2560×1440 view), timed from each request to show a photo until a frame of it is drawn.
fn bench_cull(files: &[PathBuf]) {
    use std::sync::mpsc::{Receiver, channel};
    use windedit_core::engine::Engine;
    use windedit_core::viewer::FrameEvent;

    let Some(dir) = files.first().filter(|f| f.is_dir()) else {
        eprintln!("usage: bench cull <folder>");
        return;
    };
    let start_engine = || -> (Engine, Receiver<FrameEvent>) {
        let (tx, rx) = channel();
        let engine = Engine::new(Box::new(move |e: &FrameEvent| {
            let _ = tx.send(*e);
        }))
        .expect("engine");
        (engine, rx)
    };
    // Frames drawn of `photo` until `deadline`.
    let collect = |rx: &Receiver<FrameEvent>, photo: usize, deadline: Instant| -> Vec<FrameEvent> {
        let mut frames = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(e) if e.photo == photo as i32 => frames.push(e),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        frames
    };
    let first_with = |frames: &[FrameEvent], min_layer: u32| frames.iter().find(|f| f.layer >= min_layer).map(|f| f.since_show_ms);
    let layer_name = |l: u32| ["none", "quick", "loupe", "full", "raw"][l as usize];
    let summary = |label: &str, v: &mut Vec<f64>, of: usize| {
        if v.is_empty() {
            println!("  {label}: never (0 of {of})");
            return;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        let p95 = v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)];
        println!("  {label}: median {:.1} ms, 95% within {:.1} ms ({} of {of})", v[v.len() / 2], p95, v.len());
    };

    // 1. A cold start: the first photo of the session, straight into Edit mode, so it waits
    // for the raw decode and the GPU memory, as opening a single raw from Explorer would.
    {
        let (engine, rx) = start_engine();
        println!("GPU: {} · warm-up {:.1} ms · {} decode threads", engine.info.adapter_name, engine.info.warm_up_ms, engine.jobs().worker_count());
        let count = engine.open_folder(dir).unwrap();
        println!("{} photos in {}", count, dir.display());
        engine.use_offscreen_view(2560, 1440);
        engine.set_edit_mode(true);
        engine.show(0);
        let frames = collect(&rx, 0, Instant::now() + Duration::from_secs(5));
        println!("\ncold start, first photo in Edit mode:");
        for f in &frames {
            println!("  {} at {:.1} ms{}", layer_name(f.layer), f.since_show_ms, if f.open_ms > 0.0 { format!(" (raw open {:.1} ms)", f.open_ms) } else { String::new() });
        }
    }

    let (engine, rx) = start_engine();
    let count = engine.open_folder(dir).unwrap();
    engine.use_offscreen_view(2560, 1440);
    let steps = count.min(40);

    // 2. Stepping at a steady pace, as when looking at each photo for a moment.
    for (label, pause_ms) in [("stepping, 2 photos/s", 500u64), ("stepping, 5 photos/s", 200)] {
        engine.show(0);
        engine.jobs().wait_idle(Duration::from_secs(10));
        let (mut first, mut loupe, mut raw_ready) = (Vec::new(), Vec::new(), 0);
        for i in 1..steps {
            engine.show(i);
            let frames = collect(&rx, i, Instant::now() + Duration::from_millis(pause_ms));
            if let Some(ms) = first_with(&frames, 1) {
                first.push(ms);
            }
            if let Some(ms) = first_with(&frames, 2) {
                loupe.push(ms);
            }
            raw_ready += engine.jobs().raw(i).is_some() as usize;
        }
        println!("\n{label} ({} steps):", steps - 1);
        summary("something on screen", &mut first, steps - 1);
        summary("loupe-quality preview", &mut loupe, steps - 1);
        println!("  raw decoded before moving on: {raw_ready} of {}", steps - 1);
    }

    // 3. Holding the arrow key: 30 photos a second.
    engine.show(0);
    engine.jobs().wait_idle(Duration::from_secs(10));
    let mut shown = Vec::new();
    for i in 1..steps {
        engine.show(i);
        if let Some(ms) = first_with(&collect(&rx, i, Instant::now() + Duration::from_millis(33)), 1) {
            shown.push(ms);
        }
    }
    println!("\nholding the arrow key, 30 photos/s ({} steps):", steps - 1);
    summary("shown before the next key repeat", &mut shown, steps - 1);

    // 4. Checking focus at 100% after a second on each photo (so it's had time to prefetch),
    // timed from the zoom: the first frame at 100%, and the first full-resolution one.
    let (mut any, mut sharp, mut sources) = (Vec::new(), Vec::new(), Vec::new());
    let checks = steps.min(10);
    for i in 0..checks {
        engine.set_zoom(Zoom::Fit);
        engine.show(i);
        collect(&rx, i, Instant::now() + Duration::from_millis(1000));
        let asked = Instant::now();
        engine.set_zoom(Zoom::Actual { x: 0.5, y: 0.5 });
        let deadline = asked + Duration::from_millis(1500);
        let mut first = None;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(e) = rx.recv_timeout(left) else { break };
            if e.photo != i as i32 {
                continue;
            }
            first.get_or_insert(ms(asked.elapsed()));
            if e.layer >= 3 {
                sharp.push(ms(asked.elapsed()));
                sources.push(layer_name(e.layer));
                break;
            }
        }
        any.extend(first);
    }
    engine.set_zoom(Zoom::Fit);
    println!("
100% focus check after a second on each photo ({checks} photos):");
    summary("first frame at 100%", &mut any, checks);
    summary("full resolution", &mut sharp, checks);
    println!("  full resolution from: {}", sources.join(", "));
}

/// Saves each photo's loupe preview, as decoded, to renders/<name>.preview.png (a diagnostic).
fn preview_png(files: &[PathBuf]) {
    let out_dir = std::env::current_exe().unwrap().parent().unwrap().join("renders");
    std::fs::create_dir_all(&out_dir).unwrap();
    for f in files {
        let dir = f.parent().unwrap();
        let stem = f.file_stem().unwrap().to_string_lossy().to_lowercase();
        let photo = library::scan_folder(dir).unwrap().into_iter().find(|p| p.name.to_lowercase() == stem).unwrap();
        let edge = std::env::var("EDGE").ok().and_then(|e| e.parse().ok()).unwrap_or(2560);
        let p = preview::load(&photo, preview::Want::Cover(edge)).unwrap();
        let mut rgba = p.pixels.bgra.clone();
        for px in rgba.chunks_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
        let path = out_dir.join(format!("{}.preview.png", photo.name));
        image::save_buffer(&path, &rgba, p.pixels.width, p.pixels.height, image::ExtendedColorType::Rgba8).unwrap();
        println!("{:?} {}x{} → {}", p.source, p.pixels.width, p.pixels.height, path.display());
        // And as the viewer draws it: through the GPU, fitted to a small view.
        let mut gpu = Gpu::new().unwrap();
        let tex = windedit_core::pipeline::PreviewTexture::upload(&mut gpu, None, &p.pixels, p.orientation).unwrap();
        let (w, h) = if p.orientation.swaps_axes() { (p.source_height, p.source_width) } else { (p.source_width, p.source_height) };
        let target = Target::new(&gpu, 760, 1013).unwrap();
        gpu.begin().unwrap();
        windedit_core::pipeline::render(&gpu, windedit_core::pipeline::Layer::Preview(&tex), &Edit::default(), Zoom::Fit, (w, h), &target);
        gpu.submit().unwrap();
        let pixels = read_target(&mut gpu, &target).unwrap();
        let gpu_path = out_dir.join(format!("{}.preview-gpu.png", photo.name));
        image::save_buffer(&gpu_path, &pixels, 760, 1013, image::ExtendedColorType::Rgba8).unwrap();
        println!("drawn → {}", gpu_path.display());
    }
}

/// Shows a photo with the whole engine in a 1576×1000 offscreen view, waits for decoding to
/// settle, and saves what was drawn to renders/<name>.snapshot.png (a diagnostic).
fn snapshot(files: &[PathBuf]) {
    use windedit_core::engine::Engine;
    let out_dir = std::env::current_exe().unwrap().parent().unwrap().join("renders");
    std::fs::create_dir_all(&out_dir).unwrap();
    let engine = Engine::new(Box::new(|e: &windedit_core::viewer::FrameEvent| {
        println!("  frame: photo {} layer {} at {:.1} ms", e.photo, e.layer, e.since_show_ms)
    }))
    .unwrap();
    for f in files {
        engine.open_folder(f.parent().unwrap()).unwrap();
        let stem = f.file_stem().unwrap().to_string_lossy().to_lowercase();
        let index = engine.photos().iter().position(|p| p.name.to_lowercase() == stem).unwrap();
        let size: Vec<u32> = std::env::var("VIEW").unwrap_or("1576x1000".into()).split('x').map(|n| n.parse().unwrap()).collect();
        if std::env::var_os("SHOWFIRST").is_some() {
            engine.show(index); // as the app does: before the view has a size
            std::thread::sleep(Duration::from_millis(300));
            engine.use_offscreen_view(size[0], size[1]);
            engine.show(index);
        } else {
            engine.use_offscreen_view(size[0], size[1]);
            engine.show(index);
        }
        engine.jobs().wait_idle(Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(200));
        let (w, h, px) = engine.snapshot().unwrap();
        let path = out_dir.join(format!("{}.snapshot.png", f.file_stem().unwrap().to_string_lossy()));
        image::save_buffer(&path, &px, w, h, image::ExtendedColorType::Rgba8).unwrap();
        println!("{}", path.display());
    }
}
