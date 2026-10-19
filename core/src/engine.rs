//! The engine as apps see it: open a folder, show a photo, zoom, edit. Calls return at once;
//! decoding happens on worker threads and drawing on the GPU thread, which reports each frame
//! through the event callback.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::jobs::{self, Focus, Jobs};
use crate::library::{self, Photo};
use crate::developed::{self, Cache};
use crate::ratings::Ratings;
use crate::develop::{self, Edit};
use crate::masks::{DabWalker, Masks, Stroke};
use crate::pipeline::Zoom;
use crate::export::Request;
use crate::import::{self, Import, Plan};
use crate::viewer::{self, Command, EventSink, ExportJob, FrameEvent, GpuInfo};

pub struct Engine {
    jobs: Arc<Jobs>,
    viewer: Sender<Command>,
    gpu_thread: Option<JoinHandle<()>>,
    pub info: GpuInfo,
    view: Mutex<ViewState>,
    ratings: Arc<Ratings>,
    next_export: AtomicU32,
    /// The sink the app gave us: shared, because the card import reports from its own thread.
    events: Arc<Mutex<EventSink>>,
    import_plan: Mutex<Option<Arc<Plan>>>,
    import: Mutex<Option<Import>>,
    /// The brush stroke being painted, if any.
    live: Mutex<Option<LiveStroke>>,
}

/// A brush stroke under way: what it has so far, and the walker that spaces its dabs.
struct LiveStroke {
    index: usize,
    id: u32,
    stroke: Stroke,
    walker: DabWalker,
}

/// The settings a stroke is painted with (`Stroke::new`).
#[derive(Debug, Clone, Copy)]
pub struct Brush {
    pub erase: bool,
    pub radius: f32,
    pub feather: f32,
    pub flow: f32,
    pub density: f32,
}

/// What decides which decodes are wanted.
struct ViewState {
    index: Option<usize>,
    direction: i32,
    view_size: (u32, u32),
    actual_size: bool,
    edit_mode: bool,
    crop_mode: bool,
}

impl Engine {
    /// Starts the decode workers and the GPU thread. `events` is called on the GPU thread after
    /// every frame drawn.
    pub fn new(events: EventSink) -> Result<Engine, String> {
        let events = Arc::new(Mutex::new(events));
        let for_viewer = events.clone();
        let viewer_events: EventSink = Box::new(move |event| (for_viewer.lock().unwrap_or_else(|p| p.into_inner()))(event));
        let (tx, rx) = channel();
        let ready = tx.clone();
        // Counting the cache and trimming it to its limit lists a directory: off this thread.
        let cache = Arc::new(Cache::new(developed::default_dir(), developed::CACHE_LIMIT));
        let counting = cache.clone();
        std::thread::Builder::new()
            .name("windedit cache".into())
            .spawn(move || {
                crate::sys::set_background(true);
                counting.evict();
            })
            .map_err(|e| e.to_string())?;
        let jobs = Jobs::new(
            Box::new(move |index, kind| {
                let _ = ready.send(Command::Ready(index, kind));
            }),
            cache,
        );
        let ratings = Arc::new(Ratings::new());
        let (gpu_thread, info) = match viewer::spawn(jobs.clone(), ratings.clone(), viewer_events, tx.clone(), rx) {
            Ok(started) => started,
            Err(e) => {
                jobs::shutdown(&jobs);
                return Err(e);
            }
        };
        Ok(Engine {
            jobs,
            viewer: tx,
            gpu_thread: Some(gpu_thread),
            info,
            ratings,
            next_export: AtomicU32::new(1),
            events,
            import_plan: Mutex::new(None),
            import: Mutex::new(None),
            live: Mutex::new(None),
            view: Mutex::new(ViewState {
                index: None,
                direction: 1,
                view_size: (2560, 1440),
                actual_size: false,
                edit_mode: false,
                crop_mode: false,
            }),
        })
    }

    /// Opens a folder; returns how many photos it has. Shows nothing until `show`.
    pub fn open_folder(&self, dir: &Path) -> std::io::Result<usize> {
        let photos = library::scan_folder(dir)?;
        let sidecars = photos.iter().map(|p| crate::xmp::sidecar_path(dir, &p.name)).collect();
        Ok(self.open_photos(photos, sidecars))
    }

    /// Makes `photos` the open set, their stars and flags kept in `sidecars` (one each).
    fn open_photos(&self, photos: Vec<Photo>, sidecars: Vec<PathBuf>) -> usize {
        let count = photos.len();
        self.ratings.load_sidecars(sidecars);
        self.view.lock().unwrap().index = None;
        let _ = self.viewer.send(Command::Folder);
        self.jobs.set_photos(Arc::new(photos));
        // Which photos are edited, and have a developed preview in the cache already.
        self.jobs.set_developed_all(self.ratings.all().iter().map(developed::fingerprint_of).collect());
        count
    }

    /// Looks at a camera card (the DCIM folders under `card`) and plans copying it into
    /// `dest_root`. Returns how many photos it has and how many aren't in the library yet.
    /// Reads each photo's date from the card, so it takes a moment: call it off the UI thread.
    pub fn import_scan(&self, card: &Path, dest_root: &Path) -> std::io::Result<(usize, usize)> {
        let plan = import::plan(card, dest_root)?;
        let counts = (plan.items.len(), plan.new_count());
        *self.import_plan.lock().unwrap() = Some(Arc::new(plan));
        Ok(counts)
    }

    /// Opens the scanned card as a set of photos in the order they were shot. Their stars and
    /// flags go to the sidecars in the folders they'll be copied to, never to the card.
    pub fn open_card(&self) -> Result<usize, String> {
        let plan = self.import_plan.lock().unwrap().clone().ok_or("the card hasn't been scanned")?;
        Ok(self.open_photos(plan.photos(), plan.sidecars()))
    }

    /// Starts copying the scanned card on its own thread. Progress and the end arrive as
    /// `EVENT_IMPORT` events.
    pub fn import_start(&self, options: import::Options) -> Result<(), String> {
        let plan = self.import_plan.lock().unwrap().clone().ok_or("the card hasn't been scanned")?;
        let mut running = self.import.lock().unwrap();
        if running.as_ref().is_some_and(Import::is_running) {
            return Err("an import is already running".into());
        }
        let events = self.events.clone();
        *running = Some(Import::start(plan, options, Box::new(move |report| emit_import(&events, report))));
        Ok(())
    }

    /// Stops the copy after the photo in progress's current piece; what's finished stays.
    pub fn import_cancel(&self) {
        if let Some(running) = &*self.import.lock().unwrap() {
            running.cancel();
        }
    }

    pub fn photos(&self) -> Arc<Vec<Photo>> {
        self.jobs.photos()
    }

    pub fn show(&self, index: usize) {
        {
            let mut v = self.view.lock().unwrap();
            if let Some(previous) = v.index
                && previous != index
            {
                v.direction = if index > previous { 1 } else { -1 };
            }
            v.index = Some(index);
            self.refocus(&v);
        }
        // Its adjustments first, so the first frame of it already has them.
        let _ = self.viewer.send(Command::SetEdit(self.ratings.edit(index).unwrap_or_default()));
        let _ = self.viewer.send(Command::SetMasks(Box::new(self.ratings.masks(index).unwrap_or_default())));
        let _ = self.viewer.send(Command::SetStrokes(self.ratings.strokes(index).unwrap_or_default()));
        let _ = self.viewer.send(Command::Show(index));
    }

    pub fn set_zoom(&self, zoom: Zoom) {
        {
            let mut v = self.view.lock().unwrap();
            v.actual_size = matches!(zoom, Zoom::Actual { .. });
            self.refocus(&v);
        }
        let _ = self.viewer.send(Command::Zoom(zoom));
    }

    pub fn set_edit_mode(&self, on: bool) {
        {
            let mut v = self.view.lock().unwrap();
            v.edit_mode = on;
            self.refocus(&v);
        }
        let _ = self.viewer.send(Command::EditMode(on));
    }

    /// In crop mode: which point of the frame (fractions) sits at the view's centre.
    pub fn set_crop_view(&self, x: f32, y: f32) {
        let _ = self.viewer.send(Command::CropView(x, y));
    }

    pub fn set_crop_mode(&self, on: bool) {
        {
            let mut v = self.view.lock().unwrap();
            v.crop_mode = on;
            self.refocus(&v);
        }
        let _ = self.viewer.send(Command::CropMode(on));
    }

    /// Sets the colour (0..1 each) of the empty canvas around the photo.
    pub fn set_canvas_color(&self, rgb: [f32; 3]) {
        let _ = self.viewer.send(Command::Canvas(rgb));
    }

    /// Changes a photo's adjustments: `commit` false while a slider is dragged (shown, not
    /// saved), true when it's let go (saved, and one step for undo).
    pub fn set_edit(&self, index: usize, edit: Edit, commit: bool) -> bool {
        let ok = self.ratings.set_edit(index, edit, commit);
        if ok && self.view.lock().unwrap().index == Some(index) {
            let _ = self.viewer.send(Command::SetEdit(edit));
        }
        if ok && commit {
            self.edit_committed(index);
        }
        ok
    }

    /// A photo's edit has been committed: its developed preview is out of date. (The photo in view
    /// renders its new one when things are quiet; any other is queued for the background.)
    fn edit_committed(&self, index: usize) {
        let fingerprint = self.ratings.get(index).as_ref().and_then(developed::fingerprint_of);
        if self.jobs.set_developed(index, fingerprint) && self.view.lock().unwrap().index == Some(index) {
            let _ = self.viewer.send(Command::Committed(index));
        }
    }

    /// Changes a photo's masks, like [`set_edit`](Self::set_edit): `commit` false while a handle or
    /// a slider is dragged, true when it is let go.
    pub fn set_masks(&self, index: usize, masks: Masks, commit: bool) -> bool {
        let ok = self.ratings.set_masks(index, masks, commit);
        if ok && self.view.lock().unwrap().index == Some(index) {
            let _ = self.viewer.send(Command::SetMasks(Box::new(masks)));
            // A brush component that is gone takes its strokes with it.
            let _ = self.viewer.send(Command::SetStrokes(self.ratings.strokes(index).unwrap_or_default()));
        }
        if ok && commit {
            self.edit_committed(index);
        }
        ok
    }

    /// Paints with the brush: adds the pointer positions `points` (frame units, as masks are) to the
    /// stroke under way in brush `id` of photo `index`, or starts one (`first`) with `brush`'s settings.
    /// The dabs they make show at once; `brush_end` finishes the stroke.
    pub fn brush_paint(&self, index: usize, id: u32, brush: Brush, first: bool, points: &[[f32; 2]]) -> bool {
        let mut live = self.live.lock().unwrap();
        if first || live.as_ref().is_none_or(|l| l.index != index || l.id != id) {
            if !first {
                return false;
            }
            *live = Some(LiveStroke { index, id, stroke: Stroke::new(brush.erase, brush.radius, brush.feather, brush.flow, brush.density), walker: DabWalker::new() });
        }
        let l = live.as_mut().unwrap();
        let mut dabs = Vec::new();
        for p in points {
            // The stroke keeps the pointer's positions to the sidecar's precision, and is painted from those.
            let q = Stroke::quantise(*p);
            if l.stroke.points.last() == Some(&q) {
                continue;
            }
            l.stroke.points.push(q);
            l.walker.feed(l.stroke.radius, q.map(|v| v as f32 * crate::masks::POINT_UNIT), &mut dabs);
        }
        let _ = self.viewer.send(Command::BrushDabs { id, stroke: Box::new(Stroke { points: Vec::new(), ..l.stroke.clone() }), dabs });
        true
    }

    /// Finishes the stroke under way: it joins its brush's strokes as one step for undo, and is saved.
    pub fn brush_end(&self, index: usize) -> bool {
        let Some(l) = self.live.lock().unwrap().take() else { return false };
        if l.index != index {
            return false;
        }
        let ok = self.ratings.add_stroke(index, l.id, l.stroke);
        if ok && self.view.lock().unwrap().index == Some(index) {
            let _ = self.viewer.send(Command::SetStrokes(self.ratings.strokes(index).unwrap_or_default()));
        }
        if ok {
            self.edit_committed(index);
        }
        ok
    }

    /// Shows mask `index` of the photo in view as a red overlay (None: no overlay).
    pub fn set_mask_overlay(&self, index: Option<usize>) {
        let _ = self.viewer.send(Command::MaskOverlay(index));
    }

    /// Undoes (or redoes) a photo's last committed change; returns its adjustments and masks now.
    pub fn undo(&self, index: usize, redo: bool) -> Option<(Edit, Masks)> {
        let (edit, masks, strokes) = self.ratings.undo(index, redo)?;
        if self.view.lock().unwrap().index == Some(index) {
            let _ = self.viewer.send(Command::SetEdit(edit));
            let _ = self.viewer.send(Command::SetMasks(Box::new(masks)));
            let _ = self.viewer.send(Command::SetStrokes(strokes));
        }
        self.edit_committed(index);
        Some((edit, masks))
    }

    /// Queues a full-size JPEG export of a photo with its adjustments as they are now. Returns
    /// the job's id, which the progress and completion events carry (`viewer::EVENT_EXPORT`).
    pub fn export(&self, index: usize, path: PathBuf, request: Request) -> Result<u32, String> {
        let photos = self.jobs.photos();
        let photo = photos.get(index).ok_or(format!("there's no photo {index}"))?;
        let raw = photo.raw.clone().ok_or(format!("{} has no raw file to export", photo.name))?;
        let edit = self.ratings.edit(index).unwrap_or_default();
        let masks = self.ratings.masks(index).unwrap_or_default();
        let strokes = self.ratings.strokes(index).unwrap_or_default();
        let id = self.next_export.fetch_add(1, Ordering::Relaxed);
        let _ = self.viewer.send(Command::Export(ExportJob { id, index, raw, path, edit, masks: Box::new(masks), strokes, request }));
        Ok(id)
    }

    /// The temperature (K) and tint that make the spot at (`x`, `y`) on the view neutral (the
    /// view's physical pixels). Waits briefly for the GPU thread, which knows the view; the
    /// reading itself is a few hundred CPU samples.
    pub fn pick_white_balance(&self, x: f32, y: f32) -> Result<(f32, f32), String> {
        let (reply, result) = channel();
        let _ = self.viewer.send(Command::Pick { x, y, reply });
        result.recv_timeout(std::time::Duration::from_secs(3)).map_err(|_| "The picker timed out.".to_string())?
    }

    /// What lens data the photo's raw carries, once it is decoded: whether there is distortion
    /// to correct, and whether there is chromatic aberration to correct.
    pub fn lens_info(&self, index: usize) -> Option<(bool, bool)> {
        let raw = self.jobs.raw(index)?;
        Some(raw.lens.map_or((false, false), |l| (l.has_distortion(), l.has_ca())))
    }

    /// The photo's white balance as shot, as temperature (K) and tint, once its raw is decoded.
    pub fn as_shot_white_balance(&self, index: usize) -> Option<(f32, f32)> {
        let raw = self.jobs.raw(index)?;
        Some(develop::multipliers_to_temperature(&raw.xyz_to_cam, raw.wb))
    }

    /// The camera profiles available for photo `index`: `(id, name)` pairs starting with
    /// `(0, "Velochrome")`. Reads the camera make and model from the decoded raw if ready, or from
    /// the raw file's EXIF header.
    pub fn profiles(&self, index: usize) -> Vec<(u32, String)> {
        if let Some(raw) = self.jobs.raw(index) {
            return crate::profile::profiles_for_camera(&raw.make, &raw.model);
        }
        let photos = self.jobs.photos();
        if let Some(raw_path) = photos.get(index).and_then(|p| p.raw.as_ref())
            && let Some((make, model)) = crate::exif::camera_make_model(raw_path)
        {
            return crate::profile::profiles_for_camera(&make, &model);
        }
        vec![(0, "Velochrome".into())]
    }

    /// Creates or resizes the swap chain (physical pixels; `scale` is the view's DPI scale) and
    /// returns its IDXGISwapChain pointer, owned by the engine. Waits for the GPU thread.
    pub fn resize_view(&self, width: u32, height: u32, scale: f32) -> Result<usize, String> {
        {
            let mut v = self.view.lock().unwrap();
            v.view_size = (width, height);
            self.refocus(&v);
        }
        let (reply, result) = channel();
        let _ = self.viewer.send(Command::Resize { width, height, scale, reply });
        result.recv().map_err(|_| "the GPU thread stopped".to_string())?
    }

    /// Draws into an offscreen target instead of a swap chain (for the benchmark).
    pub fn use_offscreen_view(&self, width: u32, height: u32) {
        {
            let mut v = self.view.lock().unwrap();
            v.view_size = (width, height);
            self.refocus(&v);
        }
        let _ = self.viewer.send(Command::Offscreen { width, height });
    }

    /// Allocates GPU memory for photos of this size ahead of time (e.g. the sizes from the last
    /// session), so the first such photo doesn't wait for it.
    pub fn prepare(&self, width: u32, height: u32) {
        let _ = self.viewer.send(Command::Prepare(width, height));
    }

    /// What the offscreen view last drew, as RGBA (see `use_offscreen_view`).
    pub fn snapshot(&self) -> Option<(u32, u32, Vec<u8>)> {
        let (reply, result) = channel();
        let _ = self.viewer.send(Command::Snapshot(reply));
        result.recv().ok().flatten()
    }

    /// Every photo's stars, label and pick flag, in folder order.
    pub fn ratings(&self) -> &Ratings {
        &self.ratings
    }

    /// The order the user moves through the photos (sorted and filtered), so the right
    /// neighbours are decoded ahead.
    pub fn set_order(&self, order: Vec<usize>) {
        self.jobs.set_order(order);
    }

    /// Photos whose thumbnails are on screen, most important first.
    pub fn set_visible(&self, visible: Vec<usize>) {
        self.jobs.set_visible(visible);
    }

    pub fn jobs(&self) -> &Arc<Jobs> {
        &self.jobs
    }

    fn refocus(&self, v: &ViewState) {
        let Some(index) = v.index else { return };
        self.jobs.set_focus(Focus {
            index,
            direction: v.direction,
            loupe_edge: v.view_size.0.max(v.view_size.1),
            need_raw: v.edit_mode || v.crop_mode || v.actual_size,
        });
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.viewer.send(Command::Shutdown);
        if let Some(thread) = self.gpu_thread.take() {
            let _ = thread.join();
        }
        jobs::shutdown(&self.jobs);
        self.ratings.finish();
    }
}

/// Reports a card import to the app as an `EVENT_IMPORT` event: `photo` files done, `width`
/// files in all, `height` files that failed, `layer` the state (0 working, 1 done, 2 stopped),
/// `progress` the share of bytes, `gpu_ms` and `cpu_ms` the bytes done and in all, `since_show_ms`
/// the time taken. `text` is the file being copied, or at the end the summary (`Summary::text`).
fn emit_import(events: &Mutex<EventSink>, report: import::Report) {
    let (mut event, text) = match report {
        import::Report::Progress(p) => (
            FrameEvent {
                photo: p.files_done as i32,
                width: p.files_total as u32,
                height: p.failed as u32,
                progress: if p.bytes_total == 0 { 1.0 } else { (p.bytes_done as f64 / p.bytes_total as f64) as f32 },
                since_show_ms: p.elapsed.as_secs_f64() * 1000.0,
                gpu_ms: p.bytes_done as f64,
                cpu_ms: p.bytes_total as f64,
                ..Default::default()
            },
            p.current,
        ),
        import::Report::Finished(s) => (
            FrameEvent {
                layer: if s.cancelled { 2 } else { 1 },
                height: s.failures.len() as u32,
                progress: 1.0,
                since_show_ms: s.elapsed.as_secs_f64() * 1000.0,
                gpu_ms: s.bytes as f64,
                cpu_ms: s.bytes as f64,
                ..Default::default()
            },
            s.text(),
        ),
    };
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    event.kind = viewer::EVENT_IMPORT;
    event.text = wide.as_ptr() as usize;
    (events.lock().unwrap_or_else(|p| p.into_inner()))(&event);
}
