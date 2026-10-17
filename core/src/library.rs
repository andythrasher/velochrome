//! A folder of photos. A raw and a JPEG with the same name (the camera's RAW+JPEG mode) are one
//! photo, as in Lightroom.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const RAW_EXTENSIONS: &[&str] = &["orf", "rw2", "dng"];
const JPEG_EXTENSIONS: &[&str] = &["jpg", "jpeg"];

#[derive(Debug, Clone)]
pub struct Photo {
    /// File name without extension, which the raw and JPEG share.
    pub name: String,
    pub raw: Option<PathBuf>,
    pub jpeg: Option<PathBuf>,
}

/// The photos in a folder (not its subfolders), in file name order.
pub fn scan_folder(dir: &Path) -> std::io::Result<Vec<Photo>> {
    // Keyed by lowercase name so P1020943.RW2 pairs with p1020943.jpg.
    let mut photos: BTreeMap<String, Photo> = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let (Some(stem), Some(ext)) = (path.file_stem().and_then(|s| s.to_str()), path.extension().and_then(|e| e.to_str()))
        else {
            continue;
        };
        let ext = ext.to_ascii_lowercase();
        let is_raw = RAW_EXTENSIONS.contains(&ext.as_str());
        if !is_raw && !JPEG_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        let photo = photos
            .entry(stem.to_lowercase())
            .or_insert_with(|| Photo { name: stem.to_string(), raw: None, jpeg: None });
        if is_raw {
            photo.raw = Some(path);
        } else {
            photo.jpeg = Some(path);
        }
    }
    Ok(photos.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_raw_and_jpeg_by_name() {
        let dir = std::env::temp_dir().join(format!("windedit-library-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["P1.RW2", "p1.jpg", "P2.ORF", "P3.JPG", "notes.txt"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let photos = scan_folder(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let summary: Vec<_> = photos.iter().map(|p| (p.name.as_str(), p.raw.is_some(), p.jpeg.is_some())).collect();
        assert_eq!(summary.len(), 3);
        assert!(summary.contains(&("P2", true, false)));
        assert!(summary.contains(&("P3", false, true)));
        assert!(summary.iter().any(|&(n, raw, jpeg)| n.eq_ignore_ascii_case("p1") && raw && jpeg));
    }
}
