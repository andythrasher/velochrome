//! Adobe DNG Camera Profiles (`.dcp`) and the profile registry (`docs/camera-profiles.md`).
//!
//! A `.dcp` file is a TIFF-like container (`IIRC` little-endian or `MMCR` big-endian) whose single
//! IFD carries DNG camera profile tags: two calibration illuminants, `ForwardMatrix` and
//! `ColorMatrix` pairs, a 3-D `HueSatMap` (one per illuminant), a 3-D `LookTable`, a 1-D
//! `ProfileToneCurve`, and `BaselineExposureOffset`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::color::{self, Mat3};
use crate::tone;

const TAG_UNIQUE_CAMERA_MODEL: u16 = 0xC614;
const TAG_COLOR_MATRIX_1: u16 = 0xC621;
const TAG_COLOR_MATRIX_2: u16 = 0xC622;
const TAG_CALIBRATION_ILLUMINANT_1: u16 = 0xC65A;
const TAG_CALIBRATION_ILLUMINANT_2: u16 = 0xC65B;
const TAG_PROFILE_NAME: u16 = 0xC6F8;
const TAG_HUE_SAT_MAP_DIMS: u16 = 0xC6F9;
const TAG_HUE_SAT_MAP_DATA_1: u16 = 0xC6FA;
const TAG_HUE_SAT_MAP_DATA_2: u16 = 0xC6FB;
const TAG_PROFILE_TONE_CURVE: u16 = 0xC6FC;
const TAG_FORWARD_MATRIX_1: u16 = 0xC714;
const TAG_FORWARD_MATRIX_2: u16 = 0xC715;
const TAG_LOOK_TABLE_DIMS: u16 = 0xC725;
const TAG_LOOK_TABLE_DATA: u16 = 0xC726;
const TAG_HUE_SAT_MAP_ENCODING: u16 = 0xC7A3;
const TAG_LOOK_TABLE_ENCODING: u16 = 0xC7A4;
const TAG_BASELINE_EXPOSURE_OFFSET: u16 = 0xC7A5;
const TAG_DEFAULT_BLACK_RENDER: u16 = 0xC7A6;

pub const TONE_CURVE_LUT_LEN: usize = 4096;

/// A parsed DNG camera profile (`.dcp`).
#[derive(Debug, Clone)]
pub struct Profile {
    pub unique_camera_model: String,
    pub profile_name: String,
    pub illuminant1: u16,
    pub illuminant2: u16,
    pub color_matrix1: Option<Mat3>,
    pub color_matrix2: Option<Mat3>,
    pub forward_matrix1: Option<Mat3>,
    pub forward_matrix2: Option<Mat3>,
    /// `[hue_divs, sat_divs, val_divs]`, or `[0, 0, 0]` when the profile has no `HueSatMap`.
    pub hue_sat_dims: [u32; 3],
    /// `0` = linear V, `1` = sRGB-encoded V.
    pub hue_sat_encoding: u32,
    /// `[hue_shift_in_sectors (deg / 60), sat_scale, val_scale, 0.0]`, indexed `(v * H + h) * S + s`.
    pub hue_sat_data1: Vec<[f32; 4]>,
    pub hue_sat_data2: Vec<[f32; 4]>,
    /// `[hue_divs, sat_divs, val_divs]`, or `[0, 0, 0]` when the profile has no `LookTable`.
    pub look_dims: [u32; 3],
    /// `0` = linear V, `1` = sRGB-encoded V.
    pub look_encoding: u32,
    /// `[hue_shift_in_sectors (deg / 60), sat_scale, val_scale, 0.0]`, indexed `(v * H + h) * S + s`.
    pub look_data: Vec<[f32; 4]>,
    /// Control points `(x, y)` in `0..1`, empty when the profile uses the default curve.
    pub tone_curve_points: Vec<[f32; 2]>,
    /// 4096-entry LUT evaluated from `tone_curve_points` with a monotone cubic spline, or `None`
    /// when the profile has no `ProfileToneCurve` (e.g. `Adobe Standard`).
    pub tone_curve_lut: Option<Box<[f32; TONE_CURVE_LUT_LEN]>>,
    pub baseline_exposure_offset: f32,
    pub default_black_render: u32,
}

#[derive(Clone, Copy)]
struct Endian {
    le: bool,
}

impl Endian {
    fn u16(self, b: &[u8], off: usize) -> Option<u16> {
        let s: [u8; 2] = b.get(off..off + 2)?.try_into().ok()?;
        Some(if self.le { u16::from_le_bytes(s) } else { u16::from_be_bytes(s) })
    }

    fn u32(self, b: &[u8], off: usize) -> Option<u32> {
        let s: [u8; 4] = b.get(off..off + 4)?.try_into().ok()?;
        Some(if self.le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
    }

    fn i32(self, b: &[u8], off: usize) -> Option<i32> {
        let s: [u8; 4] = b.get(off..off + 4)?.try_into().ok()?;
        Some(if self.le { i32::from_le_bytes(s) } else { i32::from_be_bytes(s) })
    }

    fn f32(self, b: &[u8], off: usize) -> Option<f32> {
        let s: [u8; 4] = b.get(off..off + 4)?.try_into().ok()?;
        Some(if self.le { f32::from_le_bytes(s) } else { f32::from_be_bytes(s) })
    }
}

fn type_size(typ: u16) -> usize {
    match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

fn parse_header(b: &[u8]) -> Option<(Endian, usize)> {
    if b.len() < 8 {
        return None;
    }
    let le = match &b[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let e = Endian { le };
    let magic = e.u16(b, 2)?;
    if magic != 0x4352 && magic != 0x002A {
        return None;
    }
    let ifd_off = e.u32(b, 4)? as usize;
    Some((e, ifd_off))
}

fn entry_slice(b: &[u8], entry_off: usize, typ: u16, count: usize, e: Endian) -> Option<&[u8]> {
    let total = count.checked_mul(type_size(typ))?;
    let data_off = if total <= 4 { entry_off + 8 } else { e.u32(b, entry_off + 8)? as usize };
    b.get(data_off..data_off.checked_add(total)?)
}

fn read_ascii(b: &[u8], entry_off: usize, typ: u16, count: usize, e: Endian) -> Option<String> {
    let raw = entry_slice(b, entry_off, typ, count, e)?;
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Some(String::from_utf8_lossy(&raw[..end]).trim().to_string())
}

fn read_u32_item(b: &[u8], entry_off: usize, typ: u16, count: usize, idx: usize, e: Endian) -> Option<u32> {
    if idx >= count {
        return None;
    }
    let raw = entry_slice(b, entry_off, typ, count, e)?;
    match typ {
        3 => Some(e.u16(raw, idx * 2)? as u32),
        4 | 9 => Some(e.u32(raw, idx * 4)?),
        _ => None,
    }
}

fn read_srational(b: &[u8], entry_off: usize, typ: u16, count: usize, idx: usize, e: Endian) -> Option<f32> {
    if idx >= count {
        return None;
    }
    let raw = entry_slice(b, entry_off, typ, count, e)?;
    match typ {
        10 => {
            let num = e.i32(raw, idx * 8)? as f32;
            let den = e.i32(raw, idx * 8 + 4)? as f32;
            (den != 0.0).then_some(num / den)
        }
        5 => {
            let num = e.u32(raw, idx * 8)? as f32;
            let den = e.u32(raw, idx * 8 + 4)? as f32;
            (den != 0.0).then_some(num / den)
        }
        11 => e.f32(raw, idx * 4),
        _ => None,
    }
}

fn read_mat3(b: &[u8], entry_off: usize, typ: u16, count: usize, e: Endian) -> Option<Mat3> {
    if count < 9 {
        return None;
    }
    let mut m = [[0.0f32; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            m[r][c] = read_srational(b, entry_off, typ, count, r * 3 + c, e)?;
        }
    }
    Some(m)
}

fn read_hsv_table(b: &[u8], entry_off: usize, typ: u16, count: usize, e: Endian) -> Option<Vec<[f32; 4]>> {
    if typ != 11 || !count.is_multiple_of(3) {
        return None;
    }
    let raw = entry_slice(b, entry_off, typ, count, e)?;
    let triples = count / 3;
    let mut out = Vec::with_capacity(triples);
    for i in 0..triples {
        let base = i * 12;
        let dh = e.f32(raw, base)? / 60.0;
        let ss = e.f32(raw, base + 4)?;
        let vs = e.f32(raw, base + 8)?;
        out.push([dh, ss, vs, 0.0]);
    }
    Some(out)
}

/// Reads only `(UniqueCameraModel, ProfileName)` from the IFD of a `.dcp` file, without reading
/// the 3-D tables.
pub fn read_header_tags(path: &Path) -> Option<(String, String)> {
    let mut f = File::open(path).ok()?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    let (_e, ifd_off) = parse_header(&buf)?;
    if ifd_off + 2 > buf.len() {
        let full = fs::read(path).ok()?;
        return read_header_tags_from_bytes(&full);
    }
    read_header_tags_from_bytes(&buf).or_else(|| {
        let full = fs::read(path).ok()?;
        read_header_tags_from_bytes(&full)
    })
}

fn read_header_tags_from_bytes(b: &[u8]) -> Option<(String, String)> {
    let (e, ifd_off) = parse_header(b)?;
    let entries = e.u16(b, ifd_off)? as usize;
    let mut camera = String::new();
    let mut name = String::new();
    for i in 0..entries {
        let off = ifd_off + 2 + i * 12;
        let tag = e.u16(b, off)?;
        let typ = e.u16(b, off + 2)?;
        let count = e.u32(b, off + 4)? as usize;
        if tag == TAG_UNIQUE_CAMERA_MODEL {
            camera = read_ascii(b, off, typ, count, e)?;
        } else if tag == TAG_PROFILE_NAME {
            name = read_ascii(b, off, typ, count, e)?;
        }
    }
    (!name.is_empty()).then_some((camera, name))
}

impl Profile {
    pub fn from_file(path: &Path) -> Result<Profile, String> {
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(b: &[u8]) -> Result<Profile, String> {
        let (e, ifd_off) = parse_header(b).ok_or("not a valid DCP header")?;
        let entries = e.u16(b, ifd_off).ok_or("truncated DCP IFD")? as usize;

        let mut unique_camera_model = String::new();
        let mut profile_name = String::new();
        let mut illuminant1 = 21u16; // D65 default
        let mut illuminant2 = 0u16;
        let mut color_matrix1 = None;
        let mut color_matrix2 = None;
        let mut forward_matrix1 = None;
        let mut forward_matrix2 = None;
        let mut hue_sat_dims = [0u32; 3];
        let mut hue_sat_encoding = 0u32;
        let mut hue_sat_data1 = Vec::new();
        let mut hue_sat_data2 = Vec::new();
        let mut look_dims = [0u32; 3];
        let mut look_encoding = 0u32;
        let mut look_data = Vec::new();
        let mut tone_curve_points = Vec::new();
        let mut baseline_exposure_offset = 0.0f32;
        let mut default_black_render = 0u32;

        for i in 0..entries {
            let off = ifd_off + 2 + i * 12;
            let tag = e.u16(b, off).ok_or("truncated IFD entry")?;
            let typ = e.u16(b, off + 2).ok_or("truncated IFD entry")?;
            let count = e.u32(b, off + 4).ok_or("truncated IFD entry")? as usize;

            match tag {
                TAG_UNIQUE_CAMERA_MODEL => {
                    unique_camera_model = read_ascii(b, off, typ, count, e).ok_or("bad UniqueCameraModel")?;
                }
                TAG_PROFILE_NAME => {
                    profile_name = read_ascii(b, off, typ, count, e).ok_or("bad ProfileName")?;
                }
                TAG_CALIBRATION_ILLUMINANT_1 => {
                    illuminant1 = read_u32_item(b, off, typ, count, 0, e).ok_or("bad CalibrationIlluminant1")? as u16;
                }
                TAG_CALIBRATION_ILLUMINANT_2 => {
                    illuminant2 = read_u32_item(b, off, typ, count, 0, e).ok_or("bad CalibrationIlluminant2")? as u16;
                }
                TAG_COLOR_MATRIX_1 => {
                    color_matrix1 = Some(read_mat3(b, off, typ, count, e).ok_or("bad ColorMatrix1")?);
                }
                TAG_COLOR_MATRIX_2 => {
                    color_matrix2 = Some(read_mat3(b, off, typ, count, e).ok_or("bad ColorMatrix2")?);
                }
                TAG_FORWARD_MATRIX_1 => {
                    forward_matrix1 = Some(read_mat3(b, off, typ, count, e).ok_or("bad ForwardMatrix1")?);
                }
                TAG_FORWARD_MATRIX_2 => {
                    forward_matrix2 = Some(read_mat3(b, off, typ, count, e).ok_or("bad ForwardMatrix2")?);
                }
                TAG_HUE_SAT_MAP_DIMS if count >= 3 => {
                    hue_sat_dims = [
                        read_u32_item(b, off, typ, count, 0, e).ok_or("bad HueSatMapDims")?,
                        read_u32_item(b, off, typ, count, 1, e).ok_or("bad HueSatMapDims")?,
                        read_u32_item(b, off, typ, count, 2, e).ok_or("bad HueSatMapDims")?,
                    ];
                }
                TAG_HUE_SAT_MAP_DATA_1 => {
                    hue_sat_data1 = read_hsv_table(b, off, typ, count, e).ok_or("bad HueSatMapData1")?;
                }
                TAG_HUE_SAT_MAP_DATA_2 => {
                    hue_sat_data2 = read_hsv_table(b, off, typ, count, e).ok_or("bad HueSatMapData2")?;
                }
                TAG_HUE_SAT_MAP_ENCODING => {
                    hue_sat_encoding = read_u32_item(b, off, typ, count, 0, e).ok_or("bad HueSatMapEncoding")?;
                }
                TAG_LOOK_TABLE_DIMS if count >= 3 => {
                    look_dims = [
                        read_u32_item(b, off, typ, count, 0, e).ok_or("bad LookTableDims")?,
                        read_u32_item(b, off, typ, count, 1, e).ok_or("bad LookTableDims")?,
                        read_u32_item(b, off, typ, count, 2, e).ok_or("bad LookTableDims")?,
                    ];
                }
                TAG_LOOK_TABLE_DATA => {
                    look_data = read_hsv_table(b, off, typ, count, e).ok_or("bad LookTableData")?;
                }
                TAG_LOOK_TABLE_ENCODING => {
                    look_encoding = read_u32_item(b, off, typ, count, 0, e).ok_or("bad LookTableEncoding")?;
                }
                TAG_PROFILE_TONE_CURVE if typ == 11 && count >= 4 && count.is_multiple_of(2) => {
                    let raw = entry_slice(b, off, typ, count, e).ok_or("bad ProfileToneCurve")?;
                    let pts = count / 2;
                    tone_curve_points.reserve(pts);
                    for p in 0..pts {
                        let x = e.f32(raw, p * 8).ok_or("bad ProfileToneCurve")?;
                        let y = e.f32(raw, p * 8 + 4).ok_or("bad ProfileToneCurve")?;
                        tone_curve_points.push([x, y]);
                    }
                }
                TAG_BASELINE_EXPOSURE_OFFSET => {
                    baseline_exposure_offset =
                        read_srational(b, off, typ, count, 0, e).ok_or("bad BaselineExposureOffset")?;
                }
                TAG_DEFAULT_BLACK_RENDER => {
                    default_black_render = read_u32_item(b, off, typ, count, 0, e).ok_or("bad DefaultBlackRender")?;
                }
                _ => {}
            }
        }

        let expected_hs = (hue_sat_dims[0] * hue_sat_dims[1] * hue_sat_dims[2]) as usize;
        if expected_hs == 0 || hue_sat_data1.len() != expected_hs {
            hue_sat_dims = [0, 0, 0];
            hue_sat_data1.clear();
            hue_sat_data2.clear();
        }
        let expected_look = (look_dims[0] * look_dims[1] * look_dims[2]) as usize;
        if expected_look == 0 || look_data.len() != expected_look {
            look_dims = [0, 0, 0];
            look_data.clear();
        }

        let tone_curve_lut = if tone_curve_points.len() >= 2 {
            Some(Box::new(evaluate_spline_lut(&tone_curve_points)))
        } else {
            None
        };

        Ok(Profile {
            unique_camera_model,
            profile_name,
            illuminant1,
            illuminant2,
            color_matrix1,
            color_matrix2,
            forward_matrix1,
            forward_matrix2,
            hue_sat_dims,
            hue_sat_encoding,
            hue_sat_data1,
            hue_sat_data2,
            look_dims,
            look_encoding,
            look_data,
            tone_curve_points,
            tone_curve_lut,
            baseline_exposure_offset,
            default_black_render,
        })
    }

    /// Dual-illuminant interpolation weight `t` in `0..=1` (`0` = illuminant 1, `1` = illuminant 2),
    /// linear in mireds (`1e6 / K`) as specified by DNG.
    pub fn interpolation_weight(&self, temperature_k: f32) -> f32 {
        if self.illuminant2 == 0 {
            return 0.0;
        }
        let t1 = illuminant_kelvin(self.illuminant1);
        let t2 = illuminant_kelvin(self.illuminant2);
        let m1 = 1e6 / t1;
        let m2 = 1e6 / t2;
        if (m2 - m1).abs() < 1e-4 {
            return 0.0;
        }
        let m = 1e6 / temperature_k.clamp(1500.0, 25000.0);
        ((m - m1) / (m2 - m1)).clamp(0.0, 1.0)
    }

    /// Interpolates `ForwardMatrix1`/`2` (or `ColorMatrix1`/`2` if the profile has no forward
    /// matrix) for `temperature_k` and combines with white balance `wb` to map raw camera RGB to
    /// linear ProPhoto RGB (before exposure).
    pub fn cam_to_prophoto(&self, temperature_k: f32, wb: &[f32; 3], fallback_xyz_to_cam: &Mat3) -> Mat3 {
        let t = self.interpolation_weight(temperature_k);
        let base = match (self.forward_matrix1, self.forward_matrix2) {
            (Some(fm1), Some(fm2)) => color::forward_to_prophoto(lerp_mat(&fm1, &fm2, t)),
            (Some(fm1), None) => color::forward_to_prophoto(fm1),
            _ => {
                let cm = match (self.color_matrix1, self.color_matrix2) {
                    (Some(cm1), Some(cm2)) => lerp_mat(&cm1, &cm2, t),
                    (Some(cm1), None) => cm1,
                    _ => *fallback_xyz_to_cam,
                };
                color::camera_to_prophoto(cm)
            }
        };
        let mut out = [[0.0f32; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                out[i][j] = base[i][j] * wb[j];
            }
        }
        out
    }

    /// Interpolates `HueSatMapData1` and `HueSatMapData2` for `temperature_k`.
    pub fn interpolated_hue_sat(&self, temperature_k: f32) -> Vec<[f32; 4]> {
        if self.hue_sat_data1.is_empty() {
            return Vec::new();
        }
        if self.hue_sat_data2.len() != self.hue_sat_data1.len() {
            return self.hue_sat_data1.clone();
        }
        let t = self.interpolation_weight(temperature_k);
        if t <= 0.0 {
            return self.hue_sat_data1.clone();
        }
        if t >= 1.0 {
            return self.hue_sat_data2.clone();
        }
        self.hue_sat_data1
            .iter()
            .zip(&self.hue_sat_data2)
            .map(|(a, b)| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, 0.0])
            .collect()
    }

    /// Evaluates the profile's tone curve (or `tone::base_curve` when the profile has none) on
    /// linear input `s`.
    pub fn eval_curve(&self, s: f32) -> f32 {
        match &self.tone_curve_lut {
            Some(lut) => {
                if s <= 0.0 {
                    return lut[0];
                }
                if s >= 1.0 {
                    return lut[TONE_CURVE_LUT_LEN - 1];
                }
                let pos = s * (TONE_CURVE_LUT_LEN - 1) as f32;
                let i = pos as usize;
                let f = pos - i as f32;
                lut[i] + (lut[(i + 1).min(TONE_CURVE_LUT_LEN - 1)] - lut[i]) * f
            }
            None => tone::base_curve(s),
        }
    }

    /// Hue-preserving tone curve in ProPhoto RGB.
    pub fn tone_rgb(&self, pro: [f32; 3]) -> [f32; 3] {
        let c = pro.map(|v| v.max(0.0));
        let hi = c[0].max(c[1]).max(c[2]);
        let lo = c[0].min(c[1]).min(c[2]);
        if hi <= lo {
            let v = self.eval_curve(hi);
            return [v, v, v];
        }
        let hi_out = self.eval_curve(hi);
        let lo_out = self.eval_curve(lo);
        let scale = (hi_out - lo_out) / (hi - lo);
        c.map(|v| lo_out + (v - lo) * scale)
    }

    /// CPU reference for the profile develop stages (Steps 1–6 up to linear sRGB before mixer and
    /// presence):
    /// 1. `cam_to_pro * cam` → linear ProPhoto RGB
    /// 2. `HueSatMap` (before exposure)
    /// 3. Exposure (`exposure_gain = 2^(camera_offset + baseline_exposure_offset + exposure)`)
    /// 4. `LookTable` (after exposure)
    /// 5. Tone sliders (`tone_scale = 2^(shifted - x)`) + `ProfileToneCurve` (hue-preserving in ProPhoto RGB)
    /// 6. `PROPHOTO_TO_SRGB` → linear sRGB
    pub fn eval_pixel(
        &self,
        cam: [f32; 3],
        cam_to_pro: &Mat3,
        hue_sat: &[[f32; 4]],
        exposure_gain: f32,
        tone_scale: f32,
    ) -> [f32; 3] {
        let mut pro = [
            cam_to_pro[0][0] * cam[0] + cam_to_pro[0][1] * cam[1] + cam_to_pro[0][2] * cam[2],
            cam_to_pro[1][0] * cam[0] + cam_to_pro[1][1] * cam[1] + cam_to_pro[1][2] * cam[2],
            cam_to_pro[2][0] * cam[0] + cam_to_pro[2][1] * cam[1] + cam_to_pro[2][2] * cam[2],
        ];
        if self.hue_sat_dims[0] > 0 && !hue_sat.is_empty() {
            pro = apply_hsv_table(pro, hue_sat, self.hue_sat_dims, self.hue_sat_encoding);
        }
        pro = pro.map(|v| v * exposure_gain);
        if self.look_dims[0] > 0 && !self.look_data.is_empty() {
            pro = apply_hsv_table(pro, &self.look_data, self.look_dims, self.look_encoding);
        }
        pro = pro.map(|v| v * tone_scale);
        let curved = self.tone_rgb(pro);
        let m = &color::PROPHOTO_TO_SRGB;
        [
            m[0][0] * curved[0] + m[0][1] * curved[1] + m[0][2] * curved[2],
            m[1][0] * curved[0] + m[1][1] * curved[1] + m[1][2] * curved[2],
            m[2][0] * curved[0] + m[2][1] * curved[1] + m[2][2] * curved[2],
        ]
    }
}

fn lerp_mat(a: &Mat3, b: &Mat3, t: f32) -> Mat3 {
    let mut out = [[0.0f32; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            out[r][c] = a[r][c] + (b[r][c] - a[r][c]) * t;
        }
    }
    out
}

/// DNG `CalibrationIlluminant` tag values to correlated colour temperature in Kelvin.
pub fn illuminant_kelvin(code: u16) -> f32 {
    match code {
        1 => 5500.0,  // Daylight
        2 => 4200.0,  // Fluorescent
        3 => 2850.0,  // Tungsten
        4 => 5500.0,  // Flash
        9 => 5500.0,  // Fine weather
        10 => 6500.0, // Cloudy
        11 => 7500.0, // Shade
        12 => 6430.0, // Daylight fluorescent (D 5700 - 7100K)
        13 => 5000.0, // Day white fluorescent (N 4600 - 5400K)
        14 => 4200.0, // Cool white fluorescent (W 3900 - 4500K)
        15 => 3450.0, // White fluorescent (WW 3200 - 3700K)
        17 => 2850.0, // Standard light A
        18 => 4874.0, // Standard light B
        19 => 6774.0, // Standard light C
        20 => 5500.0, // D55
        21 => 6500.0, // D65
        22 => 7500.0, // D75
        23 => 5000.0, // D50
        24 => 3200.0, // ISO studio tungsten
        _ => 6500.0,
    }
}

fn srgb_encode_scalar(v: f32) -> f32 {
    let x = v.max(0.0);
    if x <= 0.003_130_8 { 12.92 * x } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 }
}

fn srgb_decode_scalar(v: f32) -> f32 {
    let x = v.max(0.0);
    if x <= 0.040_45 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) }
}

/// Applies a 3-D DNG HSV table (`HueSatMap` or `LookTable`) to linear ProPhoto RGB.
pub fn apply_hsv_table(rgb: [f32; 3], table: &[[f32; 4]], dims: [u32; 3], encoding: u32) -> [f32; 3] {
    let [r, g, b] = rgb.map(|c| c.max(0.0));
    let v = r.max(g).max(b);
    let m = r.min(g).min(b);
    let span = v - m;
    if span <= 1e-7 || v <= 1e-7 {
        // Achromatic pixels still get val_scale if val_divs > 1.
        if dims[2] <= 1 {
            return [r, g, b];
        }
        let v_enc = if encoding == 1 { srgb_encode_scalar(v) } else { v };
        let [_, _, vs] = sample_hsv_table(table, dims, 0.0, 0.0, v_enc.clamp(0.0, 1.0));
        let v_out = if encoding == 1 { srgb_decode_scalar(v_enc * vs) } else { v * vs };
        let scale = if v > 0.0 { v_out / v } else { 1.0 };
        return [r * scale, g * scale, b * scale];
    }

    let s = span / v;
    let mut h = if r == v {
        (g - b) / span
    } else if g == v {
        2.0 + (b - r) / span
    } else {
        4.0 + (r - g) / span
    };
    if h < 0.0 {
        h += 6.0;
    }

    let v_enc = if encoding == 1 { srgb_encode_scalar(v) } else { v };
    let [dh, ss, vs] = sample_hsv_table(table, dims, h, s.clamp(0.0, 1.0), v_enc.clamp(0.0, 1.0));

    let mut h_new = (h + dh) % 6.0;
    if h_new < 0.0 {
        h_new += 6.0;
    }
    let s_new = (s * ss).clamp(0.0, 1.0);
    let v_new = if encoding == 1 { srgb_decode_scalar(v_enc * vs) } else { (v * vs).max(0.0) };

    hsv_to_rgb(h_new, s_new, v_new)
}

fn sample_hsv_table(table: &[[f32; 4]], dims: [u32; 3], h: f32, s: f32, v: f32) -> [f32; 3] {
    let (h_divs, s_divs, v_divs) = (dims[0] as usize, dims[1] as usize, dims[2] as usize);
    let h_pos = (h * (h_divs as f32 / 6.0)).rem_euclid(h_divs as f32);
    let h0 = (h_pos as usize) % h_divs;
    let h1 = (h0 + 1) % h_divs;
    let fh = h_pos - h_pos.floor();

    let s_pos = (s * (s_divs.saturating_sub(1)) as f32).clamp(0.0, s_divs.saturating_sub(1) as f32);
    let s0 = s_pos as usize;
    let s1 = (s0 + 1).min(s_divs.saturating_sub(1));
    let fs = s_pos - s0 as f32;

    let (v0, v1, fv) = if v_divs <= 1 {
        (0, 0, 0.0)
    } else {
        let v_pos = (v * (v_divs - 1) as f32).clamp(0.0, (v_divs - 1) as f32);
        let v0 = v_pos as usize;
        let v1 = (v0 + 1).min(v_divs - 1);
        (v0, v1, v_pos - v0 as f32)
    };

    let at = |vi: usize, hi: usize, si: usize| -> [f32; 4] { table[(vi * h_divs + hi) * s_divs + si] };
    let lerp3 = |a: [f32; 4], b: [f32; 4], t: f32| -> [f32; 4] {
        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, 0.0]
    };

    let c00 = lerp3(at(v0, h0, s0), at(v0, h0, s1), fs);
    let c01 = lerp3(at(v0, h1, s0), at(v0, h1, s1), fs);
    let c0 = lerp3(c00, c01, fh);
    if v0 == v1 {
        return [c0[0], c0[1], c0[2]];
    }
    let c10 = lerp3(at(v1, h0, s0), at(v1, h0, s1), fs);
    let c11 = lerp3(at(v1, h1, s0), at(v1, h1, s1), fs);
    let c1 = lerp3(c10, c11, fh);
    let c = lerp3(c0, c1, fv);
    [c[0], c[1], c[2]]
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let i = h.floor() as i32;
    let f = h - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i.rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// Evaluates `ProfileToneCurve` control points `(x, y)` into a 4096-entry LUT using a monotone
/// cubic Hermite spline (Fritsch–Carlson).
fn evaluate_spline_lut(pts: &[[f32; 2]]) -> [f32; TONE_CURVE_LUT_LEN] {
    let n = pts.len();
    let mut h = vec![0.0f64; n - 1];
    let mut m = vec![0.0f64; n - 1];
    for i in 0..n - 1 {
        let dx = (pts[i + 1][0] as f64 - pts[i][0] as f64).max(1e-9);
        let dy = pts[i + 1][1] as f64 - pts[i][1] as f64;
        h[i] = dx;
        m[i] = dy / dx;
    }

    let mut d = vec![0.0f64; n];
    d[0] = m[0];
    d[n - 1] = m[n - 2];
    for i in 1..n - 1 {
        if m[i - 1] * m[i] <= 0.0 {
            d[i] = 0.0;
        } else {
            let w1 = 2.0 * h[i] + h[i - 1];
            let w2 = h[i] + 2.0 * h[i - 1];
            d[i] = (w1 + w2) / (w1 / m[i - 1] + w2 / m[i]);
        }
    }

    let mut lut = [0.0f32; TONE_CURVE_LUT_LEN];
    let mut seg = 0usize;
    for (k, out) in lut.iter_mut().enumerate() {
        let x = k as f64 / (TONE_CURVE_LUT_LEN - 1) as f64;
        if x <= pts[0][0] as f64 {
            *out = pts[0][1].clamp(0.0, 1.0);
            continue;
        }
        if x >= pts[n - 1][0] as f64 {
            *out = pts[n - 1][1].clamp(0.0, 1.0);
            continue;
        }
        while seg + 1 < n - 1 && x > pts[seg + 1][0] as f64 {
            seg += 1;
        }
        let t = ((x - pts[seg][0] as f64) / h[seg]).clamp(0.0, 1.0);
        let t2 = t * t;
        let t3 = t2 * t;
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        let y = h00 * pts[seg][1] as f64
            + h10 * h[seg] * d[seg]
            + h01 * pts[seg + 1][1] as f64
            + h11 * h[seg] * d[seg + 1];
        *out = (y as f32).clamp(0.0, 1.0);
    }
    lut
}

// ---- Profile Registry ----

/// Normalizes a camera model string so `"OM Digital Solutions"` + `"OM-5MarkII"` (raw EXIF) and
/// `"OM Digital Solutions OM-5 Mark II"` (DCP `UniqueCameraModel`) match identically.
pub fn normalize_camera_model(make: &str, model: &str) -> String {
    let norm = |s: &str| -> String {
        s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
    };
    let mk = norm(make);
    let md = norm(model);
    if md.starts_with(&mk) { md } else { format!("{mk}{md}") }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    CameraMatching,
    AdobeStandard,
    Custom,
}

#[derive(Clone)]
struct Entry {
    id: u32,
    name: String,
    group: Group,
    path: PathBuf,
}

struct Registry {
    /// Interned profile names: index 0 is `"Windedit"`, `1..` are profile names (`"Camera Natural"`, …).
    names: Vec<String>,
    by_name: HashMap<String, u32>,
    /// Custom profiles scanned once from `<exe>\Profiles` and `%APPDATA%\Windedit\Profiles`, keyed
    /// by normalized `UniqueCameraModel` (empty string for camera-agnostic look profiles).
    custom: HashMap<String, Vec<Entry>>,
    /// Discovered profiles per normalized camera model.
    by_camera: HashMap<String, Vec<Entry>>,
    /// Parsed `.dcp` cache, keyed by file path.
    parsed: HashMap<PathBuf, Arc<Profile>>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn adobe_root() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\Adobe\CameraRaw\CameraProfiles")
}

fn custom_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        dirs.push(parent.join("Profiles"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("Windedit").join("Profiles"));
    }
    dirs
}

impl Registry {
    fn new() -> Registry {
        let mut reg = Registry {
            names: vec!["Windedit".to_string()],
            by_name: HashMap::from([("Windedit".to_string(), 0)]),
            custom: HashMap::new(),
            by_camera: HashMap::new(),
            parsed: HashMap::new(),
        };
        for dir in custom_dirs() {
            reg.scan_custom_dir(&dir);
        }
        reg
    }

    fn intern(&mut self, name: &str) -> u32 {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("Windedit") {
            return 0;
        }
        if let Some(&id) = self.by_name.get(trimmed) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(trimmed.to_string());
        self.by_name.insert(trimmed.to_string(), id);
        id
    }

    fn scan_custom_dir(&mut self, dir: &Path) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                self.scan_custom_dir(&path);
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("dcp"))
                && let Some((cam, name)) = read_header_tags(&path)
            {
                let id = self.intern(&name);
                let key = normalize_camera_model("", &cam);
                self.custom.entry(key).or_default().push(Entry { id, name, group: Group::Custom, path });
            }
        }
    }

    fn ensure_camera(&mut self, norm_cam: &str) {
        if self.by_camera.contains_key(norm_cam) {
            return;
        }
        let mut list = Vec::new();
        let root = adobe_root();

        // 1. Camera Matching profiles under `Camera\<CameraModel>\*.dcp`.
        let cam_root = root.join("Camera");
        if let Ok(rd) = fs::read_dir(&cam_root) {
            for sub in rd.flatten() {
                let sub_name = sub.file_name();
                if normalize_camera_model("", &sub_name.to_string_lossy()) == norm_cam
                    && let Ok(files) = fs::read_dir(sub.path())
                {
                    for f in files.flatten() {
                        let p = f.path();
                        if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dcp"))
                            && let Some((cam, name)) = read_header_tags(&p)
                            && normalize_camera_model("", &cam) == norm_cam
                        {
                            let id = self.intern(&name);
                            list.push(Entry { id, name, group: Group::CameraMatching, path: p });
                        }
                    }
                }
            }
        }

        // 2. Adobe Standard under `Adobe Standard\<CameraModel> Adobe Standard.dcp`.
        let std_root = root.join("Adobe Standard");
        if let Ok(rd) = fs::read_dir(&std_root) {
            for f in rd.flatten() {
                let p = f.path();
                if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dcp")) {
                    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
                    let cam_part = stem.strip_suffix(" Adobe Standard").unwrap_or(&stem);
                    if normalize_camera_model("", cam_part) == norm_cam
                        && let Some((cam, name)) = read_header_tags(&p)
                        && normalize_camera_model("", &cam) == norm_cam
                    {
                        let id = self.intern(&name);
                        list.push(Entry { id, name, group: Group::AdobeStandard, path: p });
                    }
                }
            }
        }

        // 3. Custom profiles matching this camera or camera-agnostic.
        for key in [norm_cam, ""] {
            if let Some(customs) = self.custom.get(key) {
                list.extend(customs.iter().cloned());
            }
        }

        // Sort Camera Matching in Lightroom's natural order (Standard/Natural, Vivid, Portrait,
        // Landscape, Flat/Muted, then Monochrome/creative filters alphabetically).
        list.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| camera_profile_rank(&a.name).cmp(&camera_profile_rank(&b.name)))
                .then_with(|| a.name.cmp(&b.name))
        });
        list.dedup_by(|a, b| a.id == b.id);
        self.by_camera.insert(norm_cam.to_string(), list);
    }
}

fn camera_profile_rank(name: &str) -> u32 {
    let s = name.strip_prefix("Camera ").unwrap_or(name);
    match s {
        "Standard" => 0,
        "Natural" => 1,
        "Vivid" => 2,
        "Portrait" => 3,
        "Landscape" => 4,
        "Flat" => 5,
        "Muted" => 6,
        "Neutral" => 7,
        "Faithful" => 8,
        _ if s.starts_with("Monotone") || s.starts_with("Monochrome") || s.starts_with("L.Monochrome") => 20,
        _ => 10,
    }
}

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mutex = REGISTRY.get_or_init(|| Mutex::new(Registry::new()));
    let mut guard = mutex.lock().unwrap();
    f(&mut guard)
}

/// Returns the non-zero `u32` ID for a profile name (`0` for `"Windedit"` or `""`).
pub fn id_for_name(name: &str) -> u32 {
    with_registry(|reg| reg.intern(name))
}

/// For `crs:CameraProfile` in Lightroom XMP sidecars: returns a non-zero ID when `name` is a
/// DCP profile name (Camera Matching, `"Adobe Standard"`, or an installed custom profile), or `0`
/// for Lightroom's built-in `"Adobe Color"` so unedited Lightroom sidecars stay on our default.
pub fn id_for_lightroom_name(name: &str) -> u32 {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("Adobe Color") {
        return 0;
    }
    if trimmed.starts_with("Camera ") || trimmed == "Adobe Standard" {
        return id_for_name(trimmed);
    }
    with_registry(|reg| reg.by_name.get(trimmed).copied().unwrap_or(0))
}

/// Returns the profile name for a non-zero `id`, or `None` for `0` (the default Windedit look).
pub fn name_of(id: u32) -> Option<String> {
    if id == 0 {
        return None;
    }
    with_registry(|reg| reg.names.get(id as usize).cloned())
}

/// Lists the profiles available for `(make, model)` in drop-down order:
/// `Windedit` (`0`), Camera Matching, `Adobe Standard`, and custom profiles.
pub fn profiles_for_camera(make: &str, model: &str) -> Vec<(u32, String)> {
    let norm = normalize_camera_model(make, model);
    with_registry(|reg| {
        if !norm.is_empty() {
            reg.ensure_camera(&norm);
        }
        let mut out = vec![(0u32, "Windedit".to_string())];
        if let Some(entries) = reg.by_camera.get(&norm) {
            for e in entries {
                out.push((e.id, e.name.clone()));
            }
        }
        out
    })
}

/// Returns the parsed `Profile` for `(make, model)` and `id`, parsing and caching its `.dcp` file
/// on first use. Returns `None` if `id == 0` or no matching DCP is installed for this camera.
pub fn get(make: &str, model: &str, id: u32) -> Option<Arc<Profile>> {
    if id == 0 {
        return None;
    }
    let norm = normalize_camera_model(make, model);
    let path = with_registry(|reg| {
        if !norm.is_empty() {
            reg.ensure_camera(&norm);
        }
        let entry = reg.by_camera.get(&norm)?.iter().find(|e| e.id == id)?;
        if let Some(p) = reg.parsed.get(&entry.path) {
            return Some(Ok(p.clone()));
        }
        Some(Err(entry.path.clone()))
    })?;

    match path {
        Ok(prof) => Some(prof),
        Err(p) => {
            let parsed = Arc::new(Profile::from_file(&p).ok()?);
            with_registry(|reg| {
                reg.parsed.insert(p, parsed.clone());
            });
            Some(parsed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_adobe_profiles() {
        let root = adobe_root();
        if !root.exists() {
            return;
        }
        let cases = [
            (
                root.join(r"Adobe Standard\OM Digital Solutions OM-5 Mark II Adobe Standard.dcp"),
                "OM Digital Solutions OM-5 Mark II",
                "Adobe Standard",
                [90, 30, 1],
                [36, 8, 16],
                false,
                0.0,
            ),
            (
                root.join(r"Camera\OM Digital Solutions OM-5 Mark II\OM Digital Solutions OM-5 Mark II Camera Natural.dcp"),
                "OM Digital Solutions OM-5 Mark II",
                "Camera Natural",
                [0, 0, 0],
                [90, 16, 16],
                true,
                -0.32,
            ),
            (
                root.join(r"Camera\OM Digital Solutions OM-5 Mark II\OM Digital Solutions OM-5 Mark II Camera Vivid.dcp"),
                "OM Digital Solutions OM-5 Mark II",
                "Camera Vivid",
                [0, 0, 0],
                [90, 16, 16],
                true,
                -0.32,
            ),
            (
                root.join(r"Camera\Panasonic DC-S1\Panasonic DC-S1 Camera Natural.dcp"),
                "Panasonic DC-S1",
                "Camera Natural",
                [0, 0, 0],
                [90, 16, 16],
                true,
                0.0,
            ),
            (
                root.join(r"Camera\Panasonic DC-S9\Panasonic DC-S9 Camera Vivid.dcp"),
                "Panasonic DC-S9",
                "Camera Vivid",
                [0, 0, 0],
                [90, 16, 16],
                true,
                0.05,
            ),
        ];

        for (path, cam, name, hs_dims, lk_dims, has_curve, exp_off) in cases {
            let hdr = read_header_tags(&path).expect("header tags");
            assert_eq!(hdr.0, cam);
            assert_eq!(hdr.1, name);

            let p = Profile::from_file(&path).expect("full parse");
            assert_eq!(p.unique_camera_model, cam);
            assert_eq!(p.profile_name, name);
            assert_eq!(p.illuminant1, 17); // Standard Light A
            assert_eq!(p.illuminant2, 21); // D65
            assert!(p.color_matrix1.is_some() && p.color_matrix2.is_some());
            assert!(p.forward_matrix1.is_some() && p.forward_matrix2.is_some());
            assert_eq!(p.hue_sat_dims, hs_dims);
            assert_eq!(p.look_dims, lk_dims);
            assert_eq!(p.tone_curve_lut.is_some(), has_curve);
            assert!((p.baseline_exposure_offset - exp_off).abs() < 1e-4);

            // Neutral [0.18, 0.18, 0.18] through cam_to_prophoto (wb=[1,1,1]) stays neutral.
            let m = p.cam_to_prophoto(5500.0, &[1.0, 1.0, 1.0], &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
            for row in m {
                let sum: f32 = row.iter().sum();
                assert!((sum - 1.0).abs() < 1e-4, "{name}: row sum {sum}");
            }
            let hs = p.interpolated_hue_sat(5500.0);
            let srgb = p.eval_pixel([0.18, 0.18, 0.18], &m, &hs, 1.0, 1.0);
            assert!((srgb[0] - srgb[1]).abs() < 1e-4 && (srgb[1] - srgb[2]).abs() < 1e-4, "{name}: {srgb:?}");
            assert!(srgb[0] > 0.1 && srgb[0] < 0.9, "{name}: {srgb:?}");
        }
    }

    #[test]
    fn registry_discovers_camera_profiles() {
        let root = adobe_root();
        if !root.exists() {
            return;
        }
        // Works with both rawler's ("OM-5 Mark II") and raw EXIF's ("OM-5MarkII") model strings.
        for model in ["OM-5 Mark II", "OM-5MarkII"] {
            let list = profiles_for_camera("OM Digital Solutions", model);
            let names: Vec<&str> = list.iter().map(|(_, n)| n.as_str()).collect();
            assert_eq!(names[0], "Windedit");
            assert_eq!(names[1], "Camera Natural");
            assert_eq!(names[2], "Camera Vivid");
            assert!(names.contains(&"Adobe Standard"));
        }
        let s1 = profiles_for_camera("Panasonic", "DC-S1");
        let s1_names: Vec<&str> = s1.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(s1_names[0], "Windedit");
        assert_eq!(s1_names[1], "Camera Standard");
        assert_eq!(s1_names[2], "Camera Natural");
        assert_eq!(s1_names[3], "Camera Vivid");
        assert!(s1_names.contains(&"Adobe Standard"));

        let nat_id = id_for_name("Camera Natural");
        assert!(nat_id > 0);
        assert_eq!(name_of(nat_id).as_deref(), Some("Camera Natural"));
        let prof = get("OM Digital Solutions", "OM-5 Mark II", nat_id).expect("OM-5 II Camera Natural");
        assert_eq!(prof.profile_name, "Camera Natural");
    }
}

