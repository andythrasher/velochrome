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
    fn invert_round_trips() {
        let p = mul(&REC2020_TO_XYZ, &invert(&REC2020_TO_XYZ));
        for (r, row) in p.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                assert!((v - if r == c { 1.0 } else { 0.0 }).abs() < 1e-5);
            }
        }
    }
}
