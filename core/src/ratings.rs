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
use crate::develop::Edit;
use crate::xmp::{self, Metadata};

/// One photo's undo history. `committed` is what's saved; a drag in progress may show
/// something else until it's let go.
struct History {
    committed: Edit,
    past: Vec<Edit>,
    future: Vec<Edit>,
}

impl History {
    fn new(committed: Edit) -> History {
        History { committed, past: Vec::new(), future: Vec::new() }
    }
}

/// Lightroom's label names, in its 6–9 key order (Purple has no key).
pub const LABELS: [&str; 5] = ["Red", "Yellow", "Green", "Blue", "Purple"];

pub struct Ratings {
    values: Mutex<Vec<Metadata>>,
    /// Undo and redo for each photo's adjustments, for this session.
    history: Mutex<HashMap<usize, History>>,
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
            history: Mutex::new(HashMap::new()),
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
        self.history.lock().unwrap().clear();
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

    pub fn edit(&self, index: usize) -> Option<Edit> {
        self.values.lock().unwrap().get(index).map(|m| m.edit)
    }

    /// Changes a photo's adjustments. While a slider is being dragged, `commit` is false: the
    /// photo shows the change but nothing is saved or added to the history. When it's let go,
    /// `commit` is true: the change becomes one undo step and is saved.
    pub fn set_edit(&self, index: usize, edit: Edit, commit: bool) -> bool {
        let committed_before = {
            let mut values = self.values.lock().unwrap();
            let Some(meta) = values.get_mut(index) else { return false };
            let mut history = self.history.lock().unwrap();
            let h = history.entry(index).or_insert_with(|| History::new(meta.edit));
            meta.edit = edit;
            if !commit || edit == h.committed {
                return true;
            }
            h.past.push(h.committed);
            h.future.clear();
            h.committed = edit;
            true
        };
        if committed_before {
            self.save(index);
        }
        true
    }

    /// Steps a photo's adjustments back (or forward, for redo). Returns the adjustments now in
    /// effect, or None if there was nothing to undo (or redo).
    pub fn undo(&self, index: usize, redo: bool) -> Option<Edit> {
        let edit = {
            let mut values = self.values.lock().unwrap();
            let meta = values.get_mut(index)?;
            let mut history = self.history.lock().unwrap();
            let h = history.entry(index).or_insert_with(|| History::new(meta.edit));
            let (from, to) = if redo { (&mut h.future, &mut h.past) } else { (&mut h.past, &mut h.future) };
            let edit = from.pop()?;
            to.push(h.committed);
            h.committed = edit;
            meta.edit = edit;
            edit
        };
        self.save(index);
        Some(edit)
    }

    fn save(&self, index: usize) {
        let Some(meta) = self.get(index) else { return };
        let path = self.sidecars.lock().unwrap()[index].clone();
        if let Some(writer) = &*self.writer.lock().unwrap() {
            let _ = writer.send((path, meta));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drags_commit_once_and_undo_redo() {
        let dir = std::env::temp_dir().join(format!("windedit-ratings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let photos = vec![Photo { name: "P1".into(), raw: None, jpeg: None }];
        let r = Ratings::new();
        r.load(&dir, &photos);
        let at = |ev: f32| Edit { exposure: ev, ..Default::default() };

        // A drag: many uncommitted steps, then one commit.
        for i in 1..=10 {
            r.set_edit(0, at(i as f32 * 0.1), false);
        }
        r.set_edit(0, at(1.0), true);
        r.set_edit(0, at(2.0), true);
        assert_eq!(r.edit(0).unwrap().exposure, 2.0);

        assert_eq!(r.undo(0, false).unwrap().exposure, 1.0);
        assert_eq!(r.undo(0, false).unwrap().exposure, 0.0, "the drag was one step");
        assert!(r.undo(0, false).is_none());
        assert_eq!(r.undo(0, true).unwrap().exposure, 1.0);

        // A new change after undoing drops what could have been redone.
        r.set_edit(0, at(-1.0), true);
        assert!(r.undo(0, true).is_none());

        r.finish();
        let saved = xmp::read(&xmp::sidecar_path(&dir, "P1"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(saved.edit.exposure, -1.0, "the last committed change is saved");
    }
}
