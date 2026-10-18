//! The colour mixer (HSL): hue, saturation and luminance for eight colour ranges, as in
//! Lightroom's Color Mixer. The GPU runs it in `shaders/develop.hlsl` (`mixer()`); this is the
//! reference the shader follows, the parameters the shader reads, and the tests. See
//! docs/phase2c.md.
//!
//! It works in OkLCh on display-linear sRGB, after the tone step and before vibrance and
//! saturation. A pixel's hue picks its two nearest ranges (raised-cosine weights that sum to 1);
//! chroma decides how much the mixer applies at all (nothing for greys).

use std::f32::consts::PI;

/// Red, Orange, Yellow, Green, Aqua, Blue, Purple, Magenta.
pub const RANGES: usize = 8;
pub const NAMES: [&str; RANGES] = ["Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta"];

/// The OkLCh hue (degrees) each range is centred on: that of the sRGB colour at Lightroom's HSL
/// hue (0, 30, 60, 120, 180, 240, 270, 300 degrees, full saturation and value). Red, Green and
/// Blue are the sRGB primaries'. Measured by `centres_follow_lightroom_hues`.
pub const CENTRES: [f32; RANGES] = [29.23, 52.78, 109.77, 142.50, 194.77, 264.05, 293.77, 328.36];

/// The most a Hue slider moves a range's centre, as a fraction of the distance to the neighbouring
/// centre it moves toward.
pub const HUE_REACH: f32 = 0.5;
/// Luminance ±100 for a fully chromatic colour: this many stops of linear luminance.
pub const LUMINANCE_STOPS: f32 = 1.0;
/// Chroma below which the mixer does nothing, and above which it applies fully (smoothstep).
pub const CHROMA_FROM: f32 = 0.005;
pub const CHROMA_TO: f32 = 0.06;

/// What the shader reads (mixer in tone.hlsli's ToneParams): per range, in order Red … Magenta.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Where the range's centre goes (degrees, a shift from `CENTRES`).
    pub hue_shift: [f32; RANGES],
    /// The hue map's slope at each centre (1 when nothing moves; less where neighbours close in).
    pub hue_slope: [f32; RANGES],
    /// −1..1; the chroma factor is 1 + this.
    pub saturation: [f32; RANGES],
    /// −1..1; stops = this × `LUMINANCE_STOPS`.
    pub luminance: [f32; RANGES],
}

pub const NEUTRAL: Params = Params { hue_shift: [0.0; RANGES], hue_slope: [1.0; RANGES], saturation: [0.0; RANGES], luminance: [0.0; RANGES] };

/// The distance from range `i`'s centre to the next one (the last wraps to the first).
fn gap(i: usize) -> f32 {
    let next = if i + 1 == RANGES { CENTRES[0] + 360.0 } else { CENTRES[i + 1] };
    next - CENTRES[i]
}

impl Params {
    /// From the edit's 24 sliders: hue ×8, saturation ×8, luminance ×8, each −100 to 100.
    pub fn from_sliders(s: &[f32; 3 * RANGES]) -> Params {
        let unit = |v: f32| if v.is_finite() { (v / 100.0).clamp(-1.0, 1.0) } else { 0.0 };
        let mut p = NEUTRAL;
        // A range's centre moves toward the neighbour on the side the slider points to, by up to
        // half the way. Two neighbours moving toward each other can only meet half way, so the
        // map stays monotonic.
        for i in 0..RANGES {
            let v = unit(s[i]);
            let toward = if v >= 0.0 { gap(i) } else { gap((i + RANGES - 1) % RANGES) };
            p.hue_shift[i] = v * HUE_REACH * toward;
            p.saturation[i] = unit(s[RANGES + i]);
            p.luminance[i] = unit(s[2 * RANGES + i]);
        }
        // Fritsch-Carlson-style limit: a cubic Hermite through the moved centres is monotonic
        // when the slopes at the ends of each span are at most three times the span's mean slope.
        let delta = |i: usize| {
            let j = (i + 1) % RANGES;
            let span = gap(i);
            (span + p.hue_shift[j] - p.hue_shift[i]) / span
        };
        for i in 0..RANGES {
            let before = delta((i + RANGES - 1) % RANGES);
            p.hue_slope[i] = 1f32.min(3.0 * before).min(3.0 * delta(i)).max(0.0);
        }
        p
    }

    /// Nothing to do: every slider is zero.
    pub fn is_neutral(&self) -> bool {
        *self == NEUTRAL
    }
}

/// Raised-cosine weights over the hue circle: the pixel's two nearest ranges share it, so they
/// sum to 1 and change smoothly (no seams) at every hue.
pub fn weights(hue: f32) -> [f32; RANGES] {
    let (i, t) = segment(hue);
    let mut w = [0.0; RANGES];
    let up = 0.5 - 0.5 * (PI * t).cos();
    w[i] = 1.0 - up;
    w[(i + 1) % RANGES] += up;
    w
}

/// The span a hue falls in (the range at its start) and how far along it (0..1).
pub fn segment(hue: f32) -> (usize, f32) {
    let mut h = hue.rem_euclid(360.0);
    if h < CENTRES[0] {
        h += 360.0;
    }
    let mut i = RANGES - 1;
    for k in 0..RANGES - 1 {
        if h < CENTRES[k + 1] {
            i = k;
            break;
        }
    }
    (i, (h - CENTRES[i]) / gap(i))
}

/// Where a hue goes: a cubic Hermite through the moved centres, which is the identity when none
/// moved. Monotonic by the slope limit in `Params::from_sliders`. In degrees, on 0..360.
pub fn map_hue(hue: f32, p: &Params) -> f32 {
    let (i, t) = segment(hue);
    let j = (i + 1) % RANGES;
    let span = gap(i);
    let (t2, t3) = (t * t, t * t * t);
    let (h00, h10, h01, h11) = (2.0 * t3 - 3.0 * t2 + 1.0, t3 - 2.0 * t2 + t, -2.0 * t3 + 3.0 * t2, t3 - t2);
    let out = h00 * (CENTRES[i] + p.hue_shift[i]) + h10 * span * p.hue_slope[i] + h01 * (CENTRES[i] + span + p.hue_shift[j]) + h11 * span * p.hue_slope[j];
    out.rem_euclid(360.0)
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How much of the mixer a pixel of this OkLab chroma gets.
pub fn chroma_factor(chroma: f32) -> f32 {
    smoothstep(CHROMA_FROM, CHROMA_TO, chroma)
}

// ---- OkLab (the same as develop.hlsl) ----

#[allow(clippy::excessive_precision)]
pub fn to_oklab(c: [f32; 3]) -> [f32; 3] {
    let l = 0.4122214708 * c[0] + 0.5363325363 * c[1] + 0.0514459929 * c[2];
    let m = 0.2119034982 * c[0] + 0.6806995451 * c[1] + 0.1073969566 * c[2];
    let s = 0.0883024619 * c[0] + 0.2817188376 * c[1] + 0.6299787005 * c[2];
    let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());
    [
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    ]
}

#[allow(clippy::excessive_precision)]
pub fn from_oklab(lab: [f32; 3]) -> [f32; 3] {
    let l = lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2];
    let m = lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2];
    let s = lab[0] - 0.0894841775 * lab[1] - 1.2914855480 * lab[2];
    let (l, m, s) = (l * l * l, m * m * m, s * s * s);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    ]
}

fn in_gamut(c: [f32; 3]) -> bool {
    c.iter().all(|&v| (-1e-5..=1.0 + 1e-5).contains(&v))
}

/// Linear sRGB → (lightness, chroma, hue in degrees 0..360).
pub fn to_lch(c: [f32; 3]) -> [f32; 3] {
    let lab = to_oklab(c);
    [lab[0], lab[1].hypot(lab[2]), lab[2].atan2(lab[1]).to_degrees().rem_euclid(360.0)]
}

pub fn from_lch(l: f32, c: f32, h: f32) -> [f32; 3] {
    let h = h.to_radians();
    from_oklab([l, c * h.cos(), c * h.sin()])
}

/// The mixer on one pixel of display-linear sRGB (clamped to 0..1 first). Out comes a colour in
/// 0..1: a change that would leave the gamut loses chroma, keeping hue and lightness.
/// The GPU twin is `mixer` in develop.hlsl.
pub fn apply(rgb: [f32; 3], p: &Params) -> [f32; 3] {
    if p.is_neutral() {
        return rgb;
    }
    let rgb = rgb.map(|v| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 });
    let [l, c, h] = to_lch(rgb);
    let g = chroma_factor(c);
    if g <= 0.0 {
        return rgb;
    }
    let w = weights(h);
    let sat: f32 = (0..RANGES).map(|k| w[k] * p.saturation[k]).sum();
    let lum: f32 = (0..RANGES).map(|k| w[k] * p.luminance[k]).sum();
    let moved = (map_hue(h, p) - h + 180.0).rem_euclid(360.0) - 180.0;
    if moved.abs() < 1e-3 && sat == 0.0 && lum == 0.0 {
        return rgb;
    }
    let hue_to = h + g * moved;
    let chroma_to = c * (1.0 + g * sat).max(0.0);
    // Linear luminance is about the cube of OkLab's lightness.
    let light_to = (l * (g * lum * LUMINANCE_STOPS / 3.0).exp2()).clamp(0.0, 1.0);
    fit(light_to, chroma_to, hue_to, rgb)
}

/// (lightness, chroma, hue) as sRGB, with the chroma cut back until it fits. Falls back to the
/// untouched pixel if something is not finite.
fn fit(l: f32, c: f32, h: f32, fallback: [f32; 3]) -> [f32; 3] {
    let out = from_lch(l, c, h);
    if in_gamut(out) {
        return out.map(|v| v.clamp(0.0, 1.0));
    }
    let (mut lo, mut hi) = (0.0, c);
    for _ in 0..FIT_STEPS {
        let mid = 0.5 * (lo + hi);
        if in_gamut(from_lch(l, mid, h)) { lo = mid } else { hi = mid }
    }
    let out = from_lch(l, lo, h).map(|v| v.clamp(0.0, 1.0));
    if out.iter().all(|v| v.is_finite()) { out } else { fallback }
}

/// Bisection steps in the shader and here.
pub const FIT_STEPS: u32 = 8;

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_to_linear(v: f32) -> f32 {
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    }

    /// Fully saturated sRGB colour at an HSL hue (degrees), in linear light.
    fn hsl_colour(hue: f32) -> [f32; 3] {
        let h = hue / 60.0;
        let x = 1.0 - (h % 2.0 - 1.0).abs();
        let rgb = match h as u32 {
            0 => [1.0, x, 0.0],
            1 => [x, 1.0, 0.0],
            2 => [0.0, 1.0, x],
            3 => [0.0, x, 1.0],
            4 => [x, 0.0, 1.0],
            _ => [1.0, 0.0, x],
        };
        rgb.map(srgb_to_linear)
    }

    const LIGHTROOM_HUES: [f32; RANGES] = [0.0, 30.0, 60.0, 120.0, 180.0, 240.0, 270.0, 300.0];

    #[test]
    fn centres_follow_lightroom_hues() {
        for i in 0..RANGES {
            let measured = to_lch(hsl_colour(LIGHTROOM_HUES[i]))[2];
            assert!((measured - CENTRES[i]).abs() < 0.05, "{}: measured {measured:.2}, table {}", NAMES[i], CENTRES[i]);
        }
        // The primaries.
        assert!((to_lch([1.0, 0.0, 0.0])[2] - CENTRES[0]).abs() < 0.05);
        assert!((to_lch([0.0, 1.0, 0.0])[2] - CENTRES[3]).abs() < 0.05);
        assert!((to_lch([0.0, 0.0, 1.0])[2] - CENTRES[5]).abs() < 0.05);
        for w in CENTRES.windows(2) {
            assert!(w[0] < w[1]);
        }
    }

    #[test]
    fn weights_sum_to_one_and_peak_at_centres() {
        for k in 0..3600 {
            let h = k as f32 / 10.0;
            let w = weights(h);
            assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5, "hue {h}");
            assert!(w.iter().all(|&v| (0.0..=1.0).contains(&v)));
            assert!(w.iter().filter(|&&v| v > 0.0).count() <= 2, "at most two ranges at {h}");
        }
        for (i, &c) in CENTRES.iter().enumerate() {
            let w = weights(c);
            assert!((w[i] - 1.0).abs() < 1e-5, "{}", NAMES[i]);
        }
        // No seams: weights are continuous (and smooth: no kink) across every centre and across 0/360.
        for &c in CENTRES.iter().chain(&[0.0, 360.0]) {
            let (a, b) = (weights(c - 0.01), weights(c + 0.01));
            for k in 0..RANGES {
                assert!((a[k] - b[k]).abs() < 1e-3, "jump at {c}");
            }
        }
    }

    fn sliders(range: usize, hue: f32, sat: f32, lum: f32) -> [f32; 24] {
        let mut s = [0.0; 24];
        s[range] = hue;
        s[8 + range] = sat;
        s[16 + range] = lum;
        s
    }

    #[test]
    fn zero_sliders_are_neutral_and_the_identity() {
        let p = Params::from_sliders(&[0.0; 24]);
        assert!(p.is_neutral());
        for k in 0..3600 {
            let h = k as f32 / 10.0;
            assert!((map_hue(h, &p) - h).abs() < 1e-3, "{h} → {}", map_hue(h, &p));
        }
        assert_eq!(apply([0.3, 0.5, 0.2], &p), [0.3, 0.5, 0.2]);
    }

    #[test]
    fn hue_map_is_monotonic_and_reaches_half_way() {
        let mut cases: Vec<[f32; 24]> = Vec::new();
        for r in 0..RANGES {
            cases.push(sliders(r, 100.0, 0.0, 0.0));
            cases.push(sliders(r, -100.0, 0.0, 0.0));
        }
        // Neighbours pushed toward each other, away, all one way, and pseudo-random mixes.
        for r in 0..RANGES {
            let mut s = [0.0; 24];
            s[r] = 100.0;
            s[(r + 1) % RANGES] = -100.0;
            cases.push(s);
        }
        cases.push(std::array::from_fn(|i| if i < 8 { 100.0 } else { 0.0 }));
        cases.push(std::array::from_fn(|i| if i < 8 { -100.0 } else { 0.0 }));
        cases.push(std::array::from_fn(|i| if i < 8 { if i % 2 == 0 { 100.0 } else { -100.0 } } else { 0.0 }));
        let mut seed = 12345u32;
        for _ in 0..300 {
            cases.push(std::array::from_fn(|i| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                if i < 8 { ((seed >> 16) % 3) as f32 * 100.0 - 100.0 } else { 0.0 }
            }));
        }
        for s in &cases {
            let p = Params::from_sliders(s);
            let mut last = None::<f32>;
            let mut total = 0.0;
            // Unwrapped hue out across one trip round the circle: never goes backwards.
            let steps = 7200;
            let mut prev = map_hue(CENTRES[0], &p);
            for k in 1..=steps {
                let h = CENTRES[0] + 360.0 * k as f32 / steps as f32;
                let now = map_hue(h, &p);
                let mut d = now - prev;
                if d < -180.0 { d += 360.0 }
                if d > 180.0 { d -= 360.0 }
                assert!(d >= -1e-3, "went backwards by {d} at {h} for {s:?}");
                total += d;
                prev = now;
                last = Some(d);
            }
            assert!(last.is_some());
            assert!((total - 360.0).abs() < 0.05, "one trip round is one trip: {total}");
        }
        // A centre moves by (up to) half the distance to the neighbour, in the slider's direction.
        for r in 0..RANGES {
            let up = Params::from_sliders(&sliders(r, 100.0, 0.0, 0.0));
            let moved = (map_hue(CENTRES[r], &up) - CENTRES[r]).rem_euclid(360.0);
            assert!((moved - 0.5 * gap(r)).abs() < 0.01, "{} +100 moved {moved}", NAMES[r]);
            let down = Params::from_sliders(&sliders(r, -100.0, 0.0, 0.0));
            let moved = (CENTRES[r] - map_hue(CENTRES[r], &down)).rem_euclid(360.0);
            assert!((moved - 0.5 * gap((r + 7) % 8)).abs() < 0.01, "{} -100 moved {moved}", NAMES[r]);
        }
    }

    #[test]
    fn greys_and_near_greys_are_untouched() {
        let all: [f32; 24] = std::array::from_fn(|i| if i % 2 == 0 { 100.0 } else { -100.0 });
        let p = Params::from_sliders(&all);
        for k in 0..=100 {
            let v = k as f32 / 100.0;
            assert_eq!(apply([v, v, v], &p), [v, v, v]);
        }
        // Noise in a near-grey: chroma under the floor does nothing either.
        let near = [0.30, 0.302, 0.299];
        assert!(to_lch(near)[1] < CHROMA_FROM);
        assert_eq!(apply(near, &p), near);
    }

    #[test]
    fn saturation_minus_100_greys_out_the_range_centre() {
        for r in 0..RANGES {
            let p = Params::from_sliders(&sliders(r, 0.0, -100.0, 0.0));
            // The colour at the centre's hue, at a lightness and chroma inside the gamut.
            let colour = from_lch(0.65, 0.1, CENTRES[r]);
            assert!(in_gamut(colour), "{}", NAMES[r]);
            let out = apply(colour, &p);
            let [l, c, _] = to_lch(out);
            assert!(c < 1e-3, "{}: chroma {c}", NAMES[r]);
            assert!((l - 0.65).abs() < 1e-3, "{}: lightness {l}", NAMES[r]);
        }
        // The pure sRGB primaries too, for their own ranges.
        for (r, c) in [(0, [1.0, 0.0, 0.0]), (3, [0.0, 1.0, 0.0]), (5, [0.0, 0.0, 1.0])] {
            let out = apply(c, &Params::from_sliders(&sliders(r, 0.0, -100.0, 0.0)));
            assert!(to_lch(out)[1] < 2e-3, "{} → {out:?}", NAMES[r]);
        }
    }

    #[test]
    fn saturation_plus_100_roughly_doubles_and_keeps_hue_and_lightness() {
        let p = Params::from_sliders(&sliders(3, 0.0, 100.0, 0.0));
        let colour = from_lch(0.6, 0.06, CENTRES[3]);
        let [l0, c0, h0] = to_lch(colour);
        let [l1, c1, h1] = to_lch(apply(colour, &p));
        assert!((c1 / c0 - 2.0).abs() < 0.02, "{}", c1 / c0);
        assert!((l1 - l0).abs() < 1e-3 && (h1 - h0).abs() < 0.1);
        // An already strong colour is limited by the gamut instead of clipping: hue and lightness stay.
        let strong = from_lch(0.6, 0.2, CENTRES[3]);
        let [l0, c0, h0] = to_lch(strong);
        let out = apply(strong, &p);
        assert!(in_gamut(out));
        let [l1, c1, h1] = to_lch(out);
        assert!(c1 >= c0 * 0.99 && (l1 - l0).abs() < 2e-3 && (h1 - h0).abs() < 0.3, "{c0}→{c1}, {l0}→{l1}, {h0}→{h1}");
    }

    #[test]
    fn luminance_is_about_a_stop_at_a_saturated_centre() {
        let colour = from_lch(0.5, 0.12, CENTRES[5]);
        let y = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        for (v, stops) in [(-100.0, -1.0f32), (100.0, 1.0), (-50.0, -0.5)] {
            let out = apply(colour, &Params::from_sliders(&sliders(5, 0.0, 0.0, v)));
            let got = (y(out) / y(colour)).log2();
            assert!((got - stops).abs() < 0.2, "Blue luminance {v}: {got} stops");
        }
        // Nothing of the other ranges moves.
        let other = from_lch(0.5, 0.12, CENTRES[0]);
        let out = apply(other, &Params::from_sliders(&sliders(5, 0.0, 0.0, -100.0)));
        for k in 0..3 {
            assert!((out[k] - other[k]).abs() < 1e-3);
        }
    }

    #[test]
    fn no_nan_and_always_in_range() {
        let settings: Vec<[f32; 24]> = vec![
            std::array::from_fn(|i| if i < 8 { 100.0 } else { 0.0 }),
            std::array::from_fn(|i| if (8..16).contains(&i) { 100.0 } else { 0.0 }),
            std::array::from_fn(|i| if i >= 16 { 100.0 } else { 0.0 }),
            std::array::from_fn(|i| if i >= 16 { -100.0 } else { 0.0 }),
            std::array::from_fn(|i| if i % 3 == 0 { 100.0 } else { -100.0 }),
            std::array::from_fn(|i| if i % 2 == 0 { 100.0 } else { -100.0 }),
            [100.0; 24],
            [-100.0; 24],
        ];
        // Steps include 0 and values at near-black and near-white, and a few out-of-range inputs.
        let levels = [0.0, 1e-6, 1e-4, 0.002, 0.02, 0.1, 0.3, 0.6, 0.9, 0.999, 1.0];
        for s in &settings {
            let p = Params::from_sliders(s);
            for &r in &levels {
                for &g in &levels {
                    for &b in &levels {
                        let out = apply([r, g, b], &p);
                        assert!(out.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)), "{:?} → {out:?}", [r, g, b]);
                    }
                }
            }
            for bad in [[-0.2, 0.5, 1.4], [f32::NAN, 0.2, 0.3], [2.0, 2.0, 2.0], [-1.0, -1.0, -1.0]] {
                let out = apply(bad, &p);
                assert!(out.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)), "{bad:?} → {out:?}");
            }
        }
    }

    #[test]
    fn range_effect_is_smooth_between_centres() {
        // Orange Saturation +100: the factor on the neighbours falls off with the weights, with
        // no jump anywhere along the hue circle.
        let p = Params::from_sliders(&sliders(1, 0.0, 100.0, 0.0));
        let mut last = None::<f32>;
        for k in 0..720 {
            let h = k as f32 * 0.5;
            let c0 = 0.12;
            let colour = from_lch(0.6, c0, h);
            if !in_gamut(colour) {
                last = None;
                continue;
            }
            let ratio = to_lch(apply(colour, &p))[1] / to_lch(colour)[1];
            if let Some(prev) = last {
                assert!((ratio - prev).abs() < 0.12, "jump at {h}: {prev} → {ratio}");
            }
            last = Some(ratio);
        }
    }
}
