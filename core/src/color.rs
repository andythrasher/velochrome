//! Color math done on the CPU once per image; the per-pixel work happens in the shaders.
//!
//! The working space is linear Rec.2020: wide enough for anything a camera records that people
//! care about, and a standard that every platform's color management understands.

pub type Mat3 = [[f32; 3]; 3];

/// Linear Rec.2020 to CIE XYZ, D65 white.
pub const REC2020_TO_XYZ: Mat3 = [
    [0.636_958, 0.144_616_9, 0.168_881],
    [0.262_700_2, 0.677_998_1, 0.059_301_7],
    [0.0, 0.028_072_7, 1.060_985_1],
];

/// Linear Rec.2020 to linear Rec.709/sRGB primaries.
pub const REC2020_TO_SRGB: Mat3 = [
    [1.660_491, -0.587_641_1, -0.072_849_9],
    [-0.124_550_5, 1.132_899_9, -0.008_349_4],
    [-0.018_150_8, -0.100_578_9, 1.118_729_7],
];

/// Linear ProPhoto RGB (ROMM RGB) to CIE XYZ, D50 white (DNG specification).
pub const PROPHOTO_TO_XYZ_D50: Mat3 = [
    [0.797_674_9, 0.135_191_7, 0.031_353_4],
    [0.288_040_2, 0.711_874_1, 0.000_085_7],
    [0.0, 0.0, 0.825_210_0],
];

/// CIE XYZ (D50 white) to linear ProPhoto RGB.
pub const XYZ_D50_TO_PROPHOTO: Mat3 = [
    [1.345_943_3, -0.255_607_5, -0.051_111_8],
    [-0.544_598_9, 1.508_167_3, 0.020_535_1],
    [0.0, 0.0, 1.211_812_8],
];

/// Linear ProPhoto RGB (D50) to linear Rec.2020 (D65), Bradford-adapted.
pub const PROPHOTO_TO_REC2020: Mat3 = [
    [1.212_101_2, -0.146_916_8, -0.065_184_4],
    [0.029_258_2, 0.960_903_2, 0.009_838_6],
    [-0.000_578_8, -0.029_442_0, 1.030_020_8],
];

/// Linear ProPhoto RGB (D50) to linear sRGB (D65), Bradford-adapted (`REC2020_TO_SRGB * PROPHOTO_TO_REC2020`).
pub const PROPHOTO_TO_SRGB: Mat3 = [
    [2.034_194_5, -0.727_420_8, -0.306_773_7],
    [-0.228_811_2, 1.231_729_5, -0.002_918_3],
    [-0.008_565_2, -0.153_268_6, 1.161_833_8],
];

pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0f32; 3]; 3];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

pub fn invert(m: &Mat3) -> Mat3 {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let co = [e * i - f * h, f * g - d * i, d * h - e * g];
    let det = a * co[0] + b * co[1] + c * co[2];
    let s = 1.0 / det;
    [
        [co[0] * s, (c * h - b * i) * s, (b * f - c * e) * s],
        [co[1] * s, (a * i - c * g) * s, (c * d - a * f) * s],
        [co[2] * s, (b * g - a * h) * s, (a * e - b * d) * s],
    ]
}

/// From the camera's XYZ→camera matrix (as published in Adobe's DNG profiles) to a matrix taking
/// white-balanced camera RGB to the working space. Normalizing the rows of working→camera means
/// a neutral surface, once white balanced, lands exactly on the working space's white.
pub fn camera_to_working(xyz_to_cam: Mat3) -> Mat3 {
    let mut cam_from_working = mul(&xyz_to_cam, &REC2020_TO_XYZ);
    for row in cam_from_working.iter_mut() {
        let sum: f32 = row.iter().sum();
        for v in row.iter_mut() {
            *v /= sum;
        }
    }
    invert(&cam_from_working)
}

/// From a DCP `ForwardMatrix` (white-balanced camera RGB → XYZ D50) to linear ProPhoto RGB, with
/// rows normalized so neutral `[1, 1, 1]` stays `[1, 1, 1]` despite 1/10000 SRATIONAL rounding.
pub fn forward_to_prophoto(forward: Mat3) -> Mat3 {
    let mut m = mul(&XYZ_D50_TO_PROPHOTO, &forward);
    for row in m.iter_mut() {
        let sum: f32 = row.iter().sum();
        if sum.abs() > 1e-6 {
            for v in row.iter_mut() {
                *v /= sum;
            }
        }
    }
    m
}

/// Fallback when a DCP has `ColorMatrix` (XYZ → camera) but no `ForwardMatrix`: takes
/// white-balanced camera RGB to linear ProPhoto RGB.
pub fn camera_to_prophoto(xyz_to_cam: Mat3) -> Mat3 {
    let mut cam_from_prophoto = mul(&xyz_to_cam, &PROPHOTO_TO_XYZ_D50);
    for row in cam_from_prophoto.iter_mut() {
        let sum: f32 = row.iter().sum();
        if sum.abs() > 1e-6 {
            for v in row.iter_mut() {
                *v /= sum;
            }
        }
    }
    invert(&cam_from_prophoto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_stays_white() {
        let xyz_to_cam = [[1.2008, -0.523, -0.1123], [-0.2265, 1.0496, 0.2046], [0.0069, 0.0734, 0.4938]];
        let m = camera_to_working(xyz_to_cam);
        for row in m {
            let sum: f32 = row.iter().sum();
            assert!((sum - 1.0).abs() < 1e-4, "{sum}");
        }
    }

    #[test]
    fn prophoto_round_trips() {
        let id = mul(&XYZ_D50_TO_PROPHOTO, &PROPHOTO_TO_XYZ_D50);
        for (r, row) in id.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                assert!((v - if r == c { 1.0 } else { 0.0 }).abs() < 1e-4);
            }
        }
        for row in PROPHOTO_TO_SRGB {
            let sum: f32 = row.iter().sum();
            assert!((sum - 1.0).abs() < 1e-5, "{sum}");
        }
    }

    #[test]
    fn invert_round_trips() {
        let p = mul(&REC2020_TO_XYZ, &invert(&REC2020_TO_XYZ));
        for (r, row) in p.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                assert!((v - if r == c { 1.0 } else { 0.0 }).abs() < 1e-5);
            }
        }
    }
}
