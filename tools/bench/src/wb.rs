//! `bench wb <raw>...`: checks the white balance picker and the manual vignetting on real files.
//!
//! 1. Picks at a neutral-looking spot and a few others (Fit), and shows the result against the
//!    as-shot white balance. Applying the pick makes the spot neutral in the GPU render, and
//!    picking the same spot again with that white balance on gives the same answer.
//! 2. Picks through a rotated, straightened, cropped view, at Fit and at 100%: the photo spot the
//!    click maps to (`pick::sensor_point`) is compared with the GPU render of the same view, whose
//!    pixel there must show what the plain frame shows at that spot (against a shifted control).
//! 3. Vignetting at −100, 0 and +100: the corner-to-centre brightness of the render, and of a
//!    full-size export (which must show it too).

use std::path::PathBuf;

use windedit_core::decode::{self, Orientation, RawFrame};
use windedit_core::develop;
use windedit_core::export::{self, TileRenderer};
use windedit_core::geometry::{self, Constrain};
use windedit_core::gpu::Gpu;
use windedit_core::pick;
use windedit_core::pipeline::{Edit, Image, ImageMemory, Layer, Target, Zoom, read_target, render, warm_up};

/// A photo open on the GPU, and ways to draw it.
struct Rig {
    gpu: Gpu,
    memory: ImageMemory,
    image: Option<Image>,
    size: (u32, u32),
}

impl Rig {
    fn open(&mut self, frame: &RawFrame) {
        self.image = None; // its memory may be reused by the next
        let (image, _) = Image::open(&mut self.gpu, &mut self.memory, frame).unwrap();
        self.size = image.oriented_size();
        self.image = Some(image);
    }

    fn draw(&mut self, edit: &Edit, zoom: Zoom, w: u32, h: u32) -> Vec<u8> {
        let target = Target::new(&self.gpu, w, h).unwrap();
        self.gpu.begin().unwrap();
        render(&self.gpu, Layer::Raw(self.image.as_ref().unwrap()), edit, zoom, self.size, &target);
        self.gpu.submit().unwrap();
        read_target(&mut self.gpu, &target).unwrap()
    }

    /// The whole photo as an export renders it: width, height and BGR bytes.
    fn export(&mut self, edit: &Edit) -> (u32, u32, Vec<u8>) {
        let window = export::window(self.size, edit);
        let mut renderer = TileRenderer::new(&self.gpu, window, edit).unwrap();
        let mut bgr = Vec::new();
        while !renderer.done() {
            if let Some(strip) = renderer.step(&mut self.gpu, self.image.as_ref().unwrap(), edit).unwrap() {
                bgr.extend_from_slice(&strip.bgr[..strip.rows as usize * window.width as usize * 3]);
            }
        }
        (window.width, window.height, bgr)
    }
}

pub fn bench_wb(files: &[PathBuf]) {
    let mut gpu = Gpu::new().expect("no Direct3D 12 GPU");
    println!("GPU: {}", gpu.adapter_name);
    warm_up(&mut gpu).unwrap();
    let mut rig = Rig { gpu, memory: ImageMemory::new(), image: None, size: (0, 0) };
    let mut failed = false;
    for f in files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let frame = decode::decode_file(f).unwrap();
        rig.open(&frame);
        println!("\n{name} ({} {}): {}x{}, orientation {:?}, lens {}", frame.make, frame.model, rig.size.0, rig.size.1, frame.orientation, frame.lens.is_some());
        failed |= !picks(&frame, &mut rig);
        failed |= !mapping(&frame, &mut rig);
        failed |= !vignette(&mut rig);
    }
    if failed {
        println!("\nFAILED");
        std::process::exit(1);
    }
}

fn report(what: &str, detail: String, ok: bool) -> bool {
    println!("  {:<4} {what}: {detail}", if ok { "ok" } else { "FAIL" });
    ok
}

fn rgb_at(px: &[u8], width: u32, x: i64, y: i64, half: i64) -> [f64; 3] {
    let (mut sum, mut n) = ([0.0f64; 3], 0.0f64);
    for j in (y - half).max(0)..=(y + half) {
        for i in (x - half).max(0)..=(x + half).min(width as i64 - 1) {
            let at = (j as usize * width as usize + i as usize) * 4;
            if at + 2 >= px.len() {
                continue;
            }
            for c in 0..3 {
                sum[c] += px[at + c] as f64;
            }
            n += 1.0;
        }
    }
    sum.map(|s| s / n.max(1.0))
}

// ---- 1. Picking ----

fn picks(frame: &RawFrame, rig: &mut Rig) -> bool {
    let (w, h) = (1600u32, 1000u32);
    let mut ok = true;
    let as_shot = develop::multipliers_to_temperature(&frame.xyz_to_cam, frame.wb);
    println!("  as shot: {:.0} K, tint {:+.1}", as_shot.0, as_shot.1);
    let plain = Edit::default();
    // The most neutral-looking spot in the middle tones, by the as-shot white balance.
    let mut best: Option<(f64, (f32, f32))> = None;
    for gy in 1..20 {
        for gx in 1..32 {
            let view = (gx as f32 * w as f32 / 32.0, gy as f32 * h as f32 / 20.0);
            let Some(s) = pick::sensor_point(frame, &plain, Zoom::Fit, (w, h), view) else { continue };
            let Ok(cam) = pick::average(frame, s.sensor, pick::window_side(s.per_pixel)) else { continue };
            let balanced = [cam[0] * frame.wb[0], cam[1] * frame.wb[1], cam[2] * frame.wb[2]];
            let chroma = balanced.iter().cloned().fold(0.0, f32::max) / balanced.iter().cloned().fold(f32::MAX, f32::min) - 1.0;
            // Even ground, too: the window next to it (each way) reads the same.
            let side = pick::window_side(s.per_pixel);
            let near = [[side as f64, 0.0], [-(side as f64), 0.0], [0.0, side as f64], [0.0, -(side as f64)]]
                .map(|d| pick::average(frame, [s.sensor[0] + d[0], s.sensor[1] + d[1]], side).map_or(1.0, |c| (c[1] / cam[1] - 1.0).abs()));
            let texture = near.iter().cloned().fold(0.0, f32::max);
            if texture > 0.10 {
                continue;
            }
            if !(0.05..0.5).contains(&balanced[1]) {
                continue;
            }
            if best.is_none_or(|(c, _)| (chroma as f64) < c) {
                best = Some((chroma as f64, view));
            }
        }
    }
    let Some((chroma, neutral)) = best else {
        println!("  no usable spot found");
        return false;
    };
    println!("  most neutral spot: view {:.0},{:.0} (colour spread {:.1}% under the as-shot white balance)", neutral.0, neutral.1, chroma * 100.0);
    let spots = [neutral, (w as f32 * 0.25, h as f32 * 0.3), (w as f32 * 0.5, h as f32 * 0.5), (w as f32 * 0.75, h as f32 * 0.7), (w as f32 * 0.5, h as f32 * 0.1)];
    for (n, view) in spots.into_iter().enumerate() {
        let s = pick::sensor_point(frame, &plain, Zoom::Fit, (w, h), view);
        let Some(s) = s else { continue };
        match pick::pick(frame, &plain, Zoom::Fit, (w, h), view) {
            Err(e) => println!("  spot {n} at {:.0},{:.0}: refused: {e}", view.0, view.1),
            Ok((k, t)) => {
                if std::env::var_os("WB_SWEEP").is_some() {
                    window_sweep(frame, s.sensor);
                }
                println!(
                    "  spot {n} at {:.0},{:.0} (sensor {:.0},{:.0}, window {} px): {k:.0} K, tint {t:+.1}",
                    view.0, view.1, s.sensor[0], s.sensor[1], pick::window_side(s.per_pixel)
                );
                // Applying it, then picking again with it on.
                let wb = Edit { custom_white_balance: 1, temperature: k, tint: t, ..plain };
                let (k2, t2) = pick::pick(frame, &wb, Zoom::Fit, (w, h), view).unwrap();
                let same = (k2 - k).abs() / k < 0.01 && (t2 - t).abs() <= 0.01 * t.abs().max(1.0);
                // The render of that spot is neutral: red, green and blue agree.
                let shot = rig.draw(&plain, Zoom::Fit, w, h);
                let after = rig.draw(&wb, Zoom::Fit, w, h);
                let half = (pick::window_side(s.per_pixel) as f64 / s.per_pixel / 2.0).round().max(1.0) as i64;
                let spread = |px: &[u8]| {
                    let c = rgb_at(px, w, view.0 as i64, view.1 as i64, half);
                    let hi = c.iter().cloned().fold(0.0, f64::max);
                    let lo = c.iter().cloned().fold(f64::MAX, f64::min);
                    (hi - lo, c)
                };
                let (before, _) = spread(&shot);
                let (now, c) = spread(&after);
                // Neutrality only holds where the ground is even: on texture the average of the render
                // and the average of the raw differ (the window sizes differ, and tone is not linear).
                let side = pick::window_side(s.per_pixel);
                let uneven = [[side as f64, 0.0], [-(side as f64), 0.0], [0.0, side as f64], [0.0, -(side as f64)]]
                    .iter()
                    .map(|d| pick::average(frame, [s.sensor[0] + d[0], s.sensor[1] + d[1]], side).map_or(1.0, |c| (c[1] / pick::average(frame, s.sensor, side).unwrap()[1] - 1.0).abs()))
                    .fold(0.0, f32::max)
                    > 0.10;
                ok &= report(
                    &format!("spot {n} re-pick and neutrality"),
                    format!(
                        "again {k2:.0} K, {t2:+.1}; render R/G/B {:.1}/{:.1}/{:.1}, spread {now:.1} levels (as shot {before:.1}){}",
                        c[0], c[1], c[2],
                        if uneven { " [uneven spot: neutrality not expected]" } else { "" }
                    ),
                    same && (uneven || now <= before.max(2.0) + 0.5),
                );
            }
        }
    }
    ok
}

// ---- 2. Mapping through the view ----

fn mapping(frame: &RawFrame, rig: &mut Rig) -> bool {
    let (ow, oh) = rig.size;
    // The plain frame at 1/k, with the same tone edit (the tone pre-pass doesn't depend on the geometry).
    let k = [4u32, 2, 1].into_iter().find(|k| ow % k == 0 && oh % k == 0).unwrap();
    let (a, b) = (ow / k, oh / k);
    let tone = Edit { exposure: 0.2, contrast: 10.0, ..Default::default() };
    let reference = rig.draw(&tone, Zoom::Region { x: 0.0, y: 0.0, scale: 1.0 / k as f32 }, a, b);
    let bilinear = |u: f64, v: f64| -> [f64; 3] {
        let (x, y) = ((u * a as f64 - 0.5).clamp(0.0, a as f64 - 1.0), (v * b as f64 - 0.5).clamp(0.0, b as f64 - 1.0));
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(a as usize - 1), (y0 + 1).min(b as usize - 1));
        let (fx, fy) = (x - x0 as f64, y - y0 as f64);
        let at = |i: usize, j: usize, c: usize| reference[(j * a as usize + i) * 4 + c] as f64;
        [0, 1, 2].map(|c| {
            (at(x0, y0, c) * (1.0 - fx) + at(x1, y0, c) * fx) * (1.0 - fy) + (at(x0, y1, c) * (1.0 - fx) + at(x1, y1, c) * fx) * fy
        })
    };
    let mut ok = true;
    let frame_of = |rotation: u32| geometry::frame_size((ow, oh), rotation);
    // A crop 70% the size of the largest that fits at this angle, about the middle.
    let crop_for = |rotation: u32, angle: f32, share: f64| {
        let f = frame_of(rotation);
        let fsz = (f.0 as f64, f.1 as f64);
        let whole = geometry::constrain_crop(fsz, angle as f64, [0.0, 0.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0], Constrain::Shrink);
        let (mx, my) = ((whole[0] + whole[2]) / 2.0, (whole[1] + whole[3]) / 2.0);
        let (hw, hh) = ((whole[2] - whole[0]) * share / 2.0, (whole[3] - whole[1]) * share / 2.0);
        [mx - hw, my - hh, mx + hw, my + hh].map(|v| v as f32)
    };
    let cases: [(&str, Edit, Zoom, (u32, u32)); 4] = [
        ("quarter turn + crop, Fit", Edit { rotation: 1, crop: [0.15, 0.2, 0.85, 0.8], ..tone }, Zoom::Fit, (1200, 800)),
        ("straighten 4° + crop, Fit", Edit { angle: 4.0, crop: crop_for(0, 4.0, 0.7), ..tone }, Zoom::Fit, (1200, 800)),
        ("straighten 4° + crop, 100%", Edit { angle: 4.0, crop: crop_for(0, 4.0, 0.7), ..tone }, Zoom::Actual { x: 0.5, y: 0.5 }, (900, 600)),
        ("3 turns, -3° + crop, 100% off centre", Edit { rotation: 3, angle: -3.0, crop: crop_for(3, -3.0, 0.7), ..tone }, Zoom::Actual { x: 0.3, y: 0.6 }, (900, 600)),
    ];
    for (label, edit, zoom, (w, h)) in cases {
        let view = rig.draw(&edit, zoom, w, h);
        let (mut matched, mut control, mut n) = (0.0, 0.0, 0u32);
        let mut shown = Vec::new();
        for gy in 1..15 {
            for gx in 1..20 {
                let p = (gx as f32 * w as f32 / 20.0, gy as f32 * h as f32 / 15.0);
                let Some(s) = pick::sensor_point(frame, &edit, zoom, (w, h), p) else { continue };
                // Averaging as much of the view as the reference's coarser pixel covers.
                let half = ((k as f64 / s.per_pixel / 2.0).round() as i64).max(1);
                let got = rgb_at(&view, w, p.0 as i64, p.1 as i64, half);
                let (fu, fv) = frame_uv(frame.orientation, s.uv);
                let want = bilinear(fu, fv);
                let off = bilinear(fu + 14.0 / a as f64, fv + 9.0 / b as f64);
                matched += (0..3).map(|c| (got[c] - want[c]).abs()).sum::<f64>() / 3.0;
                control += (0..3).map(|c| (got[c] - off[c]).abs()).sum::<f64>() / 3.0;
                n += 1;
                if shown.len() < 2 && gx % 7 == 3 && gy % 5 == 3 {
                    shown.push(format!("view {:.0},{:.0} → frame {:.4},{:.4} → sensor {:.1},{:.1}", p.0, p.1, s.uv[0], s.uv[1], s.sensor[0], s.sensor[1]));
                }
            }
        }
        let (matched, control) = (matched / n as f64, control / n as f64);
        ok &= report(
            label,
            format!("{n} points: mean difference {matched:.2} levels at the mapped spot, {control:.2} at a spot 14,9 reference pixels away ({})", shown.join("; ")),
            n > 100 && matched < control * 0.3,
        );
    }
    ok
}

// ---- 3. Vignetting ----

/// Mean linear brightness of the sRGB RGBA bytes in a box.
fn linear_box(px: &[u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> f64 {
    let decode = |v: u8| {
        let c = v as f64 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let at = (y as usize * width as usize + x as usize) * 4;
            sum += 0.2126 * decode(px[at]) + 0.7152 * decode(px[at + 1]) + 0.0722 * decode(px[at + 2]);
            n += 1.0;
        }
    }
    sum / n
}

/// Corner brightness over centre brightness (linear display light): the four corners' boxes at 2%
/// in from the edges, 8% of the width across.
fn corner_ratio(px: &[u8], w: u32, h: u32) -> f64 {
    let (bw, bh) = (w * 8 / 100, h * 8 / 100);
    let (mx, my) = (w * 2 / 100, h * 2 / 100);
    let centre = linear_box(px, w, w / 2 - bw / 2, h / 2 - bh / 2, w / 2 + bw / 2, h / 2 + bh / 2);
    let corners = [
        linear_box(px, w, mx, my, mx + bw, my + bh),
        linear_box(px, w, w - mx - bw, my, w - mx, my + bh),
        linear_box(px, w, mx, h - my - bh, mx + bw, h - my),
        linear_box(px, w, w - mx - bw, h - my - bh, w - mx, h - my),
    ];
    corners.iter().sum::<f64>() / 4.0 / centre
}

fn vignette(rig: &mut Rig) -> bool {
    let (ow, oh) = rig.size;
    let mut ok = true;
    let (w, h) = (1200u32, (1200.0 * oh as f64 / ow as f64).round() as u32);
    let mut ratios = Vec::new();
    for amount in [-100.0, 0.0, 100.0] {
        let px = rig.draw(&Edit { vignette_amount: amount, ..Default::default() }, Zoom::Fit, w, h);
        let r = corner_ratio(&px, w, h);
        println!("  vignette {amount:+4.0}: corner/centre {r:.3} ({:+.2} stops of display light)", r.log2());
        ratios.push(r);
    }
    ok &= report(
        "vignette direction",
        format!("{:.3} < {:.3} < {:.3}", ratios[0], ratios[1], ratios[2]),
        ratios[0] < ratios[1] && ratios[1] < ratios[2],
    );
    // At the very corner the gain is one stop: the same as Exposure +1 there.
    let corner = |px: &[u8]| linear_box(px, w, w * 98 / 100, h * 98 / 100, w - 1, h - 1);
    let base = corner(&rig.draw(&Edit::default(), Zoom::Fit, w, h));
    let by_vignette = corner(&rig.draw(&Edit { vignette_amount: 100.0, ..Default::default() }, Zoom::Fit, w, h)) / base;
    let by_exposure = corner(&rig.draw(&Edit { exposure: 1.0, ..Default::default() }, Zoom::Fit, w, h)) / base;
    ok &= report(
        "gain at the corner",
        format!("+100 brightens the last 2% by x{by_vignette:.3}, Exposure +1 by x{by_exposure:.3}"),
        (by_vignette / by_exposure - 1.0).abs() < 0.06,
    );
    // Midpoint: a lower midpoint reaches further in, so more of the frame changes.
    let mut mid = |offset: f32| {
        let px = rig.draw(&Edit { vignette_amount: 100.0, vignette_midpoint: offset, ..Default::default() }, Zoom::Fit, w, h);
        // The ring halfway to the corner, on the long axis.
        linear_box(&px, w, w * 3 / 4 - 6, h / 2 - 6, w * 3 / 4 + 6, h / 2 + 6)
    };
    let (low, high) = (mid(-40.0), mid(40.0));
    ok &= report("midpoint", format!("brightness at 3/4 of the width: midpoint 10 → {low:.4}, midpoint 90 → {high:.4}"), low > high);
    // Cropping doesn't move it: a crop of the corner region shows the same corner as the full frame.
    let full = rig.draw(&Edit { vignette_amount: 100.0, ..Default::default() }, Zoom::Region { x: 0.0, y: 0.0, scale: w as f32 / ow as f32 }, w, h);
    let cropped = Edit { vignette_amount: 100.0, crop: [0.5, 0.5, 1.0, 1.0], ..Default::default() };
    let (cw, ch) = (w / 2, h / 2);
    let quarter = rig.draw(&cropped, Zoom::Fit, cw, ch);
    let same = {
        let (a, b) = (linear_box(&full, w, w * 9 / 10, h * 9 / 10, w * 98 / 100, h * 98 / 100), linear_box(&quarter, cw, cw * 8 / 10, ch * 8 / 10, cw * 96 / 100, ch * 96 / 100));
        (a / b - 1.0).abs()
    };
    ok &= report("vignette follows the frame, not the crop", format!("bottom-right corner region differs by {:.1}%", same * 100.0), same < 0.03);

    // An export carries it.
    let edit = Edit { vignette_amount: 100.0, ..Default::default() };
    let (ex_w, ex_h, bgr) = rig.export(&edit);
    // Down to a small RGBA picture by taking every n-th pixel.
    let n = (ex_w / w).max(1);
    let (sw, sh) = (ex_w / n, ex_h / n);
    let mut small = vec![255u8; (sw * sh * 4) as usize];
    for y in 0..sh {
        for x in 0..sw {
            let at = ((y * n) as usize * ex_w as usize + (x * n) as usize) * 3;
            let to = (y * sw + x) as usize * 4;
            small[to] = bgr[at + 2];
            small[to + 1] = bgr[at + 1];
            small[to + 2] = bgr[at];
        }
    }
    let exported = corner_ratio(&small, sw, sh);
    ok &= report("export", format!("{ex_w}x{ex_h} corner/centre {exported:.3}; the screen's at +100 is {:.3}", ratios[2]), (exported / ratios[2] - 1.0).abs() < 0.05);
    ok
}

/// Debugging aid kept out of the checks: how much the answer depends on the window size.
#[allow(dead_code)]
fn window_sweep(frame: &RawFrame, at: [f64; 2]) {
    for side in [5usize, 9, 12, 20, 40] {
        if let Ok(cam) = pick::average(frame, at, side) {
            let (k, t) = pick::neutral_for(cam, &frame.xyz_to_cam);
            println!("    window {side:>2}: cam {:.4} {:.4} {:.4} → {k:.0} K, {t:+.1}", cam[0], cam[1], cam[2]);
        }
    }
}

/// A texture position as the photo shows it after the camera's orientation (the inverse of the
/// quarter-turn step of `geometry::view_map`).
fn frame_uv(orientation: Orientation, uv: [f64; 2]) -> (f64, f64) {
    match geometry::turns(orientation, 0) {
        0 => (uv[0], uv[1]),
        1 => (1.0 - uv[1], uv[0]),
        2 => (1.0 - uv[0], 1.0 - uv[1]),
        _ => (uv[1], 1.0 - uv[0]),
    }
}
