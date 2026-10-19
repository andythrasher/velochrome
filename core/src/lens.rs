//! Lens distortion and chromatic aberration correction, from the values cameras record in their
//! raw files. See docs/lens.md for the tags and the model.
//!
//! The Panasonic and Olympus maths follows darktable's embedded-metadata lens correction
//! (src/iop/lens.cc and src/common/exif.cc, GPL-3.0-or-later), which in turn follows what
//! exiftool and exiv2 document about the tags.

/// A minimal TIFF reader: just enough to walk directories of the raw formats we care about
/// (ORF and RW2 are TIFF variants that differ only in their magic number).
#[derive(Clone, Copy)]
struct Tiff<'a> {
    bytes: &'a [u8],
    little: bool,
}

/// One directory entry: where its value starts and how many values it has.
#[derive(Clone, Copy, Debug)]
struct Entry {
    tag: u16,
    kind: u16,
    count: usize,
    value: usize,
}

impl<'a> Tiff<'a> {
    fn new(bytes: &'a [u8]) -> Option<Tiff<'a>> {
        let little = match bytes.get(0..2)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        Some(Tiff { bytes, little })
    }

    fn u16_at(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if self.little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn u32_at(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if self.little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    /// The entries of the directory at `ifd`. Values larger than four bytes are found at an
    /// offset counted from `base`.
    fn entries(&self, ifd: usize, base: usize) -> Vec<Entry> {
        let Some(count) = self.u16_at(ifd) else { return Vec::new() };
        (0..(count as usize).min(1024))
            .filter_map(|i| {
                let at = ifd + 2 + i * 12;
                let (tag, kind, n) = (self.u16_at(at)?, self.u16_at(at + 2)?, self.u32_at(at + 4)? as usize);
                let size = n.checked_mul(type_size(kind))?;
                let value = if size <= 4 { at + 8 } else { base.checked_add(self.u32_at(at + 8)? as usize)? };
                (value + size <= self.bytes.len()).then_some(Entry { tag, kind, count: n, value })
            })
            .collect()
    }

    fn find(&self, ifd: usize, base: usize, tag: u16) -> Option<Entry> {
        self.entries(ifd, base).into_iter().find(|e| e.tag == tag)
    }

    /// The first value of an entry, as an offset or number.
    fn first_u32(&self, e: Entry) -> Option<u32> {
        match e.kind {
            3 | 8 => self.u16_at(e.value).map(u32::from),
            _ => self.u32_at(e.value),
        }
    }

    /// An entry's values as floats (integers of any width, rationals, floats).
    fn floats(&self, e: Entry) -> Vec<f32> {
        (0..e.count)
            .filter_map(|i| {
                let at = e.value + i * type_size(e.kind);
                match e.kind {
                    1 | 7 => self.bytes.get(at).map(|&b| b as f32),
                    6 => self.bytes.get(at).map(|&b| b as i8 as f32),
                    3 => self.u16_at(at).map(|v| v as f32),
                    8 => self.u16_at(at).map(|v| v as i16 as f32),
                    4 => self.u32_at(at).map(|v| v as f32),
                    9 => self.u32_at(at).map(|v| v as i32 as f32),
                    5 => Some(self.u32_at(at)? as f32 / self.u32_at(at + 4)? as f32),
                    10 => Some(self.u32_at(at)? as i32 as f32 / self.u32_at(at + 4)? as i32 as f32),
                    11 => self.u32_at(at).map(f32::from_bits),
                    _ => None,
                }
            })
            .collect()
    }
}

fn type_size(kind: u16) -> usize {
    match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 1,
    }
}

/// Which family of radial model the numbers belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LensModel {
    /// Panasonic: the distorted (source) radius is `rd`, the corrected one `rd·(1 + s·(a·rd² +
    /// b·rd⁴ + c·rd⁶))`. Inverted by iteration.
    Panasonic,
    /// Olympus: source radius = corrected radius × `drs·(1 + rs²·(k2 + rs²·(k4 + rs²·k6)))`,
    /// with `rs = drs·corrected radius`.
    Olympus,
}

/// How to find, for a position in the corrected image, where it sits in the sensor image.
/// Radii are measured from the image centre in units of the half-diagonal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LensCorrection {
    pub model: LensModel,
    /// Panasonic: `s`. Olympus: `drs`.
    pub scale: f32,
    /// Panasonic: `a, b, c`. Olympus: `k2, k4, k6`.
    pub k: [f32; 3],
    /// Lateral chromatic aberration (Olympus only): red and blue sit at the green position's
    /// radius times `1 + c0 + c2·rd² + c4·rd⁴`, `rd` being that source radius. All zero if none.
    pub ca_red: [f32; 3],
    pub ca_blue: [f32; 3],
    /// Extra zoom into the corrected image so no empty border shows (1 = none), see `fit`.
    pub zoom: f32,
}

#[derive(Clone, Copy)]
pub enum Channel {
    Red,
    Green,
    Blue,
}

impl LensCorrection {
    /// Whether this correction changes the geometry at all (some lenses need none and carry
    /// only chromatic aberration data).
    pub fn has_distortion(&self) -> bool {
        self.scale != 1.0 || self.k != [0.0; 3]
    }

    pub fn has_ca(&self) -> bool {
        self.ca_red != [0.0; 3] || self.ca_blue != [0.0; 3]
    }

    /// What is left of this correction with the Edit's switches: without distortion the geometry
    /// is the sensor's own (no scale, no zoom, so the frame keeps its size); without chromatic
    /// aberration the colour terms are dropped. None if nothing is left to apply.
    pub fn with_switches(&self, distortion: bool, chromatic_aberration: bool) -> Option<LensCorrection> {
        let ca = chromatic_aberration && self.has_ca();
        if !distortion && !ca {
            return None;
        }
        let mut l = *self;
        if !distortion {
            l.scale = 1.0;
            l.k = [0.0; 3];
            l.zoom = 1.0;
        }
        if !ca {
            l.ca_red = [0.0; 3];
            l.ca_blue = [0.0; 3];
        }
        Some(l)
    }

    /// Source radius ÷ corrected radius for the green channel, at corrected radius `r`.
    fn green_ratio(&self, r: f32) -> f32 {
        let [k0, k1, k2] = self.k;
        match self.model {
            LensModel::Olympus => {
                let rs2 = (r * self.scale).powi(2);
                self.scale * (1.0 + rs2 * (k0 + rs2 * (k1 + rs2 * k2)))
            }
            LensModel::Panasonic => {
                // Two rounds of rd = r / f(rd) get within a thousandth of the inverse.
                let mut rd = r;
                for _ in 0..2 {
                    let rd2 = rd * rd;
                    let f = 1.0 + self.scale * rd2 * (k0 + rd2 * (k1 + rd2 * k2));
                    rd = if f > 1e-6 { r / f } else { r };
                }
                if r > 0.0 { rd / r } else { 1.0 }
            }
        }
    }

    /// Source radius ÷ corrected radius for one colour channel.
    pub fn ratio(&self, channel: Channel, r: f32) -> f32 {
        let g = self.green_ratio(r);
        let c = match channel {
            Channel::Green => return g,
            Channel::Red => self.ca_red,
            Channel::Blue => self.ca_blue,
        };
        let rd2 = (g * r).powi(2);
        g * (1.0 + c[0] + rd2 * (c[1] + rd2 * c[2]))
    }

    /// Where a corrected-image pixel offset `p` (from the centre, in pixels) lands in the sensor
    /// image, as an offset from its centre, for an image of `w` × `h`. The CPU twin of the
    /// develop shader's warp.
    pub fn source(&self, channel: Channel, p: [f32; 2], w: f32, h: f32) -> [f32; 2] {
        let half_diag = 0.5 * w.hypot(h);
        let q = [p[0] * self.zoom, p[1] * self.zoom];
        let f = self.ratio(channel, q[0].hypot(q[1]) / half_diag);
        [q[0] * f, q[1] * f]
    }

    /// Works out `zoom`: the least that keeps every pixel of the corrected frame inside the
    /// sensor image for all three channels, so no empty or stretched border shows.
    pub fn fit(mut self, w: u32, h: u32) -> LensCorrection {
        let (w, h) = (w as f32, h as f32);
        let inside = |s: &LensCorrection| {
            const STEPS: usize = 128;
            (0..=STEPS).all(|i| {
                let t = i as f32 / STEPS as f32;
                let (x, y) = ((t - 0.5) * w, (t - 0.5) * h);
                // Points along all four edges of the frame.
                [[x, -h / 2.0], [x, h / 2.0], [-w / 2.0, y], [w / 2.0, y]].iter().all(|&p| {
                    [Channel::Red, Channel::Green, Channel::Blue].iter().all(|&c| {
                        let s = s.source(c, p, w, h);
                        s[0].abs() <= w / 2.0 + 0.5 && s[1].abs() <= h / 2.0 + 0.5
                    })
                })
            })
        };
        self.zoom = 1.0;
        if !inside(&self) {
            // Zooming in only ever helps: search for the largest zoom factor below 1 that fits.
            let (mut lo, mut hi) = (0.5f32, 1.0f32);
            for _ in 0..24 {
                let mid = 0.5 * (lo + hi);
                self.zoom = mid;
                if inside(&self) { lo = mid } else { hi = mid }
            }
            self.zoom = lo;
        }
        self
    }

    /// Reads the correction a raw file carries, if any. `w` × `h` is the size of the frame the
    /// correction will be applied to.
    pub fn from_raw(bytes: &[u8], w: u32, h: u32) -> Option<LensCorrection> {
        let t = Tiff::new(bytes)?;
        let ifd0 = t.u32_at(4)? as usize;
        // Panasonic keeps it in the first directory.
        if let Some(e) = t.find(ifd0, 0, 0x0119) {
            return Self::panasonic(&t.floats(e)).map(|l| l.fit(w, h));
        }
        let (ifd, base) = olympus_image_processing(&t, ifd0)?;
        Self::olympus(
            t.find(ifd, base, 0x150a).map(|e| t.floats(e)).as_deref(),
            t.find(ifd, base, 0x150c).map(|e| t.floats(e)).as_deref(),
        )
        .map(|l| l.fit(w, h))
    }

    /// From the 32 bytes of DistortionInfo (tag 0x0119), as sixteen signed 16-bit numbers.
    fn panasonic(bytes: &[f32]) -> Option<LensCorrection> {
        if bytes.len() != 32 {
            return None;
        }
        let v = |i: usize| i16::from_le_bytes([bytes[2 * i] as u8, bytes[2 * i + 1] as u8]) as f32;
        if (v(7) as i32 & 0x0f) != 1 {
            return None; // the camera marked it off
        }
        Some(LensCorrection {
            model: LensModel::Panasonic,
            scale: 1.0 / (1.0 + v(5) / 32768.0),
            k: [v(8) / 32768.0, v(4) / 32768.0, v(11) / 32768.0],
            ca_red: [0.0; 3],
            ca_blue: [0.0; 3],
            zoom: 1.0,
        })
    }

    /// From the ImageProcessing distortion (0x150a, four floats) and CA (0x150c, six floats).
    fn olympus(dist: Option<&[f32]>, ca: Option<&[f32]>) -> Option<LensCorrection> {
        let d = dist.filter(|d| d.len() == 4 && d[..3].iter().any(|&x| x != 0.0));
        let ca = ca.filter(|c| c.len() == 6 && c.iter().any(|&x| x != 0.0));
        if d.is_none() && ca.is_none() {
            return None;
        }
        let (scale, k) = d.map_or((1.0, [0.0; 3]), |d| (d[3], [d[0], d[1], d[2]]));
        let (ca_red, ca_blue) = ca.map_or(([0.0; 3], [0.0; 3]), |c| ([c[0], c[1], c[2]], [c[3], c[4], c[5]]));
        Some(LensCorrection { model: LensModel::Olympus, scale, k, ca_red, ca_blue, zoom: 1.0 })
    }
}

/// The ImageProcessing directory of an Olympus or OM System maker note: where its entries are,
/// and the offset their values are counted from.
fn olympus_image_processing(t: &Tiff, ifd0: usize) -> Option<(usize, usize)> {
    olympus_directory(t, ifd0, 0x2040)
}

/// A sub-directory of the maker note (0x2040 ImageProcessing, 0x2020 CameraSettings, ...).
fn olympus_directory(t: &Tiff, ifd0: usize, which: u16) -> Option<(usize, usize)> {
    let exif = t.first_u32(t.find(ifd0, 0, 0x8769)?)? as usize;
    let note = t.find(exif, 0, 0x927c)?;
    let head = t.bytes.get(note.value..note.value + 16)?;
    let ifd = if head.starts_with(b"OM SYSTEM") {
        note.value + 16
    } else if head.starts_with(b"OLYMPUS\0") {
        note.value + 12
    } else {
        return None;
    };
    // Offsets inside the note count from its start.
    let sub = t.find(ifd, note.value, which)?;
    Some((note.value + t.u32_at(sub.value)? as usize, note.value))
}

/// The lens focal length in millimetres, from the Exif directory (for picking test photos).
pub fn focal_length(bytes: &[u8]) -> Option<f32> {
    let t = Tiff::new(bytes)?;
    let ifd0 = t.u32_at(4)? as usize;
    let exif = t.first_u32(t.find(ifd0, 0, 0x8769)?)? as usize;
    t.floats(t.find(exif, 0, 0x920a)?).first().copied()
}

/// The camera model string from the first directory (for picking test photos).
pub fn camera_model(bytes: &[u8]) -> Option<String> {
    let t = Tiff::new(bytes)?;
    let e = t.find(t.u32_at(4)? as usize, 0, 0x0110)?;
    let raw = t.bytes.get(e.value..e.value + e.count)?;
    Some(String::from_utf8_lossy(raw).trim_end_matches('\0').to_string())
}

/// Every entry of IFD0, or of an Olympus maker-note directory (0x2040 unless SUB=<hex> says
/// otherwise), as (tag, kind, count, all values): for exploring files (`bench lenstags`).
pub fn dump_tags(bytes: &[u8], olympus: bool) -> Vec<(u16, u16, usize, Vec<f32>)> {
    let Some(t) = Tiff::new(bytes) else { return vec![] };
    let Some(ifd0) = t.u32_at(4) else { return vec![] };
    let (ifd, base) = if olympus {
        let which = std::env::var("SUB").ok().and_then(|s| u16::from_str_radix(&s, 16).ok()).unwrap_or(0x2040);
        match olympus_directory(&t, ifd0 as usize, which) { Some(x) => x, None => return vec![] }
    } else {
        (ifd0 as usize, 0)
    };
    t.entries(ifd, base).into_iter().map(|e| (e.tag, e.kind, e.count, t.floats(e))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real-file tests need Olympus and Panasonic raws. Point VELOCHROME_TEST_RAWS at a folder
    // holding them (directly or in subfolders, such as one per day); without it they're skipped.
    const ORF: &str = "P6060001.ORF";
    const RW2: &str = "P1070381.RW2";

    fn file(name: &str) -> Option<Vec<u8>> {
        let root = std::path::PathBuf::from(std::env::var_os("VELOCHROME_TEST_RAWS")?);
        let mut folders = vec![root];
        while let Some(dir) = folders.pop() {
            for entry in std::fs::read_dir(&dir).ok()?.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    folders.push(path);
                } else if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)) {
                    return std::fs::read(path).ok();
                }
            }
        }
        None
    }

    #[test]
    fn switches_flatten_the_geometry_and_drop_colour_terms() {
        let l = LensCorrection {
            model: LensModel::Olympus,
            scale: 0.97,
            k: [0.02, 0.0, 0.0],
            ca_red: [0.001, 0.0, 0.0],
            ca_blue: [-0.001, 0.0, 0.0],
            zoom: 0.98,
        };
        assert_eq!(l.with_switches(true, true), Some(l));
        let flat = l.with_switches(false, true).unwrap();
        assert_eq!((flat.scale, flat.zoom, flat.k), (1.0, 1.0, [0.0; 3]));
        assert!(flat.has_ca());
        // Distortion off with nothing else to do: the sensor's own geometry exactly.
        let source = flat.source(Channel::Green, [1000.0, 700.0], 5184.0, 3888.0);
        assert_eq!(source, [1000.0, 700.0]);
        let no_ca = l.with_switches(true, false).unwrap();
        assert!(!no_ca.has_ca() && no_ca.scale == 0.97);
        assert_eq!(l.with_switches(false, false), None);
        // A correction with no colour data left alone has nothing for the CA switch to keep.
        let plain = LensCorrection { ca_red: [0.0; 3], ca_blue: [0.0; 3], ..l };
        assert_eq!(plain.with_switches(false, true), None);
    }

    #[test]
    fn olympus_tags_from_a_real_file() {
        let Some(bytes) = file(ORF) else { return };
        let l = LensCorrection::from_raw(&bytes, 5184, 3888).unwrap();
        assert_eq!(l.model, LensModel::Olympus);
        assert!((l.scale - 0.96938187).abs() < 1e-6);
        assert!((l.k[0] - 0.024524447).abs() < 1e-6 && (l.k[2] + 0.0009840376).abs() < 1e-6);
        assert!(l.has_ca() && (l.ca_blue[0] + 0.0005849429).abs() < 1e-7);
        assert_eq!(focal_length(&bytes), Some(39.0));
        assert!(l.zoom > 0.5 && l.zoom <= 1.0);
    }

    #[test]
    fn panasonic_tags_from_a_real_file() {
        let Some(bytes) = file(RW2) else { return };
        let l = LensCorrection::from_raw(&bytes, 6000, 4000).unwrap();
        assert_eq!(l.model, LensModel::Panasonic);
        assert!((l.scale - 1.0 / (1.0 - 119.0 / 32768.0)).abs() < 1e-6);
        assert!((l.k[0] + 540.0 / 32768.0).abs() < 1e-6 && (l.k[1] - 824.0 / 32768.0).abs() < 1e-6);
        assert!(!l.has_ca());
    }

    #[test]
    fn a_file_without_correction_gives_none() {
        assert!(LensCorrection::from_raw(b"II*\0\x08\0\0\0\0\0", 100, 100).is_none());
        assert!(LensCorrection::olympus(Some(&[0.0, 0.0, 0.0, 1.0]), Some(&[0.0; 6])).is_none());
    }

    #[test]
    fn fitted_frame_stays_inside_the_sensor() {
        let l = LensCorrection::olympus(Some(&[0.03, 0.006, -0.001, 0.97]), None).unwrap().fit(5184, 3888);
        let (w, h) = (5184.0, 3888.0);
        for p in [[w / 2.0, h / 2.0], [-w / 2.0, h / 2.0], [0.0, h / 2.0], [w / 2.0, 0.0]] {
            let s = l.source(Channel::Green, p, w, h);
            assert!(s[0].abs() <= w / 2.0 + 0.6 && s[1].abs() <= h / 2.0 + 0.6, "{p:?} → {s:?}");
        }
    }

    #[test]
    fn panasonic_inverse_is_close() {
        // a = -0.05 (v8), b = 0.02 (v4), c = 0 (v11), enabled, scale 1.
        let mut b = [0f32; 32];
        for (i, v) in [(4usize, 655i16), (7, 1), (8, -1638)] {
            let [lo, hi] = v.to_le_bytes();
            b[2 * i] = lo as f32;
            b[2 * i + 1] = hi as f32;
        }
        let l = LensCorrection::panasonic(&b).unwrap();
        let r = 0.8;
        let rd = l.green_ratio(r) * r;
        let rd2 = rd * rd;
        let back = rd * (1.0 + l.scale * rd2 * (l.k[0] + rd2 * (l.k[1] + rd2 * l.k[2])));
        assert!((back - r).abs() < 2e-3, "{back}");
    }
}
