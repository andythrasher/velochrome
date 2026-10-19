//! Background decoding. Worker threads take the most wanted job that isn't done yet; what's
//! wanted is recomputed from where the user is in the folder every time they move, so the
//! photo on screen always comes first and the photos they're heading towards come next.
//!
//! A decode can't be stopped once started, but nothing is thrown away: a finished one goes
//! into the cache even if the user has moved on, and the cache keeps what's near them.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::decode::{self, RawFrame};
use crate::developed::{self, Cache, Key};
use crate::library::Photo;
use crate::preview::{self, Preview, Want};
use crate::sys;

/// What can be decoded for a photo, from cheapest to most expensive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// The quickest preview, to show something at once.
    Fast,
    /// A preview that covers the view.
    Loupe,
    /// The biggest preview, at full size, for checking focus at 100%.
    Full,
    /// The raw sensor data.
    Raw,
    /// A small preview for the filmstrip and grid.
    Thumb,
    /// Our own render of an edited photo, read from the cache (developed.rs): already cropped.
    Developed,
}

/// Thumbnails' longer side, in pixels: sharp in a grid cell of about 170 DIPs at 150% scaling.
pub const THUMB_EDGE: u32 = 256;

const PREVIEW_KINDS: [Kind; 3] = [Kind::Fast, Kind::Loupe, Kind::Full];

/// How many full-size previews stay cached: the photo in view and the one before it.
const FULL_PREVIEWS_KEPT: usize = 2;

/// Where the user is, which decides what's wanted.
#[derive(Debug, Clone, Copy)]
pub struct Focus {
    pub index: usize,
    /// +1 when moving forward through the folder, -1 when moving back.
    pub direction: i32,
    /// The view's longer side in pixels: what a Loupe preview must cover.
    pub loupe_edge: u32,
    /// Raws are needed now (editing, or 100% with no full-size JPEG), not just eventually.
    pub need_raw: bool,
}

/// A job finished. Sent from a worker thread.
pub type Notify = Box<dyn Fn(usize, Kind) + Send + Sync>;

pub struct Jobs {
    state: Mutex<State>,
    wake: Condvar,
    notify: Notify,
    cache: Arc<Cache>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    worker_count: usize,
}

struct State {
    photos: Arc<Vec<Photo>>,
    /// Bumped when the folder changes, so results for the old one are dropped.
    generation: u64,
    focus: Focus,
    /// The order the user moves through the photos in (sorted and filtered by the app), which
    /// decides which photos are "next". Photos left out of it aren't prefetched.
    order: Vec<usize>,
    /// Each photo's place in `order`.
    position: Vec<Option<usize>>,
    /// Photos whose thumbnails are on screen (filmstrip or grid), most important first.
    visible: Vec<usize>,
    thumbs: HashMap<usize, Entry<Thumbnail>>,
    thumb_bytes: usize,
    thumb_budget: usize,
    /// Wanted jobs, most wanted first, with their priority (0 is highest).
    wanted: Vec<(usize, Kind, u8)>,
    wanted_set: HashSet<(usize, Kind)>,
    in_flight: HashSet<(usize, Kind)>,
    raws_in_flight: usize,
    /// Raw decodes running that are only for the backlog of previews to develop.
    backlog_in_flight: usize,
    failed: HashSet<(usize, Kind)>,
    /// Each photo's developed preview: its current key, and whether the cache has it.
    dev: Vec<Dev>,
    /// The photos with an edit (a few, usually): where the backlog looks.
    edited: HashSet<usize>,
    /// Photos whose thumbnail must be made again (their edit changed) whether or not it is on screen.
    refresh: HashSet<usize>,
    previews: HashMap<(usize, Kind), Entry<Preview>>,
    raws: HashMap<usize, Entry<RawFrame>>,
    preview_bytes: usize,
    raw_bytes: usize,
    preview_budget: usize,
    raw_budget: usize,
    clock: u64,
    shutdown: bool,
}

struct Entry<T> {
    value: Arc<T>,
    bytes: usize,
    last_used: u64,
}

/// Jobs at or above this priority run at full speed; the rest are background work, run with
/// Windows' EcoQoS so they go to efficient cores and save battery.
const FOREGROUND: u8 = 1;

/// The backlog of edited photos to develop: raws decoded for it run after everything else.
const BACKLOG: u8 = 9;
/// How many raws are decoded ahead for the backlog.
const BACKLOG_RAWS: usize = 2;

impl Jobs {
    pub fn new(notify: Notify, cache: Arc<Cache>) -> Arc<Jobs> {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        // Leave one core for the UI and GPU threads.
        let worker_count = (cores - 1).max(2);
        let memory = sys::physical_memory_bytes();
        let jobs = Arc::new(Jobs {
            state: Mutex::new(State {
                photos: Arc::new(Vec::new()),
                generation: 0,
                focus: Focus { index: 0, direction: 1, loupe_edge: 2560, need_raw: false },
                order: Vec::new(),
                position: Vec::new(),
                visible: Vec::new(),
                thumbs: HashMap::new(),
                thumb_bytes: 0,
                // 175 KB each at 256 pixels: about 750 thumbnails. Remaking one takes about 1.5 ms
                // of a core, so scrolling back to evicted ones costs little.
                thumb_budget: 128 << 20,
                wanted: Vec::new(),
                wanted_set: HashSet::new(),
                in_flight: HashSet::new(),
                raws_in_flight: 0,
                backlog_in_flight: 0,
                failed: HashSet::new(),
                dev: Vec::new(),
                edited: HashSet::new(),
                refresh: HashSet::new(),
                previews: HashMap::new(),
                raws: HashMap::new(),
                preview_bytes: 0,
                raw_bytes: 0,
                // Enough for the photos around the one in view, not a whole folder: culling moves
                // through photos once. Raws are 48 MB at 24 MP, so 768 MB is 16 of them; on a
                // 16 GB laptop, whose GPU shares that memory, it's about 14.
                raw_budget: (memory / 24).clamp(384 << 20, 768 << 20),
                preview_budget: (memory / 40).clamp(256 << 20, 512 << 20),
                clock: 0,
                shutdown: false,
            }),
            wake: Condvar::new(),
            notify,
            cache,
            workers: Mutex::new(Vec::new()),
            worker_count,
        });
        let handles = (0..worker_count)
            .map(|i| {
                let jobs = jobs.clone();
                std::thread::Builder::new()
                    .name(format!("windedit decode {i}"))
                    .spawn(move || jobs.work())
                    .expect("couldn't start a decode thread")
            })
            .collect();
        *jobs.workers.lock().unwrap() = handles;
        jobs
    }

    pub fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Switches to another set of photos (a folder), dropping everything cached.
    pub fn set_photos(&self, photos: Arc<Vec<Photo>>) {
        let mut s = self.state.lock().unwrap();
        let n = photos.len();
        s.photos = photos;
        s.generation += 1;
        s.previews.clear();
        s.raws.clear();
        s.thumbs.clear();
        s.failed.clear();
        s.dev = (0..n).map(|_| Dev::default()).collect();
        s.edited.clear();
        s.refresh.clear();
        s.preview_bytes = 0;
        s.raw_bytes = 0;
        s.thumb_bytes = 0;
        s.visible.clear();
        s.set_order((0..n).collect());
        s.focus.index = 0;
        s.plan();
        drop(s);
        self.wake.notify_all();
    }

    pub fn photos(&self) -> Arc<Vec<Photo>> {
        self.state.lock().unwrap().photos.clone()
    }

    pub fn cache(&self) -> &Arc<Cache> {
        &self.cache
    }

    // ---- Developed previews (docs/edited-previews.md) ----

    /// Tells what each photo's edit fingerprints to (None: unedited), right after a folder is
    /// opened. Photos with a preview in the cache show it at once; those without are queued
    /// for the background to develop.
    pub fn set_developed_all(&self, fingerprints: Vec<Option<u128>>) {
        let (photos, generation) = {
            let s = self.state.lock().unwrap();
            (s.photos.clone(), s.generation)
        };
        let keyed = developed::keys_for(&photos, &fingerprints, &self.cache);
        let mut s = self.state.lock().unwrap();
        if s.generation != generation || s.dev.len() != keyed.len() {
            return;
        }
        for (i, k) in keyed.into_iter().enumerate() {
            s.dev[i] = Dev { id: k.id, key: k.key, cached: k.cached, rendering: false, failed: false };
            if s.dev[i].key.is_some() {
                s.edited.insert(i);
            }
            if s.dev[i].cached {
                s.thumbs.remove(&i);
                s.refresh.insert(i);
            }
        }
        s.plan();
        drop(s);
        self.wake.notify_all();
    }

    /// A photo's edit changed (committed, undone, reset): `fingerprint` is None if it has none now.
    /// Its old developed preview is out of date from here on; the thumbnail stays as it is until a
    /// new one comes, unless the new state is already in the cache, or is no edit at all.
    pub fn set_developed(&self, index: usize, fingerprint: Option<u128>) -> bool {
        let (id, raw, generation) = {
            let s = self.state.lock().unwrap();
            let Some(d) = s.dev.get(index) else { return false };
            (d.id, s.photos[index].raw.clone(), s.generation)
        };
        let id = id.or_else(|| raw.as_deref().and_then(developed::photo_id));
        let key = id.zip(fingerprint).map(|(id, f)| developed::key(id, f));
        let cached = key.as_deref().is_some_and(|k| self.cache.has(k));
        let mut s = self.state.lock().unwrap();
        if s.generation != generation {
            return false;
        }
        s.dev[index].id = id;
        if s.dev[index].key == key {
            return false;
        }
        s.dev[index] = Dev { id, key: key.clone(), cached, rendering: false, failed: false };
        if key.is_some() {
            s.edited.insert(index);
        } else {
            s.edited.remove(&index);
        }
        if let Some(old) = s.previews.remove(&(index, Kind::Developed)) {
            s.preview_bytes -= old.bytes;
        }
        s.failed.remove(&(index, Kind::Developed));
        if key.is_none() || cached {
            if let Some(old) = s.thumbs.remove(&index) {
                s.thumb_bytes -= old.bytes;
            }
            s.refresh.insert(index);
        }
        s.plan();
        drop(s);
        self.wake.notify_all();
        true
    }

    /// The photo's developed preview is in the cache and on its way to memory: the view waits for it
    /// rather than show the camera's JPEG, which doesn't have the edit, for a moment first.
    pub fn developed_loading(&self, index: usize) -> bool {
        let s = self.state.lock().unwrap();
        s.ready_key(index).is_some() && !s.previews.contains_key(&(index, Kind::Developed)) && !s.failed.contains(&(index, Kind::Developed))
    }

    /// The key of the photo's current edit, if it has one.
    pub fn developed_key(&self, index: usize) -> Option<Key> {
        self.state.lock().unwrap().dev.get(index)?.key.clone()
    }

    /// The raw's identity for keys (see `developed::photo_id`), once it's been worked out.
    pub fn developed_id(&self, index: usize) -> Option<u64> {
        self.state.lock().unwrap().dev.get(index)?.id
    }

    /// The photo's developed preview was just rendered, for the photo in view: it can be drawn
    /// before it is encoded and written. Ignored if the edit has moved on since.
    pub fn developed_preview(&self, index: usize, key: &str, preview: Arc<Preview>) {
        let mut s = self.state.lock().unwrap();
        if s.dev.get(index).is_none_or(|d| d.key.as_deref() != Some(key)) {
            return;
        }
        s.insert_preview(index, Kind::Developed, preview);
    }

    /// A thumbnail made from a render of the photo's current edit, ahead of the preview itself
    /// (the filmstrip follows an edit being made). Ignored if the edit has moved on since.
    pub fn developed_thumb(&self, index: usize, key: &str, thumb: Arc<Thumbnail>) {
        {
            let mut s = self.state.lock().unwrap();
            if s.dev.get(index).is_none_or(|d| d.key.as_deref() != Some(key)) {
                return;
            }
            s.insert_thumb(index, thumb);
            s.refresh.remove(&index);
        }
        (self.notify)(index, Kind::Thumb);
    }

    /// The preview is in the cache: from now on it is the photo's, and its thumbnail the one
    /// shown. `ok` false says writing it failed (it isn't tried again for this edit).
    pub fn developed_written(&self, index: usize, key: &str, ok: bool, thumb: Option<Arc<Thumbnail>>) {
        {
            let mut s = self.state.lock().unwrap();
            let Some(d) = s.dev.get_mut(index).filter(|d| d.key.as_deref() == Some(key)) else { return };
            d.rendering = false;
            d.cached = ok;
            d.failed = !ok;
            if let Some(thumb) = thumb.filter(|_| ok) {
                s.insert_thumb(index, thumb);
                s.refresh.remove(&index);
            }
            s.plan();
        }
        self.wake.notify_all();
        (self.notify)(index, Kind::Thumb);
    }

    /// How many edited photos still have no developed preview.
    pub fn backlog_len(&self) -> usize {
        let s = self.state.lock().unwrap();
        s.edited.iter().filter(|&&i| s.is_stale(i)).count()
    }

    /// The most wanted edited photo that has no developed preview and whose raw is decoded: what
    /// the GPU thread should develop next when it has nothing else to do.
    pub fn next_backlog(&self) -> Option<(usize, Key, Arc<RawFrame>)> {
        let s = self.state.lock().unwrap();
        s.stale_order().into_iter().find_map(|i| {
            let raw = s.raws.get(&i)?.value.clone();
            Some((i, s.dev[i].key.clone()?, raw))
        })
    }

    /// The GPU thread took the photo up: it isn't the backlog's any more. Its raw is dropped if
    /// nothing else wants it.
    pub fn developed_begun(&self, index: usize, key: &str) -> bool {
        let mut s = self.state.lock().unwrap();
        let Some(d) = s.dev.get_mut(index).filter(|d| d.key.as_deref() == Some(key)) else { return false };
        d.rendering = true;
        true
    }

    /// Drops the raw of a photo that was decoded only for the backlog.
    pub fn release_backlog_raw(&self, index: usize) {
        let mut s = self.state.lock().unwrap();
        s.plan();
        if !s.wanted_set.contains(&(index, Kind::Raw))
            && let Some(old) = s.raws.remove(&index)
        {
            s.raw_bytes -= old.bytes;
        }
    }

    /// The order the user moves through the photos (e.g. sorted and filtered), for deciding
    /// which photos are next. Photos left out aren't prefetched.
    pub fn set_order(&self, order: Vec<usize>) {
        let mut s = self.state.lock().unwrap();
        s.set_order(order);
        s.plan();
        drop(s);
        self.wake.notify_all();
    }

    /// Photos whose thumbnails are on screen, most important first.
    pub fn set_visible(&self, visible: Vec<usize>) {
        let mut s = self.state.lock().unwrap();
        let n = s.photos.len();
        s.visible = visible.into_iter().filter(|&i| i < n).collect();
        s.plan();
        drop(s);
        self.wake.notify_all();
    }

    pub fn thumbnail(&self, index: usize) -> Option<Arc<Thumbnail>> {
        let mut s = self.state.lock().unwrap();
        s.clock += 1;
        let now = s.clock;
        s.thumbs.get_mut(&index).map(|e| {
            e.last_used = now;
            e.value.clone()
        })
    }

    pub fn set_focus(&self, focus: Focus) {
        let mut s = self.state.lock().unwrap();
        s.focus = focus;
        s.plan();
        drop(s);
        self.wake.notify_all();
    }

    pub fn preview(&self, index: usize, kind: Kind) -> Option<Arc<Preview>> {
        let mut s = self.state.lock().unwrap();
        s.clock += 1;
        let now = s.clock;
        s.previews.get_mut(&(index, kind)).map(|e| {
            e.last_used = now;
            e.value.clone()
        })
    }

    pub fn raw(&self, index: usize) -> Option<Arc<RawFrame>> {
        let mut s = self.state.lock().unwrap();
        s.clock += 1;
        let now = s.clock;
        s.raws.get_mut(&index).map(|e| {
            e.last_used = now;
            e.value.clone()
        })
    }

    /// The job failed (e.g. a photo with no raw, or a damaged file).
    pub fn failed(&self, index: usize, kind: Kind) -> bool {
        self.state.lock().unwrap().failed.contains(&(index, kind))
    }

    /// Waits until nothing wanted is left to do, or `timeout` passes (for tests and the
    /// benchmark). Returns whether it finished.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.state.lock().unwrap();
        loop {
            if s.in_flight.is_empty() && s.next_job(self.worker_count).is_none() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            s = self.wake.wait_timeout(s, left.min(Duration::from_millis(20))).unwrap().0;
        }
    }

    fn work(&self) {
        let mut eco = None;
        loop {
            let (index, kind, priority, photo, generation, loupe_edge, ready_key) = {
                let mut s = self.state.lock().unwrap();
                loop {
                    if s.shutdown {
                        return;
                    }
                    if let Some(job) = s.next_job(self.worker_count) {
                        break job;
                    }
                    s = self.wake.wait(s).unwrap();
                }
            };
            let background = priority > FOREGROUND;
            if eco != Some(background) {
                sys::set_background(background);
                eco = Some(background);
            }

            let result = match kind {
                Kind::Raw => match &photo.raw {
                    Some(path) => decode::decode_file(path).map(|f| Done::Raw(Arc::new(f))),
                    None => Err(decode::DecodeError("no raw file".into())),
                },
                Kind::Thumb => match ready_key.as_deref().and_then(|k| developed::load_thumb(&self.cache, k)) {
                    // An edited photo with a developed preview in the cache shows that.
                    Some(t) => Ok(Done::Thumb(Arc::new(t))),
                    None => preview::load(&photo, Want::Cover(THUMB_EDGE))
                        .map(|p| Done::Thumb(Arc::new(Thumbnail::from_preview(&p, THUMB_EDGE)))),
                },
                Kind::Developed => match &ready_key {
                    Some(k) => developed::load_preview(&self.cache, k).map(|p| Done::Preview(Arc::new(p))),
                    None => Err(decode::DecodeError("no developed preview".into())),
                },
                _ => {
                    let want = match kind {
                        Kind::Fast => Want::Fast,
                        Kind::Loupe => Want::Cover(loupe_edge),
                        _ => Want::Largest,
                    };
                    preview::load(&photo, want).map(|p| Done::Preview(Arc::new(p)))
                }
            };

            let mut satisfied = Vec::new();
            {
                let mut s = self.state.lock().unwrap();
                s.in_flight.remove(&(index, kind));
                if kind == Kind::Raw {
                    s.raws_in_flight -= 1;
                    if priority >= BACKLOG {
                        s.backlog_in_flight -= 1;
                    }
                }
                // The edit may have changed while this ran: what was made is then out of date.
                let outdated = matches!(kind, Kind::Thumb | Kind::Developed) && s.ready_key(index) != ready_key;
                if s.generation == generation && !outdated {
                    match result {
                        Ok(Done::Raw(frame)) => {
                            s.insert_raw(index, frame);
                            satisfied.push(Kind::Raw);
                        }
                        Ok(Done::Thumb(t)) => {
                            s.insert_thumb(index, t);
                            s.refresh.remove(&index);
                            satisfied.push(Kind::Thumb);
                        }
                        Ok(Done::Preview(p)) if kind == Kind::Developed => {
                            s.insert_preview(index, Kind::Developed, p);
                            satisfied.push(Kind::Developed);
                        }
                        Ok(Done::Preview(p)) => {
                            // A preview can stand in for other kinds too: a 3200-pixel embedded
                            // JPEG is both the quickest and one that covers a 2560 view.
                            // It only fills other kinds that are still empty: a full-size JPEG mustn't
                            // replace the right-sized loupe preview, which is cheaper to draw.
                            for k in PREVIEW_KINDS {
                                let empty = !s.previews.contains_key(&(index, k));
                                if k == kind || (empty && satisfies(&p, k, s.focus.loupe_edge)) {
                                    s.insert_preview(index, k, p.clone());
                                    satisfied.push(k);
                                }
                            }
                        }
                        Err(_) => {
                            s.failed.insert((index, kind));
                            // A grid cell waits for its thumbnail, so say it won't come.
                            if kind == Kind::Thumb {
                                satisfied.push(Kind::Thumb);
                            }
                            // A developed preview that can't be read is made again.
                            if kind == Kind::Developed {
                                s.dev[index].cached = false;
                                s.plan();
                                satisfied.push(Kind::Developed); // the view stops waiting for it
                            }
                        }
                    }
                }
            }
            self.wake.notify_all(); // a raw slot may have freed up, and wait_idle may be done
            for k in satisfied {
                (self.notify)(index, k);
            }
        }
    }
}

/// Stops the workers and waits for them. They share `Jobs`, so dropping it alone wouldn't
/// stop them: whoever owns the engine calls this.
pub fn shutdown(jobs: &Arc<Jobs>) {
    jobs.state.lock().unwrap().shutdown = true;
    jobs.wake.notify_all();
    let handles: Vec<_> = jobs.workers.lock().unwrap().drain(..).collect();
    for handle in handles {
        let _ = handle.join();
    }
}

enum Done {
    Preview(Arc<Preview>),
    Raw(Arc<RawFrame>),
    Thumb(Arc<Thumbnail>),
}

/// A small, upright preview for the filmstrip and grid: 8-bit BGRA with opaque alpha (what
/// WinUI's WriteableBitmap takes), rows packed.
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Thumbnail {
    /// Shrinks a preview to fit `edge` (averaging blocks of pixels) and turns it upright, so
    /// an app can show it as it is.
    pub fn from_preview(p: &Preview, edge: u32) -> Thumbnail {
        let (sw, sh) = (p.pixels.width as usize, p.pixels.height as usize);
        let scale = (edge as f32 / sw.max(sh) as f32).min(1.0);
        let (w, h) = (((sw as f32 * scale).round() as usize).max(1), ((sh as f32 * scale).round() as usize).max(1));
        let mut small = vec![0u8; w * h * 4];
        for y in 0..h {
            let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1));
            for x in 0..w {
                let (x0, x1) = (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1));
                let mut sum = [0u32; 3];
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let i = (sy * sw + sx) * 4;
                        for (s, &v) in sum.iter_mut().zip(&p.pixels.bgra[i..i + 3]) {
                            *s += v as u32;
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                let o = (y * w + x) * 4;
                for (d, s) in small[o..o + 3].iter_mut().zip(sum) {
                    *d = (s / n) as u8;
                }
                small[o + 3] = 255;
            }
        }
        rotate(small, w, h, p.orientation)
    }
}

fn rotate(px: Vec<u8>, w: usize, h: usize, orientation: crate::decode::Orientation) -> Thumbnail {
    use crate::decode::Orientation::*;
    if orientation == Normal {
        return Thumbnail { width: w as u32, height: h as u32, bgra: px };
    }
    let (ow, oh) = if orientation.swaps_axes() { (h, w) } else { (w, h) };
    let mut out = vec![0u8; px.len()];
    for y in 0..h {
        for x in 0..w {
            // Where source pixel (x, y) lands once rotated clockwise by the orientation.
            let (nx, ny) = match orientation {
                Rotate90 => (h - 1 - y, x),
                Rotate180 => (w - 1 - x, h - 1 - y),
                Rotate270 => (y, w - 1 - x),
                Normal => (x, y),
            };
            let (s, d) = ((y * w + x) * 4, (ny * ow + nx) * 4);
            out[d..d + 4].copy_from_slice(&px[s..s + 4]);
        }
    }
    Thumbnail { width: ow as u32, height: oh as u32, bgra: out }
}

fn satisfies(p: &Preview, kind: Kind, loupe_edge: u32) -> bool {
    let decoded = p.pixels.width.max(p.pixels.height);
    let full_size = decoded >= p.source_width.max(p.source_height);
    match kind {
        Kind::Fast => true,
        Kind::Loupe => decoded >= loupe_edge || (p.is_largest && full_size),
        Kind::Full => p.is_largest && full_size,
        Kind::Raw | Kind::Thumb | Kind::Developed => false,
    }
}

impl State {
    /// Rebuilds the wanted list from the focus.
    fn plan(&mut self) {
        let n = self.photos.len();
        let mut wanted: Vec<(usize, Kind, u8)> = Vec::new();
        let f = self.focus;
        if n > 0 {
            // Neighbours in the order the user moves through (filters and sorting included). A
            // photo outside that order (just filtered out) still gets its own previews.
            let here = self.position.get(f.index).copied().flatten();
            let order = &self.order;
            let at = |offset: i32| -> Option<usize> {
                match here {
                    Some(pos) => {
                        let p = pos as i64 + (offset * f.direction) as i64;
                        (0..order.len() as i64).contains(&p).then(|| order[p as usize])
                    }
                    None => (offset == 0 && f.index < n).then_some(f.index),
                }
            };
            let raw_priority = if f.need_raw { 1 } else { 4 };
            let dev = &self.dev;
            let mut add = |offset: i32, kind: Kind, priority: u8| {
                if let Some(i) = at(offset) {
                    // Only an edited photo whose developed preview is in the cache has one to read.
                    if kind != Kind::Developed || dev.get(i).is_some_and(|d| d.cached && d.key.is_some()) {
                        wanted.push((i, kind, priority));
                    }
                }
            };
            add(0, Kind::Developed, 0);
            add(0, Kind::Fast, 0);
            add(0, Kind::Loupe, 0);
            add(0, Kind::Full, 1);
            add(0, Kind::Raw, raw_priority);
            add(1, Kind::Developed, 2);
            add(-1, Kind::Developed, 3);
            add(1, Kind::Fast, 2);
            add(1, Kind::Loupe, 2);
            for d in 2..=4 {
                add(d, Kind::Fast, 3);
                add(d, Kind::Loupe, 3);
            }
            add(-1, Kind::Fast, 3);
            add(-1, Kind::Loupe, 3);
            add(1, Kind::Raw, 4);
            add(2, Kind::Raw, 5);
            add(-1, Kind::Raw, 5);
            for d in 3..=4 {
                add(d, Kind::Raw, 6);
            }
            for d in 5..=12 {
                add(d, Kind::Fast, 7);
            }
            // Thumbnails on screen come right after the photo in view: they're cheap, and an
            // empty grid is the first thing anyone would notice.
            for (rank, &i) in self.visible.iter().enumerate() {
                wanted.push((i, Kind::Thumb, if rank < 64 { 1 } else { 3 }));
            }
            // Thumbnails of photos whose edit has just changed are made again, seen or not.
            wanted.extend(self.refresh.iter().map(|&i| (i, Kind::Thumb, 1)));
            // Last of all: edited photos with no developed preview yet need their raws decoded, so
            // the GPU thread can develop them when it has nothing else to do.
            wanted.extend(self.stale_order().into_iter().take(BACKLOG_RAWS).map(|i| (i, Kind::Raw, BACKLOG)));
        }
        wanted.retain(|&(i, kind, _)| kind != Kind::Raw || self.photos[i].raw.is_some());
        wanted.sort_by_key(|&(_, _, priority)| priority); // stable: keeps the order above within a priority
        self.wanted_set = wanted.iter().map(|&(i, k, _)| (i, k)).collect();
        self.wanted = wanted;
    }

    fn set_order(&mut self, order: Vec<usize>) {
        let n = self.photos.len();
        self.position = vec![None; n];
        let order: Vec<usize> = order.into_iter().filter(|&i| i < n).collect();
        for (pos, &i) in order.iter().enumerate() {
            self.position[i] = Some(pos);
        }
        self.order = order;
    }

    fn insert_thumb(&mut self, index: usize, value: Arc<Thumbnail>) {
        self.clock += 1;
        let bytes = value.bgra.len();
        if let Some(old) = self.thumbs.insert(index, Entry { value, bytes, last_used: self.clock }) {
            self.thumb_bytes -= old.bytes;
        }
        self.thumb_bytes += bytes;
        while self.thumb_bytes > self.thumb_budget {
            let Some(key) = oldest(&self.thumbs, |&i| !self.wanted_set.contains(&(i, Kind::Thumb))) else { break };
            self.thumb_bytes -= self.thumbs.remove(&key).unwrap().bytes;
        }
    }

    /// The most wanted job that isn't done, running or failed, marked as running. Raws leave
    /// two workers free, so a preview never waits behind a slow raw decode.
    fn next_job(&mut self, workers: usize) -> Option<(usize, Kind, u8, Photo, u64, u32, Option<Key>)> {
        let raw_limit = workers.saturating_sub(2).max(1);
        let job = self.wanted.iter().copied().find(|&(i, kind, priority)| {
            !self.in_flight.contains(&(i, kind))
                && !self.failed.contains(&(i, kind))
                && match kind {
                    // The backlog's raws go one at a time, behind everything else.
                    Kind::Raw => !self.raws.contains_key(&i) && self.raws_in_flight < raw_limit && (priority < BACKLOG || self.backlog_in_flight < 1),
                    Kind::Thumb => !self.thumbs.contains_key(&i),
                    Kind::Developed => !self.previews.contains_key(&(i, kind)) && self.ready_key(i).is_some(),
                    _ => !self.previews.contains_key(&(i, kind)),
                }
        })?;
        let (index, kind, priority) = job;
        self.in_flight.insert((index, kind));
        if kind == Kind::Raw {
            self.raws_in_flight += 1;
            if priority >= BACKLOG {
                self.backlog_in_flight += 1;
            }
        }
        let ready_key = self.ready_key(index);
        Some((index, kind, priority, self.photos[index].clone(), self.generation, self.focus.loupe_edge, ready_key))
    }

    /// The key of the photo's developed preview, if it has one in the cache.
    fn ready_key(&self, index: usize) -> Option<Key> {
        self.dev.get(index).filter(|d| d.cached).and_then(|d| d.key.clone())
    }

    /// Edited, no developed preview, none being made, and a raw to make it from.
    fn is_stale(&self, index: usize) -> bool {
        let d = &self.dev[index];
        d.key.is_some() && !d.cached && !d.rendering && !d.failed && self.photos[index].raw.is_some() && !self.failed.contains(&(index, Kind::Raw))
    }

    /// The photos to develop in the background, most wanted first: the one in view and its
    /// neighbours, then those whose thumbnails are on screen, then the rest nearest first.
    fn stale_order(&self) -> Vec<usize> {
        let mut stale: Vec<usize> = self.edited.iter().copied().filter(|&i| self.is_stale(i)).collect();
        if stale.len() > 1 {
            let here = self.position.get(self.focus.index).copied().flatten();
            stale.sort_by_cached_key(|&i| {
                let distance = match (here, self.position[i]) {
                    (Some(a), Some(b)) => a.abs_diff(b),
                    _ => usize::MAX / 2,
                };
                let seen = self.visible.iter().position(|&v| v == i);
                let (class, rank) = match (distance <= 1, seen) {
                    (true, _) => (0, distance),
                    (false, Some(rank)) => (1, rank),
                    (false, None) => (2, distance),
                };
                (class, rank, i)
            });
        }
        stale
    }

    fn insert_preview(&mut self, index: usize, kind: Kind, value: Arc<Preview>) {
        self.clock += 1;
        let bytes = value.pixels.bgra.len();
        if let Some(old) = self.previews.insert((index, kind), Entry { value, bytes, last_used: self.clock }) {
            self.preview_bytes -= old.bytes;
        }
        self.preview_bytes += bytes;
        // Several kinds can share one preview; counting it each time overstates the total,
        // which only makes eviction a little early.
        // Full-size previews are big (96 MB at 24 MP) and only wanted for checking focus on the
        // photo in view, so just the last two are kept, whatever the budget.
        if kind == Kind::Full {
            let focus = self.focus.index;
            loop {
                let fulls: Vec<_> = self.previews.keys().filter(|k| k.1 == Kind::Full).copied().collect();
                if fulls.len() <= FULL_PREVIEWS_KEPT {
                    break;
                }
                let Some(key) = oldest(&self.previews, |k| k.1 == Kind::Full && k.0 != focus) else { break };
                self.preview_bytes -= self.previews.remove(&key).unwrap().bytes;
            }
        }
        while self.preview_bytes > self.preview_budget {
            let Some(key) = oldest(&self.previews, |k| !self.wanted_set.contains(k)) else { break };
            self.preview_bytes -= self.previews.remove(&key).unwrap().bytes;
        }
    }

    fn insert_raw(&mut self, index: usize, value: Arc<RawFrame>) {
        self.clock += 1;
        let bytes = value.data.len() * 2;
        if let Some(old) = self.raws.insert(index, Entry { value, bytes, last_used: self.clock }) {
            self.raw_bytes -= old.bytes;
        }
        self.raw_bytes += bytes;
        while self.raw_bytes > self.raw_budget {
            let Some(key) = oldest(&self.raws, |&i| !self.wanted_set.contains(&(i, Kind::Raw))) else { break };
            self.raw_bytes -= self.raws.remove(&key).unwrap().bytes;
        }
    }
}

/// The least recently used entry that `evictable` allows.
fn oldest<K: Copy + Eq + std::hash::Hash, T>(map: &HashMap<K, Entry<T>>, evictable: impl Fn(&K) -> bool) -> Option<K> {
    map.iter().filter(|(k, _)| evictable(k)).min_by_key(|(_, e)| e.last_used).map(|(k, _)| *k)
}

/// A photo's developed preview, as far as the engine knows.
#[derive(Default)]
struct Dev {
    /// What identifies its raw (see `developed::photo_id`), once worked out.
    id: Option<u64>,
    /// The cache key of its current edit; None when it has no edit (or no raw).
    key: Option<Key>,
    /// The cache has the preview for that key.
    cached: bool,
    /// The GPU thread is developing it (or its encoder is writing it).
    rendering: bool,
    /// Making it failed: not tried again until the edit changes.
    failed: bool,
}
