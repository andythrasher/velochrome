//! The GPU thread's part of developed previews (docs/edited-previews.md): noting committed
//! edits, rendering the photo in view's thumbnail and preview when things have been quiet, and
//! developing edited photos that have no preview in the background. Rendering happens here
//! because the develop pass does; encoding and filing are done by a thread of their own.

use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use super::Viewer;
use crate::decode::RawFrame;
use crate::developed::{self, Cache};
use crate::develop::Edit;
use crate::jobs::{Jobs, THUMB_EDGE, Thumbnail};
use crate::masks::{Masks, StrokeMap, Strokes};
use crate::pipeline::{Image, Zoom};
use crate::preview::Preview;

/// How long after a committed change the photo's thumbnail is rendered again, and how long
/// after the last change (with no more coming) its full preview is.
const LIVE_DELAY: Duration = Duration::from_millis(500);
const FULL_DELAY: Duration = Duration::from_secs(3);
/// The backlog of edited photos only gets the GPU thread when the user has been still this long.
const BACKLOG_IDLE: Duration = Duration::from_millis(300);

/// A committed edit of the photo in view whose preview isn't made yet.
pub(super) struct Pending {
    index: usize,
    edit: Edit,
    masks: Masks,
    strokes: Strokes,
    /// What the preview is called in the cache.
    key: developed::Key,
    committed: Instant,
    /// The thumbnail has been rendered for this edit.
    thumb_done: bool,
}

/// What the writer thread is asked to do.
pub(super) enum WriterJob {
    /// Encode the preview and its thumbnail, file them, and tell the jobs.
    Full { index: usize, key: developed::Key, preview: Arc<Preview> },
    /// Make a thumbnail from a small render: it is only shown, not filed.
    Thumb { index: usize, key: developed::Key, preview: Preview },
}

/// The thread that encodes and writes developed previews, so the GPU thread only renders.
pub(super) fn spawn_writer(jobs: Arc<Jobs>) -> Sender<WriterJob> {
    let (tx, rx) = channel::<WriterJob>();
    std::thread::Builder::new()
        .name("windedit previews".into())
        .spawn(move || {
            crate::sys::set_background(true);
            let cache: Arc<Cache> = jobs.cache().clone();
            for job in rx {
                match job {
                    WriterJob::Thumb { index, key, preview } => {
                        let thumb = Thumbnail::from_preview(&preview, THUMB_EDGE);
                        jobs.developed_thumb(index, &key, Arc::new(thumb));
                    }
                    WriterJob::Full { index, key, preview } => {
                        let done = developed::encode_preview(&preview, THUMB_EDGE)
                            .map_err(|e| e.to_string())
                            .and_then(|e| cache.write(&key, &e.preview, &e.thumb_jpeg).map(|()| e.thumb).map_err(|e| e.to_string()));
                        match done {
                            Ok(thumb) => jobs.developed_written(index, &key, true, Some(Arc::new(thumb))),
                            Err(message) => {
                                eprintln!("windedit: couldn't file a developed preview: {message}");
                                jobs.developed_written(index, &key, false, None);
                            }
                        }
                    }
                }
            }
        })
        .expect("couldn't start the preview thread");
    tx
}

impl Viewer {
    /// Whether the photo in view has an edit to show.
    pub(super) fn is_edited(&self) -> bool {
        !self.edit.is_default() || !self.masks.is_empty()
    }

    /// A change to the photo in view was committed: note it, to render its thumbnail and then its
    /// preview when things have been quiet. (Other photos' edits are left to the backlog.)
    pub(super) fn committed(&mut self, index: usize) {
        if Some(index) != self.focus {
            return;
        }
        match self.jobs.developed_id(index) {
            Some(id) if self.is_edited() => {
                let key = developed::key(id, developed::fingerprint(&self.edit, &self.masks, &self.strokes));
                self.pending = Some(Pending {
                    index,
                    edit: self.edit,
                    masks: self.masks,
                    strokes: self.strokes.clone(),
                    key,
                    committed: Instant::now(),
                    thumb_done: false,
                });
            }
            _ => self.pending = None, // reset to nothing, or no raw to develop
        }
    }

    /// How long the thread may sleep before it has background work: None for as long as it likes.
    pub(super) fn idle_wait(&self) -> Option<Duration> {
        let mut wait: Option<Duration> = None;
        let mut soonest = |d: Duration| wait = Some(wait.map_or(d, |w| w.min(d)));
        if self.full_preview_to_upload().is_some() {
            soonest(Duration::from_millis(150));
        }
        if let Some(p) = &self.pending {
            soonest(if p.thumb_done { FULL_DELAY } else { LIVE_DELAY }.saturating_sub(p.committed.elapsed()));
        }
        if self.backlog_possible() {
            soonest(BACKLOG_IDLE.saturating_sub(self.last_activity.elapsed()));
        }
        wait
    }

    /// Does the background work that is due. Always settles whatever made it due, so the loop
    /// never spins.
    pub(super) fn idle_work(&mut self) {
        if let Err(e) = self.upload_full_preview() {
            eprintln!("windedit: uploading a preview failed: {e}");
        }
        if let Some(p) = &self.pending {
            let thumb_done = p.thumb_done;
            if p.committed.elapsed() >= if thumb_done { FULL_DELAY } else { LIVE_DELAY } {
                if thumb_done {
                    self.develop_pending();
                } else {
                    self.develop_pending_thumb();
                }
                return;
            }
        }
        if self.backlog_possible() && self.last_activity.elapsed() >= BACKLOG_IDLE {
            self.develop_backlog();
        }
    }

    /// Whether an edited photo without a preview is waiting, and nothing else needs this thread:
    /// not Edit mode or 100% (which use the GPU's copy of the raw), no export.
    fn backlog_possible(&self) -> bool {
        !self.edit_mode
            && !self.crop_mode
            && matches!(self.zoom, Zoom::Fit)
            && self.export.is_none()
            && self.exports.is_empty()
            && self.pending.is_none()
            && self.jobs.next_backlog().is_some()
    }

    /// Whether the GPU holds this photo's raw.
    fn has_image(&self, index: usize) -> bool {
        matches!(&self.image, Some((i, _)) if *i == index)
    }

    /// Renders the pending edit's thumbnail (small, so quick) for the filmstrip to show.
    fn develop_pending_thumb(&mut self) {
        let Some(p) = &mut self.pending else { return };
        p.thumb_done = true;
        let Some((_, image)) = self.image.as_ref().filter(|(i, _)| *i == p.index) else { return };
        match self.live_renderer.render(&mut self.gpu, image, &p.edit, &p.masks, &p.strokes, developed::LIVE_EDGE) {
            Ok(r) => {
                let _ = self.writer.send(WriterJob::Thumb { index: p.index, key: p.key.clone(), preview: r.into_preview() });
            }
            Err(e) => eprintln!("windedit: rendering a thumbnail failed: {e}"),
        }
    }

    /// Renders the pending edit's full preview and hands it on to be filed. If the GPU no longer
    /// holds the photo, the backlog makes it later.
    pub(super) fn develop_pending(&mut self) {
        let Some(p) = self.pending.take() else { return };
        if !self.has_image(p.index) || !self.jobs.developed_begun(p.index, &p.key) {
            return;
        }
        let (_, image) = self.image.as_ref().unwrap();
        let rendered = self.full_renderer.render(&mut self.gpu, image, &p.edit, &p.masks, &p.strokes, developed::PREVIEW_EDGE);
        self.finish_render(p.index, p.key, rendered);
    }

    /// Develops the most wanted edited photo that has no preview, from its decoded raw.
    fn develop_backlog(&mut self) {
        let Some((index, key, frame)) = self.jobs.next_backlog() else { return };
        // What the photo is now, from the ratings: the jobs' key is only a name for it.
        let (Some(meta), Some(id)) = (self.ratings.get(index), self.jobs.developed_id(index)) else {
            self.jobs.developed_written(index, &key, false, None);
            return;
        };
        let Some(fingerprint) = developed::fingerprint_of(&meta) else {
            self.jobs.set_developed(index, None);
            return;
        };
        if developed::key(id, fingerprint) != key {
            self.jobs.set_developed(index, Some(fingerprint)); // the jobs were behind: catch up
            return;
        }
        if !self.jobs.developed_begun(index, &key) {
            return;
        }
        let rendered = self.open_for_backlog(index, &frame).and_then(|()| {
            let (_, image) = self.image.as_ref().unwrap();
            let none = StrokeMap::new();
            let masks = meta.masks.as_deref().copied().unwrap_or_default();
            self.full_renderer.render(&mut self.gpu, image, &meta.edit, &masks, meta.strokes.as_deref().unwrap_or(&none), developed::PREVIEW_EDGE)
        });
        self.jobs.release_backlog_raw(index);
        self.finish_render(index, key, rendered);
    }

    /// Puts the photo's raw on the GPU (in the place of whatever is there, which nothing in view needs).
    fn open_for_backlog(&mut self, index: usize, frame: &Arc<RawFrame>) -> windows::core::Result<()> {
        if self.has_image(index) {
            return Ok(());
        }
        self.image = None; // its memory may be about to be reused
        let (image, _) = Image::open(&mut self.gpu, &mut self.memory, frame)?;
        self.image = Some((index, image));
        Ok(())
    }

    /// A developed preview came out of the renderer (or didn't): show it if it is the photo in
    /// view, and have it filed.
    fn finish_render(&mut self, index: usize, key: developed::Key, rendered: windows::core::Result<developed::Rendered>) {
        match rendered {
            Ok(r) => {
                let preview = Arc::new(r.into_preview());
                self.jobs.developed_preview(index, &key, preview.clone());
                let _ = self.writer.send(WriterJob::Full { index, key, preview });
                if Some(index) == self.focus {
                    self.dirty = true;
                }
            }
            Err(e) => {
                eprintln!("windedit: developing a preview failed: {e}");
                self.jobs.developed_written(index, &key, false, None);
            }
        }
    }
}
