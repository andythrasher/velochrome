//! Stars, color labels and pick flags for the photos in a folder: read from their XMP sidecars
//! when the folder opens, changed in memory at once, and written back on a background thread
//! so a keypress never waits for the disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use rayon::prelude::*;

use crate::library::Photo;
use crate::xmp::{self, Metadata};

/// Lightroom's label names, in its 6–9 key order (Purple has no key).
pub const LABELS: [&str; 5] = ["Red", "Yellow", "Green", "Blue", "Purple"];

pub struct Ratings {
    values: Mutex<Vec<Metadata>>,
    sidecars: Mutex<Vec<PathBuf>>,
    writer: Mutex<Option<Sender<(PathBuf, Metadata)>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Default for Ratings {
    fn default() -> Self {
        Self::new()
    }
}

impl Ratings {
    pub fn new() -> Ratings {
        let (tx, rx) = channel();
        let thread = std::thread::Builder::new()
            .name("windedit sidecars".into())
            .spawn(move || write_sidecars(rx))
            .expect("couldn't start the sidecar thread");
        Ratings {
            values: Mutex::new(Vec::new()),
            sidecars: Mutex::new(Vec::new()),
            writer: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
        }
    }

    /// Reads the sidecars of a folder's photos (in parallel; most photos have none).
    pub fn load(&self, dir: &Path, photos: &[Photo]) {
        let sidecars: Vec<PathBuf> = photos.iter().map(|p| xmp::sidecar_path(dir, &p.name)).collect();
        let values: Vec<Metadata> = sidecars.par_iter().map(|path| xmp::read(path)).collect();
        *self.values.lock().unwrap() = values;
        *self.sidecars.lock().unwrap() = sidecars;
    }

    pub fn get(&self, index: usize) -> Option<Metadata> {
        self.values.lock().unwrap().get(index).cloned()
    }

    pub fn all(&self) -> Vec<Metadata> {
        self.values.lock().unwrap().clone()
    }

    /// Changes a photo's metadata now, and queues its sidecar to be written.
    pub fn set(&self, index: usize, change: impl FnOnce(&mut Metadata)) -> bool {
        let meta = {
            let mut values = self.values.lock().unwrap();
            let Some(meta) = values.get_mut(index) else { return false };
            let before = meta.clone();
            change(meta);
            if *meta == before {
                return true;
            }
            meta.clone()
        };
        let path = self.sidecars.lock().unwrap()[index].clone();
        if let Some(writer) = &*self.writer.lock().unwrap() {
            let _ = writer.send((path, meta));
        }
        true
    }

    /// Writes everything queued and stops the writer. Called when the engine shuts down.
    pub fn finish(&self) {
        drop(self.writer.lock().unwrap().take());
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

/// Writes sidecars as changes arrive. A burst of changes to one photo (tapping through star
/// ratings) becomes one write of the last.
fn write_sidecars(rx: Receiver<(PathBuf, Metadata)>) {
    crate::sys::set_background(true);
    while let Ok(first) = rx.recv() {
        let mut latest: HashMap<PathBuf, Metadata> = HashMap::new();
        latest.insert(first.0, first.1);
        while let Ok((path, meta)) = rx.try_recv() {
            latest.insert(path, meta);
        }
        for (path, meta) in latest {
            if let Err(e) = xmp::write(&path, &meta) {
                eprintln!("windedit: couldn't save {}: {e}", path.display());
            }
        }
    }
}

/// A label name's number for the C interface: 1–5 for Lightroom's labels, 6 for any other
/// name (kept as it is), 0 for none.
pub fn label_code(label: &Option<String>) -> i32 {
    match label {
        None => 0,
        Some(name) => LABELS.iter().position(|l| l.eq_ignore_ascii_case(name)).map_or(6, |i| i as i32 + 1),
    }
}
