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
}

const PREVIEW_KINDS: [Kind; 3] = [Kind::Fast, Kind::Loupe, Kind::Full];

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
    workers: Mutex<Vec<JoinHandle<()>>>,
    worker_count: usize,
}

struct State {
    photos: Arc<Vec<Photo>>,
    /// Bumped when the folder changes, so results for the old one are dropped.
    generation: u64,
    focus: Focus,
    /// Wanted jobs, most wanted first, with their priority (0 is highest).
    wanted: Vec<(usize, Kind, u8)>,
    wanted_set: HashSet<(usize, Kind)>,
    in_flight: HashSet<(usize, Kind)>,
    raws_in_flight: usize,
    failed: HashSet<(usize, Kind)>,
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

impl Jobs {
    pub fn new(notify: Notify) -> Arc<Jobs> {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        // Leave one core for the UI and GPU threads.
        let worker_count = (cores - 1).max(2);
        let memory = sys::physical_memory_bytes();
        let jobs = Arc::new(Jobs {
            state: Mutex::new(State {
                photos: Arc::new(Vec::new()),
                generation: 0,
                focus: Focus { index: 0, direction: 1, loupe_edge: 2560, need_raw: false },
                wanted: Vec::new(),
                wanted_set: HashSet::new(),
                in_flight: HashSet::new(),
                raws_in_flight: 0,
                failed: HashSet::new(),
                previews: HashMap::new(),
                raws: HashMap::new(),
                preview_bytes: 0,
                raw_bytes: 0,
                // About 10% of memory for raws (48 MB each at 24 MP), 5% for previews.
                raw_budget: (memory / 10).max(512 << 20),
                preview_budget: (memory / 20).max(256 << 20),
                clock: 0,
                shutdown: false,
            }),
            wake: Condvar::new(),
            notify,
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
        s.photos = photos;
        s.generation += 1;
        s.previews.clear();
        s.raws.clear();
        s.failed.clear();
        s.preview_bytes = 0;
        s.raw_bytes = 0;
        s.focus.index = 0;
        s.plan();
        drop(s);
        self.wake.notify_all();
    }

    pub fn photos(&self) -> Arc<Vec<Photo>> {
        self.state.lock().unwrap().photos.clone()
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
            let (index, kind, priority, photo, generation, loupe_edge) = {
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
                }
                if s.generation == generation {
                    match result {
                        Ok(Done::Raw(frame)) => {
                            s.insert_raw(index, frame);
                            satisfied.push(Kind::Raw);
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
}

fn satisfies(p: &Preview, kind: Kind, loupe_edge: u32) -> bool {
    let decoded = p.pixels.width.max(p.pixels.height);
    let full_size = decoded >= p.source_width.max(p.source_height);
    match kind {
        Kind::Fast => true,
        Kind::Loupe => decoded >= loupe_edge || (p.is_largest && full_size),
        Kind::Full => p.is_largest && full_size,
        Kind::Raw => false,
    }
}

impl State {
    /// Rebuilds the wanted list from the focus.
    fn plan(&mut self) {
        let n = self.photos.len();
        let mut wanted: Vec<(usize, Kind, u8)> = Vec::new();
        let f = self.focus;
        if n > 0 {
            let at = |offset: i32| -> Option<usize> {
                let i = f.index as i64 + (offset * f.direction) as i64;
                (0..n as i64).contains(&i).then_some(i as usize)
            };
            let raw_priority = if f.need_raw { 1 } else { 4 };
            let mut add = |offset: i32, kind: Kind, priority: u8| {
                if let Some(i) = at(offset) {
                    wanted.push((i, kind, priority));
                }
            };
            add(0, Kind::Fast, 0);
            add(0, Kind::Loupe, 0);
            add(0, Kind::Full, 1);
            add(0, Kind::Raw, raw_priority);
            add(1, Kind::Fast, 2);
            add(1, Kind::Loupe, 2);
            for d in 2..=4 {
                add(d, Kind::Fast, 3);
                add(d, Kind::Loupe, 3);
            }
            add(-1, Kind::Fast, 3);
            add(-1, Kind::Loupe, 3);
            add(1, Kind::Full, 4);
            add(1, Kind::Raw, 4);
            add(2, Kind::Raw, 5);
            add(-1, Kind::Raw, 5);
            add(-1, Kind::Full, 5);
            for d in 3..=4 {
                add(d, Kind::Raw, 6);
            }
            for d in 5..=12 {
                add(d, Kind::Fast, 7);
            }
        }
        wanted.retain(|&(i, kind, _)| kind != Kind::Raw || self.photos[i].raw.is_some());
        wanted.sort_by_key(|&(_, _, priority)| priority); // stable: keeps the order above within a priority
        self.wanted_set = wanted.iter().map(|&(i, k, _)| (i, k)).collect();
        self.wanted = wanted;
    }

    /// The most wanted job that isn't done, running or failed, marked as running. Raws leave
    /// two workers free, so a preview never waits behind a slow raw decode.
    fn next_job(&mut self, workers: usize) -> Option<(usize, Kind, u8, Photo, u64, u32)> {
        let raw_limit = workers.saturating_sub(2).max(1);
        let job = self.wanted.iter().copied().find(|&(i, kind, _)| {
            !self.in_flight.contains(&(i, kind))
                && !self.failed.contains(&(i, kind))
                && match kind {
                    Kind::Raw => !self.raws.contains_key(&i) && self.raws_in_flight < raw_limit,
                    _ => !self.previews.contains_key(&(i, kind)),
                }
        })?;
        let (index, kind, priority) = job;
        self.in_flight.insert((index, kind));
        if kind == Kind::Raw {
            self.raws_in_flight += 1;
        }
        Some((index, kind, priority, self.photos[index].clone(), self.generation, self.focus.loupe_edge))
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
