//! The camera's EXIF for an exported JPEG. Cameras put a full JPEG in every raw, with the shot's
//! EXIF in its APP1 segment (model, date, exposure, lens, maker notes). The export copies that
//! segment and changes only what the new pixels make untrue: Orientation (they're upright now),
//! the pixel dimensions, the color space (sRGB) and the little thumbnail, which would still show
//! the sensor's orientation.

use std::path::Path;

use crate::decode;

const SOI: [u8; 2] = [0xFF, 0xD8];
const HEADER: &[u8] = b"Exif\0\0";

/// The EXIF block (the APP1 payload, starting "Exif\0\0") of the photo's biggest embedded JPEG,
/// adjusted for an upright export of `width` × `height`. None if the raw has no usable EXIF.
pub fn for_export(raw: &Path, width: u32, height: u32) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(raw).ok()?.take(1 << 20).read_to_end(&mut head).ok()?;
    let jpegs = decode::embedded_jpegs(&head);
    // The biggest JPEG first; a smaller one if that has no EXIF.
    let mut by_size: Vec<_> = jpegs.iter().collect();
    by_size.sort_by_key(|j| std::cmp::Reverse(j.width as u64 * j.height as u64));
    let tiff = by_size
        .into_iter()
        .find_map(|j| patch(find_exif(&head[j.offset..])?, width, height))
        // Olympus' JPEGs have none: describe the shot from the raw's own directories.
        .or_else(|| from_tiff(&head, width, height))?;
    let mut block = HEADER.to_vec();
    block.extend(tiff);
    Some(block)
}

/// The TIFF structure inside a JPEG's Exif APP1 segment.
fn find_exif(jpeg: &[u8]) -> Option<&[u8]> {
    let mut j = 2;
    while j + 4 <= jpeg.len() && jpeg[j] == 0xFF {
        let marker = jpeg[j + 1];
        let len = u16::from_be_bytes([jpeg[j + 2], jpeg[j + 3]]) as usize;
        if marker == 0xDA || len < 2 {
            return None;
        }
        let body = jpeg.get(j + 4..j + 2 + len)?;
        if marker == 0xE1 && body.starts_with(HEADER) {
            return Some(&body[HEADER.len()..]);
        }
        j += 2 + len;
    }
    None
}

struct Tiff {
    buf: Vec<u8>,
    little: bool,
}

impl Tiff {
    fn u16_at(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.buf.get(at..at + 2)?.try_into().ok()?;
        Some(if self.little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn u32_at(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.buf.get(at..at + 4)?.try_into().ok()?;
        Some(if self.little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn u32_of(&self, b: &[u8]) -> Option<u32> {
        let b: [u8; 4] = b.get(0..4)?.try_into().ok()?;
        Some(if self.little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn put_u16(&mut self, at: usize, v: u16) -> Option<()> {
        let b = if self.little { v.to_le_bytes() } else { v.to_be_bytes() };
        self.buf.get_mut(at..at + 2)?.copy_from_slice(&b);
        Some(())
    }

    fn put_u32(&mut self, at: usize, v: u32) -> Option<()> {
        let b = if self.little { v.to_le_bytes() } else { v.to_be_bytes() };
        self.buf.get_mut(at..at + 4)?.copy_from_slice(&b);
        Some(())
    }

    /// Where each entry of the directory at `ifd` starts, and where its next-directory pointer is.
    fn entries(&self, ifd: usize) -> Option<(Vec<usize>, usize)> {
        let count = self.u16_at(ifd)? as usize;
        let list: Vec<usize> = (0..count).map(|i| ifd + 2 + i * 12).collect();
        (ifd + 2 + count * 12 + 4 <= self.buf.len()).then_some((list, ifd + 2 + count * 12))
    }

    fn find(&self, ifd: usize, tag: u16) -> Option<usize> {
        self.entries(ifd)?.0.into_iter().find(|&e| self.u16_at(e) == Some(tag))
    }

    /// Sets a single-number entry (SHORT or LONG), widening a SHORT to a LONG if it must.
    fn set_number(&mut self, entry: usize, v: u32) -> Option<()> {
        match self.u16_at(entry + 2)? {
            3 if v <= u16::MAX as u32 => {
                self.put_u16(entry + 8, v as u16)?;
                self.put_u16(entry + 10, 0)
            }
            3 | 4 => {
                self.put_u16(entry + 2, 4)?;
                self.put_u32(entry + 8, v)
            }
            _ => None,
        }
    }
}

/// `tiff` made true of an upright `width` × `height` sRGB image. None if it isn't well-formed.
fn patch(tiff: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let little = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let mut t = Tiff { buf: tiff.to_vec(), little };
    if t.u16_at(2)? != 42 {
        return None;
    }
    let ifd0 = t.u32_at(4)? as usize;
    // Orientation 1: the pixels are upright.
    if let Some(e) = t.find(ifd0, 0x0112) {
        t.set_number(e, 1);
    }
    for (tag, v) in [(0x0100, width), (0x0101, height)] {
        if let Some(e) = t.find(ifd0, tag) {
            t.set_number(e, v);
        }
    }
    if let Some(e) = t.find(ifd0, 0x8769) {
        let exif = t.u32_at(e + 8)? as usize;
        for (tag, v) in [(0xA002, width), (0xA003, height), (0xA001, 1)] {
            if let Some(e) = t.find(exif, tag) {
                t.set_number(e, v);
            }
        }
    }

    // Drop the thumbnail (the next directory): it would still be sideways.
    let (_, next) = t.entries(ifd0)?;
    let ifd1 = t.u32_at(next)? as usize;
    if ifd1 != 0 {
        t.put_u32(next, 0)?;
        // When the thumbnail's bytes are the last thing in the block, cut them off too.
        let start = t.find(ifd1, 0x0201).and_then(|e| t.u32_at(e + 8));
        let len = t.find(ifd1, 0x0202).and_then(|e| t.u32_at(e + 8));
        if let (Some(start), Some(len)) = (start, len)
            && start as usize + len as usize == t.buf.len()
            && start as usize > ifd0
        {
            t.buf.truncate(start as usize);
        }
    }
    // An APP1 segment holds at most 64 KB.
    (t.buf.len() + HEADER.len() + 2 <= u16::MAX as usize).then_some(t.buf)
}


/// One tag of a TIFF directory, with its value read out.
struct Tag {
    tag: u16,
    ty: u16,
    count: u32,
    /// The value, in the source file's byte order.
    data: Vec<u8>,
}

fn type_size(ty: u16) -> usize {
    match ty {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

impl Tiff {
    /// The tags of the directory at `ifd` whose values can be read.
    fn tags(&self, ifd: usize) -> Vec<Tag> {
        let Some((entries, _)) = self.entries(ifd) else { return Vec::new() };
        entries
            .into_iter()
            .filter_map(|e| {
                let (tag, ty, count) = (self.u16_at(e)?, self.u16_at(e + 2)?, self.u32_at(e + 4)?);
                let size = type_size(ty).checked_mul(count as usize).filter(|&s| s > 0)?;
                let data = if size <= 4 {
                    self.buf.get(e + 8..e + 8 + size)?
                } else {
                    let at = self.u32_at(e + 8)? as usize;
                    self.buf.get(at..at.checked_add(size)?)?
                };
                Some(Tag { tag, ty, count, data: data.to_vec() })
            })
            .collect()
    }

    fn number_tag(&self, tag: u16, v: u32) -> Tag {
        let (ty, data) = match (v <= u16::MAX as u32, self.little) {
            (true, true) => (3, (v as u16).to_le_bytes().to_vec()),
            (true, false) => (3, (v as u16).to_be_bytes().to_vec()),
            (false, true) => (4, v.to_le_bytes().to_vec()),
            (false, false) => (4, v.to_be_bytes().to_vec()),
        };
        Tag { tag, ty, count: 1, data }
    }
}

/// EXIF made from a raw file's own TIFF directories, for cameras whose embedded JPEGs carry
/// none (Olympus). Keeps the camera, software, date and artist tags, and the whole Exif
/// directory (exposure, ISO, lens, dates) except the maker note, whose offsets point into the
/// raw file. Dimensions, orientation and color space are those of the export.
fn from_tiff(raw: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let little = match raw.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let src = Tiff { buf: raw.to_vec(), little };
    let ifd0 = src.u32_at(4)? as usize;
    let tags0 = src.tags(ifd0);
    let exif_at = tags0.iter().find(|t| t.tag == 0x8769).and_then(|t| src.u32_of(&t.data))? as usize;

    let mut main: Vec<Tag> = tags0.into_iter().filter(|t| matches!(t.tag, 0x010F | 0x0110 | 0x0131 | 0x0132 | 0x013B | 0x8298)).collect();
    main.push(src.number_tag(0x0112, 1));
    let mut exif: Vec<Tag> = src.tags(exif_at).into_iter().filter(|t| !matches!(t.tag, 0x927C | 0xA005 | 0xA001 | 0xA002 | 0xA003)).collect();
    exif.extend([src.number_tag(0xA001, 1), src.number_tag(0xA002, width), src.number_tag(0xA003, height)]);
    if main.iter().all(|t| t.tag != 0x0110) {
        return None; // not a camera file we know how to describe
    }
    // The pointer to the Exif directory comes last in the layout; its value is filled in below.
    main.push(Tag { tag: 0x8769, ty: 4, count: 1, data: vec![0; 4] });
    main.sort_by_key(|t| t.tag);
    exif.sort_by_key(|t| t.tag);

    let dir_len = |n: usize| 2 + n * 12 + 4;
    let exif_dir = 8 + dir_len(main.len());
    let mut data_at = exif_dir + dir_len(exif.len());
    let mut out = Tiff { buf: vec![0; data_at], little };
    out.buf[0..4].copy_from_slice(if little { b"II*\0" } else { b"MM\0*" });
    out.put_u32(4, 8)?;
    let mut data = Vec::new();
    for (dir_at, tags) in [(8, &main), (exif_dir, &exif)] {
        out.put_u16(dir_at, tags.len() as u16)?;
        for (i, t) in tags.iter().enumerate() {
            let e = dir_at + 2 + i * 12;
            out.put_u16(e, t.tag)?;
            out.put_u16(e + 2, t.ty)?;
            out.put_u32(e + 4, t.count)?;
            if t.tag == 0x8769 {
                out.put_u32(e + 8, exif_dir as u32)?;
            } else if t.data.len() <= 4 {
                out.buf[e + 8..e + 8 + t.data.len()].copy_from_slice(&t.data);
            } else {
                out.put_u32(e + 8, data_at as u32)?;
                data.extend_from_slice(&t.data);
                data_at += t.data.len();
                if data_at % 2 == 1 {
                    data.push(0);
                    data_at += 1;
                }
            }
        }
    }
    out.buf.extend(data);
    (out.buf.len() + HEADER.len() + 2 <= u16::MAX as usize).then_some(out.buf)
}
/// When a photo was taken, as the camera's clock showed it (no time zone), from EXIF
/// DateTimeOriginal. Field order makes the derived ordering chronological.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Stamp {
    /// "2026-06-06", the name of a day's folder.
    pub fn day_name(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Reads EXIF's "2026:06:06 14:03:09". Zeros (a camera whose clock was never set) are no date.
    fn parse(text: &[u8]) -> Option<Stamp> {
        let text = std::str::from_utf8(text).ok()?.trim_end_matches('\0').trim();
        let field = |from: usize, to: usize| text.get(from..to)?.parse::<u32>().ok();
        let stamp = Stamp {
            year: field(0, 4)? as u16,
            month: field(5, 7)? as u8,
            day: field(8, 10)? as u8,
            hour: field(11, 13).unwrap_or(0) as u8,
            minute: field(14, 16).unwrap_or(0) as u8,
            second: field(17, 19).unwrap_or(0) as u8,
        };
        ((1..=12).contains(&stamp.month) && (1..=31).contains(&stamp.day) && stamp.year >= 1990).then_some(stamp)
    }
}

/// DateTimeOriginal (else DateTime) from a TIFF structure: a raw file, or what a JPEG's Exif
/// segment holds.
fn stamp_in_tiff(tiff: &[u8]) -> Option<Stamp> {
    let little = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let t = Tiff { buf: tiff.to_vec(), little };
    let ifd0 = t.u32_at(4)? as usize;
    let tags0 = t.tags(ifd0);
    let ascii = |tags: &[Tag], id: u16| tags.iter().find(|x| x.tag == id && x.ty == 2).and_then(|x| Stamp::parse(&x.data));
    if let Some(exif) = tags0.iter().find(|x| x.tag == 0x8769).and_then(|x| t.u32_of(&x.data)) {
        let tags = t.tags(exif as usize);
        if let Some(stamp) = ascii(&tags, 0x9003).or_else(|| ascii(&tags, 0x9004)) {
            return Some(stamp);
        }
    }
    ascii(&tags0, 0x0132)
}

/// When the photo in this file (a raw or a JPEG) was taken, from its EXIF. Olympus raws carry
/// the date in their own directories near the start; Panasonic keeps it only in the embedded
/// JPEG, which sits further in, so that's read only if the first look finds nothing.
pub fn capture_time(path: &Path) -> Option<Stamp> {
    use std::io::Read;
    let read = |limit: u64| -> Option<Vec<u8>> {
        let mut head = Vec::new();
        std::fs::File::open(path).ok()?.take(limit).read_to_end(&mut head).ok()?;
        Some(head)
    };
    let in_jpeg = |bytes: &[u8]| find_exif(bytes).and_then(stamp_in_tiff);
    let head = read(64 << 10)?;
    if head.starts_with(&SOI) {
        return in_jpeg(&head);
    }
    if let Some(stamp) = stamp_in_tiff(&head) {
        return Some(stamp);
    }
    let head = read(1 << 20)?;
    decode::embedded_jpegs(&head).iter().find_map(|j| in_jpeg(&head[j.offset..]))
}

/// Reads `(Make, Model)` (`0x010F`, `0x0110`) from the start of a raw or JPEG file.
pub fn camera_make_model(path: &Path) -> Option<(String, String)> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path).ok()?.take(64 << 10).read_to_end(&mut head).ok()?;
    let tiff = if head.starts_with(&SOI) { find_exif(&head)? } else { &head };
    let little = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let t = Tiff { buf: tiff.to_vec(), little };
    let tags0 = t.tags(t.u32_at(4)? as usize);
    let ascii = |id: u16| -> Option<String> {
        let x = tags0.iter().find(|x| x.tag == id && x.ty == 2)?;
        Some(std::str::from_utf8(&x.data).ok()?.trim_end_matches('\0').trim().to_string())
    };
    let model = ascii(0x0110)?;
    let make = ascii(0x010F).unwrap_or_default();
    Some((make, model))
}

/// A raw-like file for tests: a TIFF whose Exif directory holds `date` as DateTimeOriginal.
#[cfg(test)]
pub fn test_tiff(date: &str) -> Vec<u8> {
    let mut t = b"IIRO".to_vec();
    t.extend(8u32.to_le_bytes());
    // IFD0 at 8 holds the Exif pointer (2 + 12 + 4 = 18 bytes), so the Exif IFD is at 26; it holds
    // one entry and ends at 44, where the 20 bytes of text go.
    t.extend(1u16.to_le_bytes());
    t.extend(0x8769u16.to_le_bytes());
    t.extend(4u16.to_le_bytes());
    t.extend(1u32.to_le_bytes());
    t.extend(26u32.to_le_bytes());
    t.extend(0u32.to_le_bytes());
    t.extend(1u16.to_le_bytes());
    t.extend(0x9003u16.to_le_bytes());
    t.extend(2u16.to_le_bytes());
    t.extend(20u32.to_le_bytes());
    t.extend(44u32.to_le_bytes());
    t.extend(0u32.to_le_bytes());
    t.extend(date.as_bytes());
    t.push(0);
    t
}

/// A JPEG for tests whose Exif segment is `test_tiff(date)`.
#[cfg(test)]
pub fn test_jpeg(date: &str) -> Vec<u8> {
    let mut block = HEADER.to_vec();
    block.extend(test_tiff(date));
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
    jpeg.extend(((block.len() + 2) as u16).to_be_bytes());
    jpeg.extend(block);
    jpeg.extend([0xFF, 0xD9]);
    jpeg
}

/// `jpeg` with the EXIF block right after its start marker, where readers look for it.
pub fn insert(jpeg: &[u8], exif: &[u8]) -> Vec<u8> {
    debug_assert!(jpeg.starts_with(&SOI));
    let mut out = Vec::with_capacity(jpeg.len() + exif.len() + 4);
    out.extend_from_slice(&SOI);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(exif);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little-endian TIFF: IFD0 (Orientation 6, ExifIFD pointer), an Exif IFD (pixel dimensions
    /// as SHORTs, ColorSpace 65535), and an IFD1 whose thumbnail ends the block.
    fn sample() -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend(8u32.to_le_bytes());
        let entry = |t: &mut Vec<u8>, tag: u16, ty: u16, v: u32| {
            t.extend(tag.to_le_bytes());
            t.extend(ty.to_le_bytes());
            t.extend(1u32.to_le_bytes());
            t.extend(v.to_le_bytes());
        };
        // IFD0 at 8: 2 entries, ends at 38; the Exif IFD at 38 has 3 entries.
        t.extend(2u16.to_le_bytes());
        entry(&mut t, 0x0112, 3, 6);
        entry(&mut t, 0x8769, 4, 38);
        t.extend(80u32.to_le_bytes()); // IFD1 at 38 + 2 + 36 + 4 = 80
        t.extend(3u16.to_le_bytes());
        entry(&mut t, 0xA001, 3, 65535);
        entry(&mut t, 0xA002, 3, 5184);
        entry(&mut t, 0xA003, 3, 3888);
        t.extend(0u32.to_le_bytes());
        // IFD1 at 80: 2 entries (thumbnail offset 80 + 2 + 24 + 4 = 110, length 5).
        assert_eq!(t.len(), 80);
        t.extend(2u16.to_le_bytes());
        entry(&mut t, 0x0201, 4, 110);
        entry(&mut t, 0x0202, 4, 5);
        t.extend(0u32.to_le_bytes());
        t.extend([1, 2, 3, 4, 5]);
        t
    }

    #[test]
    fn patches_what_the_export_changes() {
        let out = patch(&sample(), 3888, 5184).unwrap();
        let t = Tiff { buf: out, little: true };
        assert_eq!(t.u16_at(t.find(8, 0x0112).unwrap() + 8), Some(1));
        let exif = t.u32_at(t.find(8, 0x8769).unwrap() + 8).unwrap() as usize;
        assert_eq!(t.u16_at(t.find(exif, 0xA001).unwrap() + 8), Some(1));
        assert_eq!(t.u16_at(t.find(exif, 0xA002).unwrap() + 8), Some(3888));
        assert_eq!(t.u16_at(t.find(exif, 0xA003).unwrap() + 8), Some(5184));
        // No thumbnail directory, and its bytes are gone.
        assert_eq!(t.u32_at(t.entries(8).unwrap().1), Some(0));
        assert_eq!(t.buf.len(), 110);
    }

    #[test]
    fn widens_dimensions_that_no_longer_fit_a_short() {
        let out = patch(&sample(), 70000, 5184).unwrap();
        let t = Tiff { buf: out, little: true };
        let exif = t.u32_at(t.find(8, 0x8769).unwrap() + 8).unwrap() as usize;
        let e = t.find(exif, 0xA002).unwrap();
        assert_eq!((t.u16_at(e + 2), t.u32_at(e + 8)), (Some(4), Some(70000)));
    }

    #[test]
    fn refuses_what_is_not_tiff() {
        assert!(patch(b"not exif", 10, 10).is_none());
    }


    /// A raw's directories, as Olympus lays them out: IFD0 with Make (out of line), Model,
    /// Orientation and the Exif pointer; an Exif directory with an exposure time and a maker note.
    #[test]
    fn describes_the_shot_from_a_raws_own_directories() {
        let entry = |t: &mut Vec<u8>, tag: u16, ty: u16, count: u32, v: u32| {
            t.extend(tag.to_le_bytes());
            t.extend(ty.to_le_bytes());
            t.extend(count.to_le_bytes());
            t.extend(v.to_le_bytes());
        };
        let mut t = b"IIRO".to_vec();
        t.extend(8u32.to_le_bytes());
        // IFD0 at 8: 4 entries (2 + 48 + 4 = 54), Exif IFD at 62: 2 entries (2 + 24 + 4 = 30);
        // values from 92: "OLYMPUS\0" (8), the exposure time 1/250 (8).
        t.extend(4u16.to_le_bytes());
        entry(&mut t, 0x010F, 2, 8, 92);
        entry(&mut t, 0x0110, 2, 2, u32::from_le_bytes([b'X', 0, 0, 0]));
        entry(&mut t, 0x0112, 3, 1, 6);
        entry(&mut t, 0x8769, 4, 1, 62);
        t.extend(0u32.to_le_bytes());
        t.extend(2u16.to_le_bytes());
        entry(&mut t, 0x829A, 5, 1, 100);
        entry(&mut t, 0x927C, 7, 4, u32::from_le_bytes(*b"OLYM"));
        t.extend(0u32.to_le_bytes());
        t.extend(b"OLYMPUS\0");
        t.extend(1u32.to_le_bytes());
        t.extend(250u32.to_le_bytes());

        let out = from_tiff(&t, 3888, 5184).unwrap();
        let o = Tiff { buf: out, little: true };
        let ifd0 = o.u32_at(4).unwrap() as usize;
        let tags: Vec<_> = o.tags(ifd0);
        let make = tags.iter().find(|t| t.tag == 0x010F).unwrap();
        assert_eq!(make.data, b"OLYMPUS\0");
        assert_eq!(o.u16_at(o.find(ifd0, 0x0112).unwrap() + 8), Some(1));
        let exif = o.u32_at(o.find(ifd0, 0x8769).unwrap() + 8).unwrap() as usize;
        assert_eq!(o.u16_at(o.find(ifd0, 0x8769).unwrap() + 2), Some(4), "the pointer must be a LONG for readers to follow it");
        let exif_tags = o.tags(exif);
        let seconds = exif_tags.iter().find(|t| t.tag == 0x829A).unwrap();
        assert_eq!([o.u32_of(&seconds.data[..4]), o.u32_of(&seconds.data[4..])], [Some(1), Some(250)]);
        assert!(exif_tags.iter().all(|t| t.tag != 0x927C), "the maker note's offsets would point into the raw");
        assert_eq!(o.u16_at(o.find(exif, 0xA002).unwrap() + 8), Some(3888));
        assert_eq!(o.u16_at(o.find(exif, 0xA003).unwrap() + 8), Some(5184));
        assert_eq!(o.u32_at(o.entries(ifd0).unwrap().1), Some(0));
    }
    #[test]
    fn reads_the_capture_time_from_a_raw_and_a_jpeg() {
        let dir = std::env::temp_dir().join(format!("windedit-exif-time-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.ORF"), test_tiff("2026:06:06 14:03:09")).unwrap();
        std::fs::write(dir.join("b.jpg"), test_jpeg("2025:12:31 23:59:58")).unwrap();
        std::fs::write(dir.join("c.RW2"), test_tiff("0000:00:00 00:00:00")).unwrap();
        std::fs::write(dir.join("d.jpg"), b"not a photo").unwrap();
        let at = |name: &str| capture_time(&dir.join(name));
        let (a, b) = (at("a.ORF").unwrap(), at("b.jpg").unwrap());
        assert_eq!((a.day_name(), a.hour, a.minute, a.second), ("2026-06-06".to_string(), 14, 3, 9));
        assert_eq!(b.day_name(), "2025-12-31");
        assert!(a > b);
        assert_eq!(at("c.RW2"), None, "an unset clock is no date");
        assert_eq!(at("d.jpg"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exif_goes_right_after_the_start_marker() {
        let out = insert(&[0xFF, 0xD8, 0xFF, 0xDB, 0, 2], b"Exif\0\0xyz");
        assert_eq!(&out[..6], &[0xFF, 0xD8, 0xFF, 0xE1, 0, 11]);
        assert_eq!(&out[out.len() - 4..], &[0xFF, 0xDB, 0, 2]);
    }
}
