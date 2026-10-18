//! A diagnostic, not part of the engine: what a per-photo vignetting match would do. It compares
//! the raw's brightness with the camera's embedded JPEG along the radius and fits the gain the
//! camera applied, `g(r) = 1 + a·r² + b·r⁴`, `r` in half-diagonals from the sensor's centre,
//! accepting it only when the camera's corners are clearly brighter and the fit is trustworthy.
//! It was built, measured and left out of decode (see docs/lens.md).

use rayon::prelude::*;

use windedit_core::color;
use windedit_core::decode::{self, RawFrame};
use windedit_core::jpeg;
use windedit_core::lens::Channel;

/// The analysis grid's width; the height follows the photo's shape.
const GRID_W: usize = 384;
const RINGS: usize = 12;
const TONE_BINS: usize = 40;
/// The smallest corner gap, in stops, that's worth correcting.
const MIN_GAP: f32 = 0.05;
/// Most scatter, in stops, the fit may leave in the rings.
const MAX_SCATTER: f32 = 0.05;

/// What the comparison found.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Per ring of radius (0..1 in `RINGS` steps): how much brighter the camera's JPEG is than the
    /// raw, in stops (so positive means the raw needs lifting), and how many pixels it rests on.
    pub rings: Vec<(f32, usize)>,
    /// The fitted `[a, b]`, if it passed every check.
    pub gain: Option<[f32; 2]>,
    /// The fit's gain at the outer ring, in stops.
    pub outer_stops: f32,
    /// Root-mean-square distance of the rings from the fit, in stops.
    pub scatter: f32,
    pub why_not: Option<&'static str>,
}

/// The gain at radius `r`.
pub fn gain(g: [f32; 2], r: f32) -> f32 {
    let r2 = r * r;
    1.0 + g[0] * r2 + g[1] * r2 * r2
}

fn srgb_to_linear(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// A grid of average values, each flagged unusable when clipping or saturation touched it.
struct Grid {
    w: usize,
    h: usize,
    v: Vec<f32>,
    ok: Vec<bool>,
}

impl Grid {
    /// Bilinear value at grid position (x, y), where cell centres sit at whole numbers; `None`
    /// if outside or touching an unusable cell.
    fn at(&self, x: f32, y: f32) -> Option<f32> {
        if x < 0.0 || y < 0.0 || x > self.w as f32 - 1.001 || y > self.h as f32 - 1.001 {
            return None;
        }
        let (ix, iy) = (x as usize, y as usize);
        let (fx, fy) = (x - ix as f32, y - iy as f32);
        let i = iy * self.w + ix;
        if !(self.ok[i] && self.ok[i + 1] && self.ok[i + self.w] && self.ok[i + self.w + 1]) {
            return None;
        }
        let top = self.v[i] * (1.0 - fx) + self.v[i + 1] * fx;
        let bottom = self.v[i + self.w] * (1.0 - fx) + self.v[i + self.w + 1] * fx;
        Some(top * (1.0 - fy) + bottom * fy)
    }
}

/// The raw's luminance (white balanced, scene-linear) averaged into a grid, from 2×2 blocks.
fn raw_grid(frame: &RawFrame, gh: usize) -> Grid {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (qw, qh) = (w / 2, h / 2);
    // Camera RGB (white balanced) to luminance, as the develop pass does it.
    let to_srgb = color::mul(&color::REC2020_TO_SRGB, &frame.cam_to_working);
    let lw: [f32; 3] = [0, 1, 2].map(|c| 0.2126 * to_srgb[0][c] + 0.7152 * to_srgb[1][c] + 0.0722 * to_srgb[2][c]);
    let cfa = frame.cfa;
    let range = (frame.white - frame.black[0][0]).max(1.0);
    let rows: Vec<(Vec<f32>, Vec<bool>)> = (0..gh)
        .into_par_iter()
        .map(|gy| {
            let (q0, q1) = (gy * qh / gh, ((gy + 1) * qh / gh).max(gy * qh / gh + 1));
            let mut v = vec![0.0; GRID_W];
            let mut ok = vec![true; GRID_W];
            for gx in 0..GRID_W {
                let (p0, p1) = (gx * qw / GRID_W, ((gx + 1) * qw / GRID_W).max(gx * qw / GRID_W + 1));
                let (mut sum, mut n) = (0.0f32, 0.0f32);
                for qy in q0..q1.min(qh) {
                    for qx in p0..p1.min(qw) {
                        let mut c = [0.0f32; 3];
                        let mut cnt = [0.0f32; 3];
                        for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                            let k = cfa[dy][dx] as usize;
                            let raw = frame.data[(2 * qy + dy) * w + 2 * qx + dx] as f32;
                            c[k] += ((raw - frame.black[dy][dx]) / range).max(0.0);
                            cnt[k] += 1.0;
                        }
                        let cam = [0, 1, 2].map(|k| c[k] / cnt[k].max(1.0));
                        if cam.iter().any(|&x| x > 0.97) {
                            ok[gx] = false;
                        }
                        let cam = [cam[0] * frame.wb[0], cam[1] * frame.wb[1], cam[2] * frame.wb[2]];
                        sum += lw[0] * cam[0] + lw[1] * cam[1] + lw[2] * cam[2];
                        n += 1.0;
                    }
                }
                v[gx] = if n > 0.0 { sum / n } else { 0.0 };
            }
            (v, ok)
        })
        .collect();
    Grid {
        w: GRID_W,
        h: gh,
        v: rows.iter().flat_map(|r| r.0.iter().copied()).collect(),
        ok: rows.iter().flat_map(|r| r.1.iter().copied()).collect(),
    }
}

/// The camera's JPEG as linear luminance on the same grid. A cell is unusable if a tenth of its
/// pixels are clipped, nearly black or strongly colored (where luminance comparisons mislead).
fn jpeg_grid(bytes: &[u8], gh: usize, aspect: f32) -> Option<Grid> {
    let j = decode::largest_embedded_jpeg(bytes)?;
    let end = decode::jpeg_end(&bytes[j.offset..], 0)?;
    let px = jpeg::decode(&bytes[j.offset..j.offset + end], 640).ok()?;
    let (w, h) = (px.width as usize, px.height as usize);
    if ((w as f32 / h as f32) / aspect - 1.0).abs() > 0.01 {
        return None;
    }
    let mut v = vec![0.0f32; GRID_W * gh];
    let mut ok = vec![true; GRID_W * gh];
    for gy in 0..gh {
        let (y0, y1) = (gy * h / gh, ((gy + 1) * h / gh).max(gy * h / gh + 1));
        for gx in 0..GRID_W {
            let (x0, x1) = (gx * w / GRID_W, ((gx + 1) * w / GRID_W).max(gx * w / GRID_W + 1));
            let (mut sum, mut n, mut bad) = (0.0f32, 0.0f32, 0.0f32);
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    let p = &px.bgra[(y * w + x) * 4..];
                    let (b, g, r) = (p[0], p[1], p[2]);
                    let (hi, lo) = (r.max(g).max(b), r.min(g).min(b));
                    let lum = 0.2126 * srgb_to_linear(r) + 0.7152 * srgb_to_linear(g) + 0.0722 * srgb_to_linear(b);
                    if hi >= 250 || lum < 0.004 || (hi - lo) as f32 > 0.5 * hi as f32 {
                        bad += 1.0;
                    }
                    sum += lum;
                    n += 1.0;
                }
            }
            v[gy * GRID_W + gx] = if n > 0.0 { sum / n } else { 0.0 };
            ok[gy * GRID_W + gx] = n > 0.0 && bad / n < 0.1;
        }
    }
    Some(Grid { w: GRID_W, h: gh, v, ok })
}

fn median(v: &mut [f32]) -> f32 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Compares the raw with the camera's JPEG in `file` (the whole raw file's bytes).
pub fn analyse(frame: &RawFrame, file: &[u8]) -> Analysis {
    let none = |why| Analysis { rings: Vec::new(), gain: None, outer_stops: 0.0, scatter: 0.0, why_not: Some(why) };
    let (w, h) = (frame.width as f32, frame.height as f32);
    let gh = (GRID_W as f32 * h / w).round() as usize;
    let Some(reference) = jpeg_grid(file, gh, w / h) else { return none("no usable embedded JPEG") };
    let raw = raw_grid(frame, gh);
    let half_diag = 0.5 * w.hypot(h);

    // Each grid pixel of the corrected frame: its JPEG value, the raw's at the sensor position it
    // came from, and that position's radius.
    let mut samples: Vec<(f32, f32, f32, u8)> = Vec::new(); // (radius, jpeg log2, raw log2, which side)
    for gy in 0..gh {
        for gx in 0..GRID_W {
            let i = gy * GRID_W + gx;
            if !reference.ok[i] || reference.v[i] <= 0.0 {
                continue;
            }
            let p = [((gx as f32 + 0.5) / GRID_W as f32 - 0.5) * w, ((gy as f32 + 0.5) / gh as f32 - 0.5) * h];
            let s = frame.lens.map_or(p, |l| l.source(Channel::Green, p, w, h));
            let (u, v) = ((s[0] / w + 0.5) * GRID_W as f32 - 0.5, (s[1] / h + 0.5) * gh as f32 - 0.5);
            let Some(r) = raw.at(u, v).filter(|&r| r > 0.004) else { continue };
            let side = (s[0] < 0.0) as u8 | (((s[1] < 0.0) as u8) << 1);
            samples.push((s[0].hypot(s[1]) / half_diag, reference.v[i].log2(), r.log2(), side));
        }
    }

    // The camera's tone curve, from the middle of the frame where falloff is small: for bins
    // of the JPEG's brightness, the raw's median brightness.
    let bin_of = |l: f32| (((l + 10.0) / 10.0 * TONE_BINS as f32) as isize).clamp(0, TONE_BINS as isize - 1) as usize;
    let mut bins: Vec<Vec<f32>> = vec![Vec::new(); TONE_BINS];
    for &(r, j, raw_l, _) in &samples {
        if r < 0.4 {
            bins[bin_of(j)].push(raw_l);
        }
    }
    let curve: Vec<Option<f32>> = bins.iter_mut().map(|b| (b.len() >= 20).then(|| median(b))).collect();
    if curve.iter().filter(|c| c.is_some()).count() < 4 {
        return none("too little of the middle of the frame to match tones");
    }
    let expected = |j: f32| -> Option<f32> {
        let b = bin_of(j);
        // Only a populated bin at or within two steps of this one counts.
        (0..=2usize).find_map(|d| curve[b.saturating_sub(d)].or_else(|| curve.get(b + d).copied().flatten()))
    };
    let mut rings: Vec<Vec<f32>> = vec![Vec::new(); RINGS];
    // The outer part of each half of the frame (left, right, top, bottom), to check the falloff
    // is the same all round, as a lens's is.
    let mut halves: [Vec<f32>; 4] = Default::default();
    for &(r, j, raw_l, side) in &samples {
        let ring = (r * RINGS as f32) as usize;
        if ring < RINGS
            && let Some(e) = expected(j)
        {
            rings[ring].push(e - raw_l); // positive: the raw is darker than the JPEG's tones say
            if r >= 0.7 {
                halves[(side & 1) as usize].push(e - raw_l);
                halves[2 + (side >> 1) as usize].push(e - raw_l);
            }
        }
    }
    let measured: Vec<(f32, usize)> =
        rings.iter_mut().map(|v| if v.len() >= 60 { (median(v), v.len()) } else { (f32::NAN, v.len()) }).collect();
    let usable: Vec<usize> = (0..RINGS).filter(|&k| measured[k].0.is_finite()).collect();
    let outer = usable.iter().filter(|&&k| k >= RINGS * 7 / 10).count();
    if usable.len() < 6 || outer < 2 {
        return Analysis { rings: measured, why_not: Some("too few usable rings, or none near the corners"), ..none("") };
    }

    // Least squares for a, b on the rings' linear gains, weighted by pixel count, over the
    // candidates that keep the gain at 1 or more and rising; the best of those.
    let target: Vec<(f32, f32, f32)> = usable
        .iter()
        .map(|&k| ((k as f32 + 0.5) / RINGS as f32, measured[k].0.exp2(), (measured[k].1 as f32).sqrt()))
        .collect();
    let sse = |g: [f32; 2]| -> f32 {
        target.iter().map(|&(r, t, wt)| (wt * (gain(g, r).log2() - t.log2())).powi(2)).sum::<f32>()
    };
    // Normal equations for 1 + a·x + b·y with x = r², y = r⁴.
    let (mut sxx, mut sxy, mut syy, mut sxt, mut syt) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for &(r, t, wt) in &target {
        let (x, y, wt2) = (r * r, r.powi(4), wt * wt);
        sxx += wt2 * x * x;
        sxy += wt2 * x * y;
        syy += wt2 * y * y;
        sxt += wt2 * x * (t - 1.0);
        syt += wt2 * y * (t - 1.0);
    }
    let det = sxx * syy - sxy * sxy;
    let mut candidates = vec![[0.0, 0.0]];
    if det.abs() > 1e-12 {
        candidates.push([(sxt * syy - syt * sxy) / det, (syt * sxx - sxt * sxy) / det]);
    }
    candidates.push([(sxt / sxx).max(0.0), 0.0]);
    candidates.push([0.0, (syt / syy).max(0.0)]);
    let rising = |g: [f32; 2]| (0..=20).all(|i| 2.0 * g[0] * (i as f32 / 20.0) + 4.0 * g[1] * (i as f32 / 20.0).powi(3) >= -1e-6);
    let best = candidates
        .into_iter()
        .filter(|&g| rising(g))
        .filter(|&g| g.iter().all(|v| v.is_finite()) && gain(g, 1.0) >= 1.0)
        .min_by(|a, b| sse(*a).total_cmp(&sse(*b)));
    let Some(g) = best else { return Analysis { rings: measured, why_not: Some("no rising fit"), ..none("") } };

    let wsum: f32 = target.iter().map(|t| t.2 * t.2).sum();
    let scatter = (target.iter().map(|&(r, t, wt)| wt * wt * (gain(g, r).log2() - t.log2()).powi(2)).sum::<f32>() / wsum).sqrt();
    let outer_stops = gain(g, 0.95).log2();
    // Falloff is the same all round: a strong fit needs every half to show at least a fifth of
    // it (one bright side is not a lens); a faint one (under 0.1 stop, too small to see) only
    // needs no half to say the opposite.
    let lopsided = halves.iter_mut().any(|h| h.len() < 60 || median(h) < if outer_stops >= 0.1 { outer_stops / 5.0 } else { -0.02 });
    let verdict = if outer_stops < MIN_GAP {
        Some("the camera's corners are not brighter than the raw's")
    } else if lopsided {
        Some("the falloff is not the same all round")
    } else if scatter > MAX_SCATTER {
        Some("the fit is too scattered to trust")
    } else if outer_stops > 1.5 {
        Some("implausibly strong")
    } else {
        None
    };
    Analysis { rings: measured, gain: verdict.is_none().then_some(g), outer_stops, scatter, why_not: verdict }
}
