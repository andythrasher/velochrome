//! Stars, color labels and pick flags for the photos in a folder: read from their XMP sidecars
//! when the folder opens, changed in memory at once, and written back on a background thread
//! so a keypress never waits for the disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use rayon::prelude::*;

use crate::library::Photo;
use crate::develop::Edit;
use crate::masks::Masks;
use crate::xmp::{self, Metadata};

/// What undo steps through: the adjustments and the masks together, so one step covers whichever
/// changed. The masks are shared, so a snapshot costs a pointer (and none at all for a photo
/// without masks).
pub type State = (Edit, Option<Arc<Masks>>);

/// One photo's undo history. `committed` is what's saved; a drag in progress may show
/// something else until it's let go.
struct History {
    committed: State,
    past: Vec<State>,
    future: Vec<State>,
}

impl History {
    fn new(committed: State) -> History {
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

    /// A photo's masks (none for a photo without).
    pub fn masks(&self, index: usize) -> Option<Masks> {
        self.values.lock().unwrap().get(index).map(|m| m.masks.as_deref().copied().unwrap_or_default())
    }

    /// Changes a photo's adjustments. While a slider is being dragged, `commit` is false: the
    /// photo shows the change but nothing is saved or added to the history. When it's let go,
    /// `commit` is true: the change becomes one undo step and is saved.
    pub fn set_edit(&self, index: usize, edit: Edit, commit: bool) -> bool {
        self.set_state(index, commit, |state| state.0 = edit)
    }

    /// Like [`set_edit`](Self::set_edit), for the photo's masks.
    pub fn set_masks(&self, index: usize, masks: Masks, commit: bool) -> bool {
        self.set_state(index, commit, |state| state.1 = (!masks.is_empty()).then(|| Arc::new(masks)))
    }

    fn set_state(&self, index: usize, commit: bool, change: impl FnOnce(&mut State)) -> bool {
        {
            let mut values = self.values.lock().unwrap();
            let Some(meta) = values.get_mut(index) else { return false };
            let mut history = self.history.lock().unwrap();
            let h = history.entry(index).or_insert_with(|| History::new((meta.edit, meta.masks.clone())));
            let mut state = (meta.edit, meta.masks.clone());
            change(&mut state);
            (meta.edit, meta.masks) = state.clone();
            if !commit || state == h.committed {
                return true;
            }
            h.past.push(std::mem::replace(&mut h.committed, state));
            h.future.clear();
        }
        self.save(index);
        true
    }

    /// Steps a photo's adjustments and masks back (or forward, for redo). Returns what is now in
    /// effect, or None if there was nothing to undo (or redo).
    pub fn undo(&self, index: usize, redo: bool) -> Option<(Edit, Masks)> {
        let state = {
            let mut values = self.values.lock().unwrap();
            let meta = values.get_mut(index)?;
            let mut history = self.history.lock().unwrap();
            let h = history.entry(index).or_insert_with(|| History::new((meta.edit, meta.masks.clone())));
            let (from, to) = if redo { (&mut h.future, &mut h.past) } else { (&mut h.past, &mut h.future) };
            let state = from.pop()?;
            to.push(std::mem::replace(&mut h.committed, state.clone()));
            (meta.edit, meta.masks) = state.clone();
            state
        };
        self.save(index);
        Some((state.0, state.1.as_deref().copied().unwrap_or_default()))
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

        assert_eq!(r.undo(0, false).unwrap().0.exposure, 1.0);
        assert_eq!(r.undo(0, false).unwrap().0.exposure, 0.0, "the drag was one step");
        assert!(r.undo(0, false).is_none());
        assert_eq!(r.undo(0, true).unwrap().0.exposure, 1.0);

        // A new change after undoing drops what could have been redone.
        r.set_edit(0, at(-1.0), true);
        assert!(r.undo(0, true).is_none());

        r.finish();
        let saved = xmp::read(&xmp::sidecar_path(&dir, "P1"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(saved.edit.exposure, -1.0, "the last committed change is saved");
    }

    #[test]
    fn undo_covers_edit_and_masks() {
        use crate::masks::{Component, KIND_RADIAL, Mask};
        let dir = std::env::temp_dir().join(format!("windedit-ratings-masks-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let photos = vec![Photo { name: "P1".into(), raw: None, jpeg: None }];
        let r = Ratings::new();
        r.load(&dir, &photos);
        let with_mask = |ev: f32| {
            let mut m = Masks::default();
            m.masks[0] = Mask::new();
            m.masks[0].components[0] = Component { kind: KIND_RADIAL, p: [0.5, 0.4, 0.2, 0.2, 0.0, 50.0, 0.0, 0.0], ..Default::default() };
            m.masks[0].count = 1;
            m.masks[0].sliders[2] = ev;
            m.count = 1;
            m
        };
        assert!(r.masks(0).unwrap().is_empty());
        r.set_masks(0, with_mask(0.5), false); // a drag: shown, not a step
        r.set_masks(0, with_mask(1.0), true);
        r.set_edit(0, Edit { exposure: 1.0, ..Default::default() }, true);
        r.set_masks(0, with_mask(2.0), true);
        assert_eq!(r.masks(0).unwrap().masks[0].sliders[2], 2.0);

        let (e, m) = r.undo(0, false).unwrap();
        assert_eq!((e.exposure, m.masks[0].sliders[2]), (1.0, 1.0));
        let (e, m) = r.undo(0, false).unwrap();
        assert_eq!((e.exposure, m.masks[0].sliders[2]), (0.0, 1.0), "the edit alone was one step");
        let (e, m) = r.undo(0, false).unwrap();
        assert!(e.is_default() && m.is_empty(), "and the first mask change one more");
        assert!(r.undo(0, false).is_none());
        assert_eq!(r.undo(0, true).unwrap().1.masks[0].sliders[2], 1.0);
        // Committing what is already there is not a step (and keeps what can be redone).
        r.set_masks(0, with_mask(1.0), true);
        assert!(r.undo(0, true).is_some());
        r.set_masks(0, Masks::default(), true);
        assert!(r.masks(0).unwrap().is_empty());
        assert!(r.undo(0, false).unwrap().1.masks[0].sliders[2] != 0.0, "removing them is a step too");

        r.set_masks(0, with_mask(3.0), true);
        r.finish();
        let saved = xmp::read(&xmp::sidecar_path(&dir, "P1"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(saved.masks.as_deref().unwrap().masks[0].sliders[2], 3.0, "the masks are saved with the edit");
    }
}
