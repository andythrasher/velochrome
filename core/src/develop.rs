//! The basic adjustments and the CPU side of the white balance math: a color temperature and tint
//! in, camera multipliers out. The tone sliders are in `tone.rs`; the per-pixel work is in
//! `shaders/develop.hlsl`.

use crate::color::{self, Mat3};

/// A photo's adjustments. All zero (with `custom_white_balance` off) is the photo as shot.
/// Sliders follow Lightroom's ranges.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edit {
    /// Stops, −5 to +5.
    pub exposure: f32,
    /// 0 = as shot; otherwise `temperature` and `tint` apply.
    pub custom_white_balance: u32,
    /// Kelvin, 2000–50000.
    pub temperature: f32,
    /// −150 (green) to +150 (magenta).
    pub tint: f32,
    /// The rest are −100 to +100.
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub vibrance: f32,
    pub saturation: f32,
    /// Quarter turns clockwise (0–3), on top of the camera's orientation.
    pub rotation: u32,
    /// Straighten, in degrees clockwise, −45 to 45 (see `geometry.rs`).
    pub angle: f32,
    /// Left, top, right and bottom of the crop, as fractions of the frame. [0, 0, 1, 1] is none.
    pub crop: [f32; 4],
    /// Manual lens vignetting, −100 (darker corners) to +100 (brighter), 0 = none.
    pub vignette_amount: f32,
    /// The vignetting midpoint (0–100, 50 by default) as an offset from 50, so a zeroed `Edit`
    /// holds the default. Lower reaches further in from the corners.
    pub vignette_midpoint: f32,
    /// 1 turns off the camera's distortion correction; 0 (the default) applies it.
    pub no_distortion: u32,
    /// 1 turns off the camera's chromatic aberration correction.
    pub no_chromatic_aberration: u32,
    // Detail (docs/phase2b.md). Each is the slider's value as an offset from its default (the
    // `DETAIL_DEFAULTS`), so a zeroed `Edit` holds the defaults: sharpening on at 40, colour
    // noise reduction at 25.
    pub sharpen_amount: f32,
    pub sharpen_radius: f32,
    pub sharpen_detail: f32,
    pub sharpen_masking: f32,
    pub noise_luminance: f32,
    pub noise_luminance_detail: f32,
    pub noise_color: f32,
    pub noise_color_detail: f32,
    /// Texture and Clarity, −100 to +100 (0 = none, so no offset is needed).
    pub texture: f32,
    pub clarity: f32,
    /// The colour mixer (docs/phase2c.md), −100 to +100 each, 0 = none: hue for Red, Orange, Yellow,
    /// Green, Aqua, Blue, Purple, Magenta, then their saturations, then their luminances.
    pub mixer: [f32; MIXER_FIELDS],
}

/// Sliders in the colour mixer: three for each of the eight ranges.
pub const MIXER_FIELDS: usize = 3 * crate::mixer::RANGES;

/// No crop.
pub const FULL_CROP: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

impl Default for Edit {
    fn default() -> Self {
        Edit {
            exposure: 0.0,
            custom_white_balance: 0,
            temperature: 0.0,
            tint: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            vibrance: 0.0,
            saturation: 0.0,
            rotation: 0,
            angle: 0.0,
            crop: FULL_CROP,
            vignette_amount: 0.0,
            vignette_midpoint: 0.0,
            no_distortion: 0,
            no_chromatic_aberration: 0,
            sharpen_amount: 0.0,
            sharpen_radius: 0.0,
            sharpen_detail: 0.0,
            sharpen_masking: 0.0,
            noise_luminance: 0.0,
            noise_luminance_detail: 0.0,
            noise_color: 0.0,
            noise_color_detail: 0.0,
            texture: 0.0,
            clarity: 0.0,
            mixer: [0.0; MIXER_FIELDS],
        }
    }
}

impl Edit {
    pub fn is_default(&self) -> bool {
        *self == Edit::default()
    }

    /// Puts the lens fields in range: what a hand-edited or damaged sidecar might hold.
    pub fn clean_lens(&mut self) {
        let fix = |v: f32, limit: f32| if v.is_finite() { v.clamp(-limit, limit) } else { 0.0 };
        self.vignette_amount = fix(self.vignette_amount, 100.0);
        self.vignette_midpoint = fix(self.vignette_midpoint, 50.0);
        self.no_distortion = (self.no_distortion != 0) as u32;
        self.no_chromatic_aberration = (self.no_chromatic_aberration != 0) as u32;
    }

    /// Puts the detail fields in range (a damaged sidecar, or a wrapper's wild value): each
    /// slider's value stays inside its range.
    pub fn clean_detail(&mut self) {
        let fix = |offset: f32, default: f32, min: f32, max: f32| {
            if offset.is_finite() { (default + offset).clamp(min, max) - default } else { 0.0 }
        };
        let d = DETAIL_DEFAULTS;
        self.sharpen_amount = fix(self.sharpen_amount, d.amount, 0.0, 150.0);
        self.sharpen_radius = fix(self.sharpen_radius, d.radius, 0.5, 3.0);
        self.sharpen_detail = fix(self.sharpen_detail, d.detail, 0.0, 100.0);
        self.sharpen_masking = fix(self.sharpen_masking, d.masking, 0.0, 100.0);
        self.noise_luminance = fix(self.noise_luminance, d.luminance, 0.0, 100.0);
        self.noise_luminance_detail = fix(self.noise_luminance_detail, d.luminance_detail, 0.0, 100.0);
        self.noise_color = fix(self.noise_color, d.color, 0.0, 100.0);
        self.noise_color_detail = fix(self.noise_color_detail, d.color_detail, 0.0, 100.0);
        self.texture = fix(self.texture, 0.0, -100.0, 100.0);
        self.clarity = fix(self.clarity, 0.0, -100.0, 100.0);
    }

    /// Puts the mixer sliders in range.
    pub fn clean_mixer(&mut self) {
        for v in &mut self.mixer {
            *v = if v.is_finite() { v.clamp(-100.0, 100.0) } else { 0.0 };
        }
    }

    /// The detail sliders as the user sees them (absolute values).
    pub fn detail(&self) -> Detail {
        let d = DETAIL_DEFAULTS;
        Detail {
            amount: d.amount + self.sharpen_amount,
            radius: d.radius + self.sharpen_radius,
            detail: d.detail + self.sharpen_detail,
            masking: d.masking + self.sharpen_masking,
            luminance: d.luminance + self.noise_luminance,
            luminance_detail: d.luminance_detail + self.noise_luminance_detail,
            color: d.color + self.noise_color,
            color_detail: d.color_detail + self.noise_color_detail,
            texture: self.texture,
        }
    }

    /// Sets the detail sliders from absolute values.
    pub fn set_detail(&mut self, v: Detail) {
        let d = DETAIL_DEFAULTS;
        self.sharpen_amount = v.amount - d.amount;
        self.sharpen_radius = v.radius - d.radius;
        self.sharpen_detail = v.detail - d.detail;
        self.sharpen_masking = v.masking - d.masking;
        self.noise_luminance = v.luminance - d.luminance;
        self.noise_luminance_detail = v.luminance_detail - d.luminance_detail;
        self.noise_color = v.color - d.color;
        self.noise_color_detail = v.color_detail - d.color_detail;
        self.texture = v.texture;
    }
}

/// The detail sliders: sharpening (Amount 0–150, Radius 0.5–3 pixels, Detail 0–100, Masking
/// 0–100) and noise reduction (Luminance and Color 0–100, each with a Detail 0–100).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detail {
    pub amount: f32,
    pub radius: f32,
    pub detail: f32,
    pub masking: f32,
    pub luminance: f32,
    pub luminance_detail: f32,
    pub color: f32,
    pub color_detail: f32,
    /// Texture, −100 to 100.
    pub texture: f32,
}

/// Lightroom's defaults for a raw: sharpening 40 / 1.0 / 25 / 0, no luminance noise reduction,
/// colour 25, both Details 50.
pub const DETAIL_DEFAULTS: Detail = Detail {
    amount: 40.0,
    radius: 1.0,
    detail: 25.0,
    masking: 0.0,
    luminance: 0.0,
    luminance_detail: 50.0,
    color: 25.0,
    color_detail: 50.0,
    texture: 0.0,
};

impl Detail {
    /// Nothing for the detail pass to do: no sharpening, noise reduction or texture.
    pub fn is_neutral(&self) -> bool {
        self.amount <= 0.0 && self.luminance <= 0.0 && self.color <= 0.0 && self.texture == 0.0
    }
}

// ---- Vignetting ----

/// What the shader gets for the vignetting: stops at the corners (±1 for ±100), and the radius
/// where the change starts. That moves out toward the corners as the midpoint rises (as in
/// Lightroom: a lower midpoint reaches further in); 0.9 at most, so the ramp keeps some width.
pub fn vignette_terms(amount: f32, midpoint_offset: f32) -> (f32, f32) {
    (amount / 100.0, 0.9 * ((50.0 + midpoint_offset) / 100.0).clamp(0.0, 1.0))
}

/// The vignetting gain in stops at normalized radius `r` (the lens-corrected frame's corners
/// are at 1). The CPU twin of `vignetteStops` in shaders/lens.hlsli.
pub fn vignette_stops(amount: f32, midpoint_offset: f32, r: f32) -> f32 {
    let (scale, r0) = vignette_terms(amount, midpoint_offset);
    if scale == 0.0 {
        return 0.0;
    }
    let t = ((r - r0) / (1.0 - r0)).clamp(0.0, 1.0);
    scale * (t * t * (3.0 - 2.0 * t)).powf(1.5)
}

// ---- White balance ----

/// The Planckian locus in CIE 1960 (u, v), Krystek's approximation (1000–15000 K, and close
/// enough beyond for a white balance slider).
pub fn planck_uv(kelvin: f32) -> (f32, f32) {
    let t = kelvin as f64;
    let u = (0.860117757 + 1.54118254e-4 * t + 1.28641212e-7 * t * t) / (1.0 + 8.42420235e-4 * t + 7.08145163e-7 * t * t);
    let v = (0.317398726 + 4.22806245e-5 * t + 4.20481691e-8 * t * t) / (1.0 - 2.89741816e-5 * t + 1.61456053e-7 * t * t);
    (u as f32, v as f32)
}

/// Unit normal to the locus at `kelvin`, pointing to the green side.
fn locus_normal(kelvin: f32) -> (f32, f32) {
    let (u0, v0) = planck_uv(kelvin * 0.999);
    let (u1, v1) = planck_uv(kelvin * 1.001);
    let (tu, tv) = (u1 - u0, v1 - v0);
    let len = (tu * tu + tv * tv).sqrt();
    let n = (-tv / len, tu / len);
    if n.1 >= 0.0 { n } else { (-n.0, -n.1) }
}

/// Lightroom-like tint units per unit of distance from the locus (Duv). As in Lightroom, a
/// positive tint is a white point on the green side of the locus (fluorescent light, say), which
/// the correction turns magenta; negative, the reverse.
const TINT_SCALE: f32 = 3000.0;

/// The white point for a temperature and tint, in CIE 1960 (u, v).
pub fn white_uv(kelvin: f32, tint: f32) -> (f32, f32) {
    let (u, v) = planck_uv(kelvin);
    let (nu, nv) = locus_normal(kelvin);
    let d = tint / TINT_SCALE;
    (u + nu * d, v + nv * d)
}

/// The temperature and tint of a white point given in (u, v): the nearest point on the locus
/// (found by golden-section search over log temperature) and the signed distance from it.
pub fn temperature_tint(u: f32, v: f32) -> (f32, f32) {
    let dist = |log_t: f32| {
        let (pu, pv) = planck_uv(log_t.exp());
        (pu - u).powi(2) + (pv - v).powi(2)
    };
    let (mut a, mut b) = (1000f32.ln(), 60000f32.ln());
    let g = 0.618_034f32;
    for _ in 0..60 {
        let c = b - g * (b - a);
        let d = a + g * (b - a);
        if dist(c) < dist(d) { b = d } else { a = c }
    }
    let kelvin = ((a + b) / 2.0).exp();
    let (pu, pv) = planck_uv(kelvin);
    let (nu, nv) = locus_normal(kelvin);
    let duv = (u - pu) * nu + (v - pv) * nv;
    (kelvin, duv * TINT_SCALE)
}

fn uv_to_xyz(u: f32, v: f32) -> [f32; 3] {
    let d = 2.0 * u - 8.0 * v + 4.0;
    let (x, y) = (3.0 * u / d, 2.0 * v / d);
    [x / y, 1.0, (1.0 - x - y) / y]
}

fn xyz_to_uv(xyz: [f32; 3]) -> (f32, f32) {
    let d = xyz[0] + 15.0 * xyz[1] + 3.0 * xyz[2];
    (4.0 * xyz[0] / d, 6.0 * xyz[1] / d)
}

fn mul_vec(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

/// The temperature and tint a camera's white balance multipliers (R, G, B, G = 1) stand for.
pub fn multipliers_to_temperature(xyz_to_cam: &Mat3, wb: [f32; 3]) -> (f32, f32) {
    // A neutral surface reads as 1/multiplier in each camera channel.
    let neutral = [1.0 / wb[0], 1.0 / wb[1], 1.0 / wb[2]];
    let xyz = mul_vec(&color::invert(xyz_to_cam), neutral);
    let (u, v) = xyz_to_uv(xyz);
    temperature_tint(u, v)
}

/// The white balance multipliers (R, G, B, G = 1) for a temperature and tint.
pub fn temperature_to_multipliers(xyz_to_cam: &Mat3, kelvin: f32, tint: f32) -> [f32; 3] {
    let (u, v) = white_uv(kelvin, tint);
    let neutral = mul_vec(xyz_to_cam, uv_to_xyz(u, v));
    let wb = neutral.map(|n| 1.0 / n.max(1e-6));
    [wb[0] / wb[1], 1.0, wb[2] / wb[1]]
}

/// What to multiply as-shot white-balanced camera values by to get the edit's white balance.
pub fn white_balance_change(edit: &Edit, xyz_to_cam: &Mat3, as_shot: [f32; 3]) -> [f32; 3] {
    if edit.custom_white_balance == 0 {
        return [1.0; 3];
    }
    let wanted = temperature_to_multipliers(xyz_to_cam, edit.temperature, edit.tint);
    [0, 1, 2].map(|i| wanted[i] / as_shot[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    const OM5_XYZ_TO_CAM: Mat3 = [[1.2008, -0.523, -0.1123], [-0.2265, 1.0496, 0.2046], [0.0069, 0.0734, 0.4938]];

    #[test]
    fn no_crop_is_the_default() {
        let e = Edit::default();
        assert_eq!(e.crop, FULL_CROP);
        assert!(e.is_default());
        assert!(!Edit { rotation: 1, ..Default::default() }.is_default());
        assert!(!Edit { angle: 0.5, ..Default::default() }.is_default());
        assert!(!Edit { crop: [0.1, 0.0, 1.0, 1.0], ..Default::default() }.is_default());
        assert!(!Edit { vignette_amount: 10.0, ..Default::default() }.is_default());
        assert!(!Edit { no_distortion: 1, ..Default::default() }.is_default());
        // Struct update from a default keeps the whole-frame crop.
        assert!(Edit { exposure: 0.0, ..Default::default() }.is_default());
    }

    #[test]
    fn vignette_gain_shape() {
        // Nothing at 0, and nothing inside the start radius (0.45 at the default midpoint).
        assert_eq!(vignette_stops(0.0, 0.0, 1.0), 0.0);
        assert_eq!(vignette_stops(100.0, 0.0, 0.0), 0.0);
        assert_eq!(vignette_stops(100.0, 0.0, 0.4), 0.0);
        // One stop at the corners at ±100, in proportion between.
        assert!((vignette_stops(100.0, 0.0, 1.0) - 1.0).abs() < 1e-6);
        assert!((vignette_stops(-100.0, 0.0, 1.0) + 1.0).abs() < 1e-6);
        assert!((vignette_stops(50.0, 0.0, 1.0) - 0.5).abs() < 1e-6);
        // Rises with radius, and a higher midpoint keeps the change nearer the corners.
        let mut last = 0.0;
        for i in 0..=20 {
            let g = vignette_stops(100.0, 0.0, i as f32 / 20.0);
            assert!(g >= last - 1e-6, "{g} after {last}");
            last = g;
        }
        assert!(vignette_stops(100.0, -50.0, 0.7) > vignette_stops(100.0, 0.0, 0.7));
        assert!(vignette_stops(100.0, 0.0, 0.7) > vignette_stops(100.0, 50.0, 0.7));
    }

    #[test]
    fn clean_lens_repairs_damage() {
        let mut e = Edit { vignette_amount: 300.0, vignette_midpoint: f32::NAN, no_distortion: 7, ..Default::default() };
        e.clean_lens();
        assert_eq!((e.vignette_amount, e.vignette_midpoint, e.no_distortion, e.no_chromatic_aberration), (100.0, 0.0, 1, 0));
    }

    #[test]
    fn detail_defaults_are_a_zeroed_edit() {
        let e = Edit::default();
        assert_eq!(e.detail(), DETAIL_DEFAULTS);
        assert!(!e.detail().is_neutral(), "sharpening and colour noise reduction are on by default");
        let off = Detail { amount: 0.0, color: 0.0, ..DETAIL_DEFAULTS };
        let mut e = Edit::default();
        e.set_detail(off);
        assert!(e.detail().is_neutral() && !e.is_default());
        e.set_detail(DETAIL_DEFAULTS);
        assert!(e.is_default());
    }

    #[test]
    fn clean_detail_repairs_damage() {
        let mut e = Edit { sharpen_amount: 900.0, sharpen_radius: -9.0, noise_color: f32::NAN, noise_luminance: -50.0, ..Default::default() };
        e.clean_detail();
        let d = e.detail();
        assert_eq!((d.amount, d.radius, d.color, d.luminance), (150.0, 0.5, 25.0, 0.0));
    }

    #[test]
    fn temperature_round_trips() {
        for (kelvin, tint) in [(2800.0, 0.0), (5500.0, 10.0), (6500.0, -25.0), (12000.0, 40.0)] {
            let (u, v) = white_uv(kelvin, tint);
            let (k, t) = temperature_tint(u, v);
            assert!((k - kelvin).abs() / kelvin < 0.002, "{kelvin} K came back as {k}");
            assert!((t - tint).abs() < 0.5, "tint {tint} came back as {t}");
        }
    }

    #[test]
    fn daylight_is_about_d65() {
        // D65's (u, v) is (0.1978, 0.3122): about 6500 K, slightly above the locus (green), so a
        // small positive tint.
        let (k, t) = temperature_tint(0.1978, 0.3122);
        assert!((6400.0..6600.0).contains(&k), "{k}");
        assert!(t > 0.0 && t < 15.0, "{t}");
    }

    #[test]
    fn multipliers_round_trip() {
        // An OM-5 II daylight white balance (from its files).
        let wb = [2.046875, 1.0, 1.828125];
        let (k, t) = multipliers_to_temperature(&OM5_XYZ_TO_CAM, wb);
        assert!((3500.0..9000.0).contains(&k), "{k} K");
        let back = temperature_to_multipliers(&OM5_XYZ_TO_CAM, k, t);
        for i in 0..3 {
            assert!((back[i] - wb[i]).abs() < 0.01, "{back:?} vs {wb:?}");
        }
        // Warmer means less blue gain and more red, as on a camera.
        let warm = temperature_to_multipliers(&OM5_XYZ_TO_CAM, k + 1500.0, t);
        assert!(warm[2] < wb[2] && warm[0] > wb[0], "{warm:?}");
    }
}
