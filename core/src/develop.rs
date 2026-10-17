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
}

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
        }
    }
}

impl Edit {
    pub fn is_default(&self) -> bool {
        *self == Edit::default()
    }
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
        // Struct update from a default keeps the whole-frame crop.
        assert!(Edit { exposure: 0.0, ..Default::default() }.is_default());
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
