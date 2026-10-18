//! The tone model: how scene light becomes display light, and what the tone sliders do on the
//! way. Fitted to Lightroom Classic's rendering from screenshots of both apps (see docs/tone.md
//! and tools/tonefit).
//!
//! Everything works in *curve input*: log2 of scene light after the camera's offset and the
//! Exposure slider, where the base curve puts middle grey at [`MIDDLE`]. The sliders move each
//! pixel's brightness along that axis, and the base curve then maps it to the display, in a way
//! that keeps hues:
//! - Contrast steepens or flattens around middle grey.
//! - Blacks subtracts (or adds) a little light, moving where the image turns black.
//! - Whites lifts or lowers the upper tones; lifting also stretches a photo whose brightest
//!   parts sit well below white toward it, as Lightroom's does.
//! - Highlights and Shadows act on each area's surroundings rather than on the pixel alone: the
//!   amount is chosen from a smoothed, edge-preserving copy of the image (the *base*), and the
//!   same change applied to every pixel of the area, so detail within it is kept.
//!
//! This is the CPU reference. The GPU develop pass implements the same functions.

use crate::color;
use crate::develop::Edit;

/// The curve input that becomes middle grey (0.18).
pub const MIDDLE: f32 = -3.72;

// The base curve: y = YMAX · s^N / (s^N + SIGMA^N), with s = 2^x, clipped at 1. Fitted to
// Lightroom's default rendering within 0.07 stops from 9 stops below middle grey to white.
pub const N: f32 = 1.65;
pub const LOG2_SIGMA: f32 = -2.34;
pub const YMAX: f32 = 1.05;

/// Where the base curve reaches 0.97 (just short of white): the target Whites stretches toward.
pub const NEAR_WHITE: f32 = -0.16;

/// How much brighter or darker each make's raws are rendered, in stops: the camera's own
/// metering puts middle grey at different distances below the sensor's clipping point (Adobe's
/// profiles carry the same kind of offset). Measured against Lightroom: OM System 0, Panasonic
/// −0.47; others are untested and get Panasonic's, the more common case.
pub fn camera_offset(make: &str) -> f32 {
    let make = make.to_ascii_lowercase();
    if make.contains("olympus") || make.contains("om digital") || make.starts_with("om") {
        0.0
    } else {
        -0.47
    }
}

/// Display light (linear, 0–1) for a curve input given as linear light `s` (= 2^x).
pub fn base_curve(s: f32) -> f32 {
    if s <= 0.0 {
        return 0.0;
    }
    let r = (s.log2() - LOG2_SIGMA) * N;
    // s^N / (s^N + σ^N) = 1 / (1 + 2^-r), which stays finite for any s.
    (YMAX / (1.0 + (-r).exp2())).min(1.0)
}

/// Applies the base curve keeping hue, as Adobe's DNG reference does: the largest and smallest
/// channels go through the curve, and the middle one keeps its place between them. Near white
/// the smallest channel rises toward the largest, so bright colors fade to white naturally.
pub fn tone_rgb(rgb: [f32; 3]) -> [f32; 3] {
    let c = rgb.map(|v| v.max(0.0));
    let (hi, lo) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
    if hi <= lo {
        let v = base_curve(hi);
        return [v; 3];
    }
    let (hi_out, lo_out) = (base_curve(hi), base_curve(lo));
    c.map(|v| lo_out + (hi_out - lo_out) * (v - lo) / (hi - lo))
}

/// The shape of each slider. The values are fitted to Lightroom by `tonefit fit`; distances are
/// in stops of curve input, measured from middle grey unless said otherwise.
// All f32, in this order, so the GPU reads it as it is.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Model {
    /// Contrast ±100 scales distances from middle grey by 2^±this.
    pub contrast: f32,
    /// Blacks −100 subtracts this much linear curve input (middle grey's is 0.076)…
    pub blacks_down: f32,
    /// …and Blacks +100 adds this much at middle grey's input, growing with brightness as
    /// s^`blacks_up_growth`.
    pub blacks_up: f32,
    pub blacks_up_growth: f32,
    /// Blacks responds to its slider as |v|^this.
    pub blacks_power: f32,
    /// Highlights ±100 moves an area by `highlights_floor` + `highlights_amount` · ramp, the ramp
    /// rising from `highlights_from` to `highlights_to` (by the area's base); +100 moves
    /// `highlights_up` times as far as −100.
    pub highlights_floor: f32,
    pub highlights_amount: f32,
    pub highlights_from: f32,
    pub highlights_to: f32,
    pub highlights_up: f32,
    /// Shadows ±100 moves an area by `shadows_slope` stops per stop its base sits below
    /// `shadows_knee`, the corner softened over `shadows_soft` stops and capped at 3.5.
    pub shadows_slope: f32,
    pub shadows_knee: f32,
    pub shadows_soft: f32,
    /// For Shadows +100, the knee moves up by how far the image's 25th percentile sits below
    /// `shadows_target`, by up to `shadows_reach` stops.
    pub shadows_reach: f32,
    pub shadows_target: f32,
    /// Whites −100 lowers a pixel by `whites_down` · ramp from `whites_down_from` to
    /// `whites_down_to`, responding as |v|^`whites_down_power`.
    pub whites_down: f32,
    pub whites_down_from: f32,
    pub whites_down_to: f32,
    pub whites_down_power: f32,
    /// Whites +100 raises a pixel by `whites_up` · ramp from `whites_up_from` to `whites_up_to`,
    /// as v^`whites_up_power`; and stretches the image's top toward white by its headroom
    /// (scaled from `whites_stretch_floor` far below the top to 1 at it), as v^`whites_stretch_power`.
    pub whites_up: f32,
    pub whites_up_from: f32,
    pub whites_up_to: f32,
    pub whites_up_power: f32,
    pub whites_stretch_floor: f32,
    pub whites_stretch_power: f32,
    /// The base layer: the filter radius as a fraction of the long edge, and its edge
    /// threshold in stops² (larger blurs across more edges).
    pub base_radius: f32,
    pub base_eps: f32,
}

pub const MODEL: Model = Model {
    contrast: 0.6000,
    blacks_down: 0.0285,
    blacks_up: 0.0308,
    blacks_up_growth: 0.6831,
    blacks_power: 1.5625,
    highlights_floor: 0.0078,
    highlights_amount: 1.1219,
    highlights_from: -2.1719,
    highlights_to: 3.1876,
    highlights_up: 0.8375,
    shadows_slope: 0.4500,
    shadows_knee: 0.2750,
    shadows_soft: 0.3250,
    shadows_reach: 3.0000,
    shadows_target: -2.5780,
    whites_down: 0.7750,
    whites_down_from: -1.0000,
    whites_down_to: 3.2000,
    whites_down_power: 0.7000,
    whites_up: 0.8531,
    whites_up_from: -2.6720,
    whites_up_to: 2.1688,
    whites_up_power: 1.8000,
    whites_stretch_floor: 0.3437,
    whites_stretch_power: 3.0000,
    base_radius: 0.1000,
    base_eps: 0.5000,
};

/// What the tone sliders need to know about the whole image.
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    /// The 99.5th percentile of curve input, as shot (before Exposure and the tone sliders):
    /// where the image's brightest parts are.
    pub top: f32,
    /// The 25th percentile: where its shadows are.
    pub dark: f32,
}

/// Slider values as −1 to 1 (Exposure stays in stops).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Sliders {
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
}

impl Sliders {
    pub fn from_edit(e: &Edit) -> Self {
        Sliders {
            contrast: e.contrast / 100.0,
            highlights: e.highlights / 100.0,
            shadows: e.shadows / 100.0,
            whites: e.whites / 100.0,
            blacks: e.blacks / 100.0,
        }
    }
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The global sliders, Contrast then Blacks, on one curve input. Applied to both the pixel and
/// its base, so the local sliders see the same tones the pixel has.
pub fn global(m: &Model, x: f32, s: &Sliders) -> f32 {
    let x = MIDDLE + (x - MIDDLE) * (m.contrast * s.contrast).exp2();
    let lin = x.exp2();
    let amount = s.blacks.abs().powf(m.blacks_power);
    let lin = if s.blacks < 0.0 {
        // The darkest tones go to black, and the rest darken less the brighter they are.
        let b = m.blacks_down * amount;
        (lin - b).max(1e-9) / (1.0 - b)
    } else {
        // Opens the darkest tones the most.
        lin + m.blacks_up * amount * (lin / MIDDLE.exp2()).powf(m.blacks_up_growth)
    };
    lin.log2()
}

/// The local sliders and Whites: the change in stops for a pixel at `x` whose base is `base`
/// (both after [`global`]).
pub fn local(m: &Model, x: f32, base: f32, s: &Sliders, stats: &Stats) -> f32 {
    let mut dx = 0.0;
    let d = base - MIDDLE;
    let h = m.highlights_floor + m.highlights_amount * smoothstep(m.highlights_from, m.highlights_to, d);
    dx += s.highlights * if s.highlights > 0.0 { m.highlights_up * h } else { h };
    // A photo whose shadows sit deep gets more of its tones treated as shadows: its knee moves
    // up by how deep they are (measured as shot, so no slider moves it). The slope is held
    // under 0.75 stop per stop, so tones never swap places, in the photo or as Exposure moves.
    let depth = if s.shadows > 0.0 { (m.shadows_target - (stats.dark - MIDDLE)).clamp(0.0, m.shadows_reach) } else { 0.0 };
    let w = m.shadows_soft;
    let soft = m.shadows_slope.min(0.75) * w * (1.0 + ((m.shadows_knee + depth - d) / w).min(40.0).exp()).ln();
    dx += s.shadows * soft.min(3.5);
    let d = x - MIDDLE;
    if s.whites < 0.0 {
        dx -= (-s.whites).powf(m.whites_down_power) * m.whites_down * smoothstep(m.whites_down_from, m.whites_down_to, d);
    } else if s.whites > 0.0 {
        dx += s.whites.powf(m.whites_up_power) * m.whites_up * smoothstep(m.whites_up_from, m.whites_up_to, d);
        let headroom = (NEAR_WHITE - stats.top).max(0.0);
        let reach = m.whites_stretch_floor + (1.0 - m.whites_stretch_floor) * smoothstep(-6.0, 0.5, x - stats.top);
        dx += s.whites.powf(m.whites_stretch_power) * headroom * reach;
    }
    dx
}

/// Rec.709 luminance of linear sRGB: the brightness the sliders measure.
pub fn luminance(p: [f32; 3]) -> f32 {
    0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]
}

/// The edge-preserving base: a guided filter (He et al.) of log2 luminance, computed on a copy
/// `factor` times smaller and brought back up through its coefficients, so edges stay sharp at
/// full size. `radius` is in pixels of the small copy; `eps` is in stops², and the larger it is
/// the more edges blur.
/// The small copy: the mean of each `factor` × `factor` block of `x`. Returns it with its size.
pub fn small_copy(x: &[f32], w: usize, h: usize, factor: usize) -> (Vec<f32>, usize, usize) {
    let (sw, sh) = (w.div_ceil(factor), h.div_ceil(factor));
    let mut small = vec![0.0f32; sw * sh];
    let mut count = vec![0u32; sw * sh];
    for j in 0..h {
        for i in 0..w {
            let k = (j / factor) * sw + i / factor;
            small[k] += x[j * w + i];
            count[k] += 1;
        }
    }
    for (v, c) in small.iter_mut().zip(&count) {
        *v /= *c as f32;
    }
    (small, sw, sh)
}

/// The base layer of `x` (see [`base_from_small`]).
pub fn base_layer(x: &[f32], w: usize, h: usize, factor: usize, radius: usize, eps: f32) -> Vec<f32> {
    let (small, sw, sh) = small_copy(x, w, h, factor);
    base_from_small(&small, sw, sh, x, w, h, factor, radius, eps)
}

/// Like [`base_layer`], from a small copy already made.
#[allow(clippy::too_many_arguments)]
pub fn base_from_small(small: &[f32], sw: usize, sh: usize, x: &[f32], w: usize, h: usize, factor: usize, radius: usize, eps: f32) -> Vec<f32> {
    let sq: Vec<f32> = small.iter().map(|v| v * v).collect();
    let mean = box_blur(small, sw, sh, radius);
    let mean_sq = box_blur(&sq, sw, sh, radius);
    let mut a = vec![0.0f32; sw * sh];
    let mut b = vec![0.0f32; sw * sh];
    for k in 0..sw * sh {
        let var = (mean_sq[k] - mean[k] * mean[k]).max(0.0);
        a[k] = var / (var + eps);
        b[k] = mean[k] - a[k] * mean[k];
    }
    let (a, b) = (box_blur(&a, sw, sh, radius), box_blur(&b, sw, sh, radius));
    let mut out = vec![0.0f32; w * h];
    for j in 0..h {
        for i in 0..w {
            let (ca, cb) = (bilinear(&a, sw, sh, i, j, factor), bilinear(&b, sw, sh, i, j, factor));
            out[j * w + i] = ca * x[j * w + i] + cb;
        }
    }
    out
}

/// A box average over (2r+1)² pixels, with the window shrunk at the borders.
fn box_blur(v: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let pass = |src: &[f32], horizontal: bool| -> Vec<f32> {
        let mut out = vec![0.0f32; w * h];
        let (len, lines) = if horizontal { (w, h) } else { (h, w) };
        for line in 0..lines {
            let at = |p: usize| if horizontal { line * w + p } else { p * w + line };
            let mut prefix = vec![0.0f64; len + 1];
            for p in 0..len {
                prefix[p + 1] = prefix[p] + src[at(p)] as f64;
            }
            for p in 0..len {
                let (lo, hi) = (p.saturating_sub(r), (p + r + 1).min(len));
                out[at(p)] = ((prefix[hi] - prefix[lo]) / (hi - lo) as f64) as f32;
            }
        }
        out
    };
    pass(&pass(v, true), false)
}

/// Samples a small-copy value at full-size pixel (i, j), between the small copy's pixel centres.
fn bilinear(v: &[f32], sw: usize, sh: usize, i: usize, j: usize, factor: usize) -> f32 {
    let fx = ((i as f32 + 0.5) / factor as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
    let fy = ((j as f32 + 0.5) / factor as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(sw - 1), (y0 + 1).min(sh - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let row = |y: usize| v[y * sw + x0] * (1.0 - tx) + v[y * sw + x1] * tx;
    row(y0) * (1.0 - ty) + row(y1) * ty
}

/// The base layer is computed on a copy about this many pixels on its long edge, whatever the
/// image size.
pub const BASE_EDGE: f32 = 128.0;

/// Clarity's base is computed on a copy about this many pixels on its long edge (the tone model's
/// is 128): fine enough to follow the shapes of a scene's light, with a guided filter of
/// `CLARITY_RADIUS` of the long edge and `CLARITY_EPS` (stops²; small, so strong edges stay put and
/// the local contrast has no halos).
pub const CLARITY_EDGE: f32 = 512.0;
pub const CLARITY_RADIUS: f32 = 0.06;
pub const CLARITY_EPS: f32 = 0.8;

/// Clarity: the change in stops for a pixel at `x` (after [`global`]) whose edge-preserving base is
/// `base`, for `c` from −1 to 1. The pixel's distance from its base is scaled, softly limited at 1.5
/// stops so one bright speck doesn't swing; weighted toward the mid-tones, fading out in the deep
/// shadows and toward white so neither clips. The weight changes slowly and the distance's own
/// slope is at most 1, so tones never swap places (not in the photo, not as Exposure moves).
/// The GPU twin is `clarityShift` in develop.hlsl.
pub fn clarity_shift(x: f32, base: f32, c: f32) -> f32 {
    let d = x - MIDDLE;
    let weight = smoothstep(-10.0, -4.0, d) * (1.0 - smoothstep(1.0, 5.5, d));
    let deviation = 2.0 * ((x - base) / 2.0).tanh();
    c * if c > 0.0 { 1.3 } else { 0.55 } * weight * deviation
}

/// Develops a scene-linear image (linear sRGB primaries, 1.0 = the sensor's clipping point) to
/// display-linear sRGB with the tone model: the reference the GPU must match.
pub fn render(m: &Model, linear: &[[f32; 3]], w: usize, h: usize, edit: &Edit, camera_offset: f32) -> Vec<[f32; 3]> {
    let s = Sliders::from_edit(edit);
    let gain = (camera_offset + edit.exposure).exp2();
    let x: Vec<f32> = linear.iter().map(|p| (luminance(*p) * gain).max(1e-9).log2()).collect();
    let xg: Vec<f32> = x.iter().map(|v| global(m, *v, &s)).collect();
    let edge = w.max(h) as f32;
    let factor = ((edge / BASE_EDGE).ceil() as usize).max(1);
    let radius = ((edge * m.base_radius / factor as f32).round() as usize).max(1);
    // Like the GPU, the guided filter and the percentiles both see a small copy. The percentiles
    // see the photo as shot (before Exposure and the other sliders), so moving a slider never
    // changes what the adaptive terms react to: otherwise lowering Exposure deepens the
    // shadows, Shadows lifts them further, and the photo can end up brighter.
    let (small, sw, sh) = small_copy(&xg, w, h, factor);
    let base = base_from_small(&small, sw, sh, &xg, w, h, factor, radius, m.base_eps);
    // Clarity's base: a bigger small copy and a wider, stricter filter.
    let clarity_base = (edit.clarity != 0.0).then(|| {
        let factor = ((edge / CLARITY_EDGE).ceil() as usize).max(1);
        let radius = ((edge * CLARITY_RADIUS / factor as f32).round() as usize).max(1);
        let (small, sw, sh) = small_copy(&xg, w, h, factor);
        base_from_small(&small, sw, sh, &xg, w, h, factor, radius, CLARITY_EPS)
    });
    let as_shot: Vec<f32> = x.iter().map(|v| v - edit.exposure).collect();
    let (mut sorted, _, _) = small_copy(&as_shot, w, h, factor);
    sorted.sort_by(f32::total_cmp);
    let at = |q: f32| sorted[((sorted.len() - 1) as f32 * q) as usize];
    let stats = Stats { top: at(0.995), dark: at(0.25) };
    // The curve keeps hue in Rec.2020, the working space: in a space as narrow as sRGB, strong
    // colors' channels sit so far apart that the curve darkens them (a deep blue sky by a
    // fifth of a stop), which Lightroom, working in a wide space too, doesn't.
    let to_wide = color::invert(&color::REC2020_TO_SRGB);
    let apply = |m: &color::Mat3, p: [f32; 3]| m.map(|r| r[0] * p[0] + r[1] * p[1] + r[2] * p[2]);
    linear
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let mut out = xg[k] + local(m, xg[k], base[k], &s, &stats);
            if let Some(b) = &clarity_base {
                out += clarity_shift(xg[k], b[k], edit.clarity / 100.0);
            }
            let wide = apply(&to_wide, p.map(|v| v * gain * (out - x[k]).exp2()));
            apply(&color::REC2020_TO_SRGB, tone_rgb(wide)).map(|v| v.clamp(0.0, 1.0))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_grey_and_white() {
        assert!((base_curve(MIDDLE.exp2()) - 0.18).abs() < 0.002);
        assert!((base_curve(NEAR_WHITE.exp2()) - 0.97).abs() < 0.003);
        assert_eq!(base_curve(8.0), 1.0);
    }

    #[test]
    fn hue_is_kept() {
        let out = tone_rgb([0.02, 0.05, 0.11]);
        let ratio = |c: [f32; 3]| (c[1] - c[0]) / (c[2] - c[0]);
        assert!((ratio(out) - ratio([0.02, 0.05, 0.11])).abs() < 1e-5);
    }

    #[test]
    fn neutral_sliders_change_nothing() {
        let s = Sliders::from_edit(&Edit::default());
        let stats = Stats { top: -1.0, dark: -6.0 };
        for x in [-9.0, -3.72, -0.5] {
            assert!((global(&MODEL, x, &s) - x).abs() < 1e-4);
            assert_eq!(local(&MODEL, x, x, &s, &stats), 0.0);
        }
    }


    #[test]
    fn clarity_shift_properties() {
        // Nothing at 0 or at the base; weighted to the mid-tones; limited; never swaps tones.
        assert_eq!(clarity_shift(-3.0, -2.0, 0.0), 0.0);
        assert_eq!(clarity_shift(-3.0, -3.0, 1.0), 0.0);
        assert!(clarity_shift(MIDDLE, MIDDLE - 1.0, 1.0) > 0.8);
        assert!(clarity_shift(MIDDLE, MIDDLE - 1.0, -1.0) < -0.4);
        assert_eq!(clarity_shift(-15.0, -17.0, 1.0), 0.0, "true black is left alone");
        assert_eq!(clarity_shift(2.0, -1.0, 1.0), 0.0, "and so are the clipped tones");
        // The shadows and the bright tones take part, at reduced strength.
        let mid = clarity_shift(MIDDLE, MIDDLE - 1.0, 1.0);
        let (shadow, bright) = (clarity_shift(-10.0, -11.0, 1.0), clarity_shift(-0.5, -1.5, 1.0));
        assert!(shadow > 0.2 * mid && shadow < 0.9 * mid, "shadows: {shadow} against {mid}");
        assert!(bright > 0.3 * mid && bright < 0.9 * mid, "bright tones: {bright} against {mid}");
        assert!(clarity_shift(MIDDLE, MIDDLE - 40.0, 1.0) < 3.1, "one bright speck doesn't swing");
        // Output is increasing in x for any base within reach (the guided filter's slope a is in 0..1).
        for c in [-1.0f32, 1.0] {
            for a in [0.0f32, 0.3, 1.0] {
                let mut last = f32::MIN;
                for i in 0..400 {
                    let x = -14.0 + i as f32 * 0.04;
                    let base = a * x - 0.5 * a; // base = a x + b
                    let y = x + clarity_shift(x, base, c);
                    assert!(y > last - 1e-6, "c {c}, a {a}: tones swap at x {x}");
                    last = y;
                }
            }
        }
    }
    /// Lowering Exposure must never brighten the photo, whatever the other sliders: the
    /// adaptive terms react to the image's own brightness, so they mustn't outrun it.
    #[test]
    fn less_exposure_is_never_brighter() {
        // A dark scene with a little sky: shadows deep enough for the adaptive lift to act.
        let (w, h) = (64, 48);
        let img: Vec<[f32; 3]> = (0..w * h)
            .map(|k| {
                let (i, j) = (k % w, k / w);
                let v = if j < 12 { 0.5 } else { 0.002 * (1.0 + (i * 7 % 13) as f32) * (1.0 + j as f32 / 48.0) };
                [v, v * 0.9, v * 1.1]
            })
            .collect();
        for (clarity, shadows, whites, highlights) in [(0.0, 0.0, 0.0, 0.0), (0.0, 50.0, 0.0, 0.0), (0.0, 100.0, 0.0, 0.0), (0.0, 100.0, 100.0, -100.0), (0.0, 100.0, 100.0, 100.0), (0.0, -100.0, -100.0, 100.0), (0.0, 0.0, 100.0, 0.0), (100.0, 0.0, 0.0, 0.0), (-100.0, 0.0, 0.0, 0.0), (100.0, 100.0, 100.0, 100.0), (-100.0, 100.0, -100.0, 100.0), (100.0, -100.0, 100.0, -100.0)] {
            let mut last = f32::MAX;
            for step in 0..=80 {
                let edit = Edit { exposure: 3.0 - step as f32 * 0.1, shadows, whites, highlights, clarity, ..Default::default() };
                let out = render(&MODEL, &img, w, h, &edit, 0.0);
                let mean = out.iter().map(|p| luminance(*p)).sum::<f32>() / out.len() as f32;
                assert!(mean <= last + 1e-4, "exposure {} brightened the photo (clarity {clarity}, shadows {shadows}, whites {whites}): {last} → {mean}", edit.exposure);
                last = mean;
            }
        }
    }
}
