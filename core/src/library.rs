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
    scan_folders(&[dir.to_path_buf()])
}

/// The photos in several folders (a card's DCIM subfolders), each folder's in file name order,
/// one folder after the next. A raw and a JPEG pair up only within their own folder: cameras
/// restart their numbering in a new folder, so `P1020001` can be two different photos.
pub fn scan_folders(dirs: &[PathBuf]) -> std::io::Result<Vec<Photo>> {
    let mut all = Vec::new();
    for dir in dirs {
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
        all.extend(photos.into_values());
    }
    Ok(all)
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

    #[test]
    fn several_folders_keep_same_named_photos_apart() {
        let dir = std::env::temp_dir().join(format!("windedit-library-multi-{}", std::process::id()));
        let (a, b) = (dir.join("100OMSYS"), dir.join("101OMSYS"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for path in [a.join("P1.ORF"), a.join("P1.JPG"), b.join("P1.ORF")] {
            std::fs::write(path, b"").unwrap();
        }
        let photos = scan_folders(&[a.clone(), b.clone()]).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(photos.len(), 2, "the same name in two folders is two photos");
        assert!(photos[0].jpeg.is_some() && photos[1].jpeg.is_none());
        assert_eq!(photos[1].raw.as_ref().unwrap().parent().unwrap(), b);
    }
}
