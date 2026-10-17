//! Finds the photo inside each screenshot, crops it and shrinks it to a small PNG.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use image::RgbImage;
use windedit_core::decode;

use crate::linear::oriented_size;
use crate::util::*;

/// Where the photo can be, horizontally: after Lightroom's left panel, before the right panels.
const LIGHTROOM_X: (u32, u32) = (470, 3200);
const WINDEDIT_X: (u32, u32) = (0, 3200);
/// And vertically: between the toolbars above and below the photo.
const LIGHTROOM_Y: (u32, u32) = (88, 1776);
const WINDEDIT_Y: (u32, u32) = (100, 1939);
/// How many background pixels in a row end the photo (a little fewer for Lightroom, whose
/// canvas is thin below the photo).
const RUN_LIGHTROOM: u32 = 6;
const RUN_WINDEDIT: u32 = 8;
/// Pixels shaved off each side of the rectangle, in case the edge is anti-aliased.
const INSET: u32 = 2;
const MAX_ASPECT_ERROR: f64 = 0.03;

pub struct Row {
    pub folder: String,
    pub file: String,
    pub app: String,
    pub raw: String,
    pub slider: String,
    pub value: String,
}

pub fn read_manifest(path: &Path) -> Result<Vec<Row>> {
    let text = fs::read_to_string(path)?;
    text.lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.trim().split(',').collect();
            if f.len() != 6 {
                return Err(format!("bad manifest row: {l}").into());
            }
            Ok(Row { folder: f[0].into(), file: f[1].into(), app: f[2].into(), raw: f[3].into(), slider: f[4].into(), value: f[5].into() })
        })
        .collect()
}

/// Whether a pixel is within `tolerance` of the background on every channel.
fn is_bg(img: &RgbImage, x: u32, y: u32, bg: [u8; 3], tolerance: i32) -> bool {
    let p = img.get_pixel(x, y).0;
    (0..3).all(|c| (p[c] as i32 - bg[c] as i32).abs() <= tolerance)
}

/// Walks from `start` toward `limit` (inclusive) along a line and returns the last photo pixel
/// before a run of `run_len` background pixels, or `limit` if the photo reaches it.
fn find_edge(start: u32, limit: u32, run_len: u32, is_background: impl Fn(u32) -> bool) -> u32 {
    let step: i64 = if limit >= start { 1 } else { -1 };
    let mut run = 0;
    let mut pos = start as i64;
    while pos != limit as i64 + step {
        if is_background(pos as u32) {
            run += 1;
            if run == run_len {
                return (pos - step * run_len as i64) as u32;
            }
        } else {
            run = 0;
        }
        pos += step;
    }
    limit
}

struct Found {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    bg: [u8; 3],
}

/// The screenshots are 3768 pixels wide; a few pasted ones are smaller, and lossy (WebP), so the
/// layout limits scale and the colour matches loosen.
const FULL_WIDTH: u32 = 3768;

fn find_photo(img: &RgbImage, app: &str) -> Result<Found> {
    let scale = img.width() as f64 / FULL_WIDTH as f64;
    let lossy = img.width() != FULL_WIDTH;
    let at = |v: u32| (v as f64 * scale).round() as u32;
    let (xa, xb) = if app == "windedit" { WINDEDIT_X } else { LIGHTROOM_X };
    let (xa, xb) = (at(xa), at(xb));
    let mid_y = img.height() / 2;
    // The background is measured just inside the viewing area, left of the photo.
    let bg = img.get_pixel(xa + 6, mid_y).0;
    let expected = if app == "windedit" { 28 } else { 129 };
    if bg.iter().any(|&v| (v as i32 - expected).abs() > if lossy { 6 } else { 2 }) {
        return Err(format!("background is {bg:?}, expected {expected}").into());
    }
    // Windedit paints its canvas exactly; Lightroom's greys are within a level or two.
    let tolerance = if app == "windedit" { 1 } else { 2 } + if lossy { 3 } else { 0 };
    let run = if app == "windedit" { RUN_WINDEDIT } else { RUN_LIGHTROOM };
    let row_bg = |x| is_bg(img, x, mid_y, bg, tolerance);
    let mid_x = (xa + xb) / 2;
    if row_bg(mid_x) {
        return Err("the middle of the viewing area is background".into());
    }
    let mut x0 = find_edge(mid_x, xa, run, row_bg);
    let mut x1 = find_edge(mid_x, xb, run, row_bg);
    let col = (x0 + x1) / 2;
    let col_bg = |y| is_bg(img, col, y, bg, tolerance);
    // Windedit's toolbar above the canvas and bar below it are a few levels off the canvas
    // colour, and Lightroom's toolbars sit just beyond its canvas.
    let (ya, yb) = if app == "windedit" { WINDEDIT_Y } else { LIGHTROOM_Y };
    let (ya, yb) = (at(ya), at(yb));
    let mut y0 = find_edge(mid_y, ya, run, col_bg);
    let mut y1 = find_edge(mid_y, yb, run, col_bg);
    if app != "windedit" {
        // Library draws a soft shadow around the photo: neutral greys fading into the canvas.
        let shadow = |x: u32, y: u32| {
            let p = img.get_pixel(x, y).0;
            p[0] == p[1] && p[1] == p[2] && (100..=bg[0]).contains(&p[0])
        };
        while shadow(x0, mid_y) { x0 += 1 }
        while shadow(x1, mid_y) { x1 -= 1 }
        while shadow(col, y0) { y0 += 1 }
        while shadow(col, y1) { y1 -= 1 }
    }
    Ok(Found { x0, y0, x1, y1, bg })
}

/// One screenshot's measurements.
struct Shot {
    name: String,
    group: String,
    pub raw: String,
    found: Found,
}

pub fn run(manifest: &Path, outdir: &Path) -> Result<()> {
    let rows = read_manifest(manifest)?;
    let base = manifest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(outdir)?;
    let lut: Vec<f32> = (0..256).map(|v| srgb_to_linear(v as f32 / 255.0)).collect();
    let path_of = |row: &Row| base.join(&row.folder).join(&row.file);

    // First pass: find the photo in every screenshot.
    let mut shots = Vec::new();
    for row in &rows {
        let img = open_rgb(&path_of(row))?;
        let found = find_photo(&img, &row.app).map_err(|e| format!("{}/{}: {e}", row.folder, row.file))?;
        let name = format!("{}_{}_{}_{}", row.folder, row.app, row.slider, row.value);
        shots.push(Shot { name, group: format!("{}_{}_{}", row.folder, row.app, img.width()), raw: row.raw.clone(), found });
    }

    // Photos that are very dark or flat can look like canvas and shrink the rectangle, but it
    // never grows past the photo, and the photo sits in the same place in a whole series (same
    // folder, app and screenshot size). So
    // every shot gets the largest rectangle found in its series.
    let mut union: HashMap<String, (u32, u32, u32, u32)> = HashMap::new();
    for s in &shots {
        let f = &s.found;
        let u = union.entry(s.group.clone()).or_insert((f.x0, f.y0, f.x1, f.y1));
        *u = (u.0.min(f.x0), u.1.min(f.y0), u.2.max(f.x1), u.3.max(f.y1));
    }

    // The raw's shape after orientation, per raw file (decoding is slow).
    let mut shapes: HashMap<String, f64> = HashMap::new();
    let mut flagged = 0;
    for (row, s) in rows.iter().zip(&shots) {
        let aspect = match shapes.get(&row.raw) {
            Some(a) => *a,
            None => {
                let frame = decode::decode_file(Path::new(&row.raw))?;
                let (w, h) = oriented_size(&frame);
                shapes.insert(row.raw.clone(), w as f64 / h as f64);
                w as f64 / h as f64
            }
        };
        let (x0, y0, x1, y1) = union[&s.group];
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        let error = (w as f64 / h as f64) / aspect - 1.0;
        let flag = if error.abs() > MAX_ASPECT_ERROR {
            flagged += 1;
            "  <-- FLAG"
        } else {
            ""
        };
        let own = &s.found;
        let note = if (own.x0, own.y0, own.x1, own.y1) == (x0, y0, x1, y1) {
            String::new()
        } else {
            format!("  (own x {}..{} y {}..{})", own.x0, own.x1, own.y0, own.y1)
        };
        println!(
            "{}: x {x0}..{x1} y {y0}..{y1} ({w}x{h}) bg {:?} aspect {:.4} vs {:.4} ({}), error {:+.2}%{flag}{note}",
            s.name, own.bg, w as f64 / h as f64, aspect, Path::new(&s.raw).file_name().and_then(|n| n.to_str()).unwrap_or(""), error * 100.0
        );

        // Crop, average in linear light, shrink so the long edge is 512, and encode again.
        let img = open_rgb(&path_of(row))?;
        let (cx, cy, cw, ch) = (x0 + INSET, y0 + INSET, w - 2 * INSET, h - 2 * INSET);
        let mut px = Vec::with_capacity((cw * ch) as usize);
        for y in cy..cy + ch {
            for x in cx..cx + cw {
                let p = img.get_pixel(x, y).0;
                px.push([lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]]);
            }
        }
        let (dw, dh) = fit_size(cw as usize, ch as usize, 512);
        let small = box_downscale(&px, cw as usize, ch as usize, dw, dh);
        let bytes: Vec<u8> = small.iter().flatten().map(|&v| (linear_to_srgb(v) * 255.0).round() as u8).collect();
        RgbImage::from_raw(dw as u32, dh as u32, bytes).ok_or("size mismatch")?.save(outdir.join(format!("{}.png", s.name)))?;
    }
    println!("{} images, {flagged} flagged", rows.len());
    Ok(())
}
