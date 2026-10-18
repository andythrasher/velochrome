//! Copying a camera card into the library, filed the way the photographer files by hand:
//! `<root>\2026\2026-06-06\P1020943.RW2`.
//!
//! `plan` looks at the card without touching it: which photos are on it (every folder under
//! DCIM), when each was taken, where each would go and whether it's already there. `run` does the
//! copying, on its own thread when started with `Import::start`, writing every file to a `.tmp`
//! name and renaming it once its size (and, if asked, its contents) check out, so a pulled card
//! or a full disk never leaves half a photo under a real name. Nothing on the card is ever
//! changed or deleted, and nothing already in the library is ever replaced.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::exif::{self, Stamp};
use crate::library::{self, Photo};

/// How much of a file is read at a time. Big reads keep a card's controller streaming.
const BUFFER: usize = 4 << 20;

/// How often progress is reported while bytes move.
const REPORT_EVERY: Duration = Duration::from_millis(100);

/// One file of a photo on the card and where it goes.
#[derive(Debug, Clone)]
pub struct FilePlan {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub size: u64,
    /// A sidecar (`.xmp`) rather than a picture.
    pub sidecar: bool,
    /// Not at the destination yet (a picture), or no sidecar there yet.
    pub needed: bool,
}

/// One photo: its raw and JPEG (as the library pairs them) and any sidecar, all going to the
/// same folder under the same name.
#[derive(Debug, Clone)]
pub struct Item {
    /// The photo as the card has it (what the app shows while browsing the card).
    pub photo: Photo,
    pub taken: Stamp,
    pub dest_dir: PathBuf,
    /// Where its sidecar is or will be: stars and flags given on the card are saved here.
    pub sidecar: PathBuf,
    pub files: Vec<FilePlan>,
    /// Some of its pictures are not in the library yet.
    pub is_new: bool,
}

/// What importing a card would do right now.
#[derive(Debug, Clone)]
pub struct Plan {
    pub card: PathBuf,
    pub dest_root: PathBuf,
    /// In the order the photos were taken.
    pub items: Vec<Item>,
}

impl Plan {
    pub fn new_count(&self) -> usize {
        self.items.iter().filter(|i| i.is_new).count()
    }

    /// Bytes still to copy (to one destination).
    pub fn bytes_to_copy(&self) -> u64 {
        self.items.iter().flat_map(|i| &i.files).filter(|f| f.needed).map(|f| f.size).sum()
    }

    pub fn photos(&self) -> Vec<Photo> {
        self.items.iter().map(|i| i.photo.clone()).collect()
    }

    pub fn sidecars(&self) -> Vec<PathBuf> {
        self.items.iter().map(|i| i.sidecar.clone()).collect()
    }
}

/// The folders of the card's DCIM folder (`100OMSYS`, `101_PANA`, …), by name.
pub fn card_folders(card: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(card.join("DCIM"))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort_by_key(|d| d.file_name().map(|n| n.to_ascii_lowercase()));
    Ok(dirs)
}

/// When a photo was taken: the EXIF date of its raw, else of its JPEG, else when its file was
/// last changed (the camera sets that too), else a date that is obviously wrong.
fn taken(photo: &Photo) -> Stamp {
    let files = || photo.raw.iter().chain(photo.jpeg.iter());
    files()
        .find_map(|f| exif::capture_time(f))
        .or_else(|| files().find_map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok().and_then(crate::sys::local_stamp)))
        .unwrap_or(Stamp { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 })
}

/// What a destination file is, compared with the card's.
enum There {
    Missing,
    /// A file of the same name and size: this one was imported already.
    Same,
    /// Something else is using the name.
    Different,
}

fn look(dest: &Path, size: u64) -> There {
    match std::fs::metadata(dest) {
        Ok(m) if m.len() == size => There::Same,
        Ok(_) => There::Different,
        Err(_) => There::Missing,
    }
}

/// Where `source` goes in `dest_dir` when the photo is filed as `stem`.
fn dest_file(dest_dir: &Path, stem: &str, source: &Path) -> PathBuf {
    match source.extension() {
        Some(ext) => dest_dir.join(format!("{stem}.{}", ext.to_string_lossy())),
        None => dest_dir.join(stem),
    }
}

/// The photos on the card and where each goes under `dest_root`. Reads each photo's date from
/// the card (in parallel), but nothing else of it.
pub fn plan(card: &Path, dest_root: &Path) -> std::io::Result<Plan> {
    let photos = library::scan_folders(&card_folders(card)?)?;
    let mut dated: Vec<(Photo, Stamp)> = photos.into_par_iter().map(|p| { let stamp = taken(&p); (p, stamp) }).collect();
    // In the order they were shot; photos taken in the same second keep the card's order.
    dated.sort_by_key(|(_, stamp)| *stamp);

    // Names given out in this plan, so two photos never share one before either is copied. The
    // file system ignores case, so this does too.
    let mut claimed: HashSet<String> = HashSet::new();
    let mut items = Vec::with_capacity(dated.len());
    for (photo, taken) in dated {
        let dest_dir = dest_root.join(format!("{:04}", taken.year)).join(taken.day_name());
        let mut sources: Vec<(PathBuf, bool)> = photo.raw.iter().chain(photo.jpeg.iter()).map(|p| (p.clone(), false)).collect();
        if let Some(first) = sources.first() {
            let side = first.0.with_extension("xmp");
            if side.is_file() {
                sources.push((side, true));
            }
        }
        let sizes: Vec<u64> = sources.iter().map(|(p, _)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).collect();

        // The first name (the camera's, then with -1, -2, … added) that no other photo uses and
        // none of whose pictures is a different file already.
        let (stem, is_new) = (0..)
            .find_map(|n| {
                let stem = if n == 0 { photo.name.clone() } else { format!("{}-{n}", photo.name) };
                let key = dest_dir.join(&stem).to_string_lossy().to_lowercase();
                if claimed.contains(&key) {
                    return None;
                }
                let states: Vec<There> = sources
                    .iter()
                    .zip(&sizes)
                    .filter(|((_, sidecar), _)| !sidecar)
                    .map(|((p, _), &size)| look(&dest_file(&dest_dir, &stem, p), size))
                    .collect();
                if states.iter().any(|s| matches!(s, There::Different)) {
                    return None;
                }
                claimed.insert(key);
                Some((stem, states.iter().any(|s| matches!(s, There::Missing))))
            })
            .expect("some name is free");
        let files = sources
            .into_iter()
            .zip(sizes)
            .map(|((source, sidecar), size)| {
                let dest = dest_file(&dest_dir, &stem, &source);
                // A sidecar is the user's work: it is copied only where there is none.
                let needed = if sidecar { !dest.exists() } else { !matches!(look(&dest, size), There::Same) };
                FilePlan { source, dest, size, sidecar, needed }
            })
            .collect();
        items.push(Item { photo, taken, sidecar: dest_dir.join(format!("{stem}.xmp")), dest_dir, files, is_new });
    }
    Ok(Plan { card: card.to_path_buf(), dest_root: dest_root.to_path_buf(), items })
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// A second library to write every file to as well, laid out the same way.
    pub backup_root: Option<PathBuf>,
    /// Read each copy back and compare it with what was read from the card.
    pub verify: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Working,
    Done,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub failed: usize,
    pub elapsed: Duration,
    /// The file being copied.
    pub current: String,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Summary {
    /// Photos whose new pictures all arrived.
    pub photos: usize,
    /// The folders those photos went to, in the order they were reached.
    pub folders: Vec<PathBuf>,
    pub bytes: u64,
    pub failures: Vec<Failure>,
    pub cancelled: bool,
    pub elapsed: Duration,
}

impl Summary {
    /// The text the app shows: a headline, then `!name: reason` for each failure and `>folder`
    /// for each folder that gained photos.
    pub fn text(&self) -> String {
        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        let mut text = if self.cancelled {
            format!("Stopped after {}", plural(self.photos, "photo"))
        } else {
            format!("Imported {} to {}", plural(self.photos, "photo"), plural(self.folders.len(), "folder"))
        };
        if !self.failures.is_empty() {
            text += &format!(", {} couldn't be copied", self.failures.len());
        }
        for f in &self.failures {
            text += &format!("\n!{}: {}", f.name, f.reason);
        }
        for dir in &self.folders {
            text += &format!("\n>{}", dir.display());
        }
        text
    }
}

pub enum Report {
    Progress(Progress),
    Finished(Summary),
}

/// A fast checksum for comparing a copy with the card's file: not cryptographic, just enough to
/// notice a byte that didn't survive the trip. Depends on how the file is cut into chunks, so
/// both sides read in the same `BUFFER` pieces.
fn mix(mut h: u64, data: &[u8]) -> u64 {
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31);
    }
    for &b in rest {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01B3);
    }
    h
}

fn temp_name(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Reads `file` in `BUFFER` pieces; false means end of file.
fn read_piece(file: &mut impl Read, buf: &mut Vec<u8>) -> std::io::Result<bool> {
    buf.clear();
    file.take(BUFFER as u64).read_to_end(buf)?;
    Ok(!buf.is_empty())
}

/// Copies `source` to every path in `dests`, reading it once: each destination has a thread
/// writing from the same buffers. Each ends as a finished file or as nothing; one destination
/// failing doesn't stop the others. `moved` is told about each piece read.
fn copy_file(source: &Path, dests: &[PathBuf], verify: bool, cancel: &AtomicBool, moved: &mut dyn FnMut(u64)) -> Vec<Result<(), String>> {
    let fail_all = |message: String| vec![Err(message); dests.len()];
    for dest in dests {
        if let Some(dir) = dest.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return fail_all(format!("couldn't make {}: {e}", dir.display()));
        }
    }
    let mut input = match std::fs::File::open(source) {
        Ok(f) => f,
        Err(e) => return fail_all(format!("couldn't read it from the card: {e}")),
    };
    let temps: Vec<PathBuf> = dests.iter().map(|d| temp_name(d)).collect();

    let mut read_error = None;
    let (mut total, mut hash) = (0u64, 0u64);
    let mut written: Vec<Result<u64, String>> = std::thread::scope(|scope| {
        let mut senders = Vec::new();
        let mut writers = Vec::new();
        for temp in &temps {
            let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(2);
            senders.push(Some(tx));
            writers.push(scope.spawn(move || -> Result<u64, String> {
                crate::sys::set_background(true);
                let mut file = std::fs::File::create(temp).map_err(|e| format!("couldn't create {}: {e}", temp.display()))?;
                let mut bytes = 0u64;
                for piece in rx {
                    file.write_all(&piece).map_err(|e| format!("couldn't write {}: {e}", temp.display()))?;
                    bytes += piece.len() as u64;
                }
                file.flush().map_err(|e| e.to_string())?;
                Ok(bytes)
            }));
        }
        let mut buf = Vec::with_capacity(BUFFER);
        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            match read_piece(&mut input, &mut buf) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    read_error = Some(format!("couldn't read it from the card: {e}"));
                    break;
                }
            }
            if verify {
                hash = mix(hash, &buf);
            }
            total += buf.len() as u64;
            let piece = Arc::new(std::mem::replace(&mut buf, Vec::with_capacity(BUFFER)));
            // A writer that failed has hung up; the others carry on.
            for sender in &mut senders {
                if sender.as_ref().is_some_and(|tx| tx.send(piece.clone()).is_err()) {
                    *sender = None;
                }
            }
            moved(piece.len() as u64);
            if senders.iter().all(|s| s.is_none()) {
                break;
            }
        }
        drop(senders);
        writers.into_iter().map(|w| w.join().unwrap_or_else(|_| Err("the writer stopped".to_string()))).collect()
    });

    let mut results = Vec::new();
    for (i, temp) in temps.iter().enumerate() {
        let outcome = (|| -> Result<(), String> {
            if let Some(e) = &read_error {
                return Err(e.clone());
            }
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".to_string());
            }
            let bytes = std::mem::replace(&mut written[i], Ok(0))?;
            let on_disk = std::fs::metadata(temp).map_err(|e| e.to_string())?.len();
            if bytes != total || on_disk != total {
                return Err(format!("the copy is {on_disk} bytes, the original {total}"));
            }
            if verify && read_back(temp)? != hash {
                return Err("the copy doesn't match the original".to_string());
            }
            // Nothing is replaced, even by a file that appeared since the plan was made.
            if dests[i].exists() {
                return Err("a file of that name appeared meanwhile".to_string());
            }
            std::fs::rename(temp, &dests[i]).map_err(|e| format!("couldn't finish {}: {e}", dests[i].display()))
        })();
        if outcome.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        results.push(outcome);
    }
    results
}

/// The checksum of a file as it is on disk now.
fn read_back(path: &Path) -> Result<u64, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let (mut buf, mut hash) = (Vec::with_capacity(BUFFER), 0u64);
    while read_piece(&mut file, &mut buf).map_err(|e| e.to_string())? {
        hash = mix(hash, &buf);
    }
    Ok(hash)
}

/// Copies what the plan says is missing. Blocks until it's done; progress and the summary go to
/// `report`. Stops between pieces once `cancel` is set, leaving the finished photos and no
/// partial files.
pub fn run(plan: &Plan, options: &Options, cancel: &AtomicBool, report: &mut dyn FnMut(Report)) -> Summary {
    let started = Instant::now();
    let wanted: Vec<&FilePlan> = plan.items.iter().flat_map(|i| &i.files).filter(|f| f.needed).collect();
    let mut progress = Progress {
        files_done: 0,
        files_total: wanted.len(),
        bytes_done: 0,
        bytes_total: wanted.iter().map(|f| f.size).sum(),
        failed: 0,
        elapsed: Duration::ZERO,
        current: String::new(),
    };
    let mut summary = Summary { photos: 0, folders: Vec::new(), bytes: 0, failures: Vec::new(), cancelled: false, elapsed: Duration::ZERO };
    let mut last_report = Instant::now() - REPORT_EVERY;
    let mut say = |progress: &mut Progress, force: bool, report: &mut dyn FnMut(Report)| {
        if force || last_report.elapsed() >= REPORT_EVERY {
            last_report = Instant::now();
            progress.elapsed = started.elapsed();
            report(Report::Progress(progress.clone()));
        }
    };

    'photos: for item in plan.items.iter().filter(|i| i.files.iter().any(|f| f.needed)) {
        let mut copied = 0;
        let mut failed_here = false;
        for file in item.files.iter().filter(|f| f.needed) {
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break 'photos;
            }
            let name = file.dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
            progress.current = name.clone();
            let before = progress.bytes_done;

            // Where this file goes: the library, and the backup. Neither replaces anything.
            let mut dests = Vec::new();
            let mut problems = Vec::new();
            let backup = options.backup_root.as_ref().and_then(|root| file.dest.strip_prefix(&plan.dest_root).ok().map(|rel| root.join(rel)));
            for (dest, is_backup) in std::iter::once((&file.dest, false)).chain(backup.as_ref().map(|b| (b, true))) {
                match std::fs::metadata(dest) {
                    Err(_) => dests.push(dest.clone()),
                    // Already there: fine for a sidecar (never replaced) or a same-size picture.
                    Ok(m) if file.sidecar || m.len() == file.size => {}
                    Ok(_) => problems.push(format!("{}a different file of that name is there", if is_backup { "backup: " } else { "" })),
                }
            }
            let mut results = if dests.is_empty() {
                Vec::new()
            } else {
                copy_file(&file.source, &dests, options.verify, cancel, &mut |n| {
                    progress.bytes_done += n;
                    say(&mut progress, false, report);
                })
            };
            if cancel.load(Ordering::Relaxed) && results.iter().any(|r| matches!(r, Err(e) if e == "cancelled")) {
                summary.cancelled = true;
                break 'photos;
            }
            for (dest, result) in dests.iter().zip(results.drain(..)) {
                if let Err(e) = result {
                    problems.push(format!("{}{e}", if Some(dest) == backup.as_ref() { "backup: " } else { "" }));
                } else if *dest == file.dest {
                    copied += 1;
                    summary.bytes += file.size;
                }
            }
            progress.bytes_done = before + file.size;
            progress.files_done += 1;
            if !problems.is_empty() {
                failed_here = true;
                progress.failed += 1;
                for reason in problems {
                    summary.failures.push(Failure { name: name.clone(), reason });
                }
            }
            say(&mut progress, true, report);
        }
        if copied > 0 && !failed_here {
            summary.photos += 1;
        }
        if copied > 0 && !summary.folders.contains(&item.dest_dir) {
            summary.folders.push(item.dest_dir.clone());
        }
    }
    summary.elapsed = started.elapsed();
    summary
}

/// A copy running on its own thread. Dropping it cancels the copy and waits for the thread.
pub struct Import {
    cancel: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    finished: Arc<AtomicBool>,
}

impl Import {
    /// Starts copying. `report` is called on the copy thread: progress as it goes, then the
    /// summary once.
    pub fn start(plan: Arc<Plan>, options: Options, report: Box<dyn Fn(Report) + Send>) -> Import {
        let cancel = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (flag, done) = (cancel.clone(), finished.clone());
        let thread = std::thread::Builder::new()
            .name("windedit import".into())
            .spawn(move || {
                // Below the decoders, so browsing the card stays smooth while it copies.
                crate::sys::set_background(true);
                let summary = run(&plan, &options, &flag, &mut |r| report(r));
                done.store(true, Ordering::Release);
                report(Report::Finished(summary));
            })
            .expect("couldn't start the import thread");
        Import { cancel, thread: Mutex::new(Some(thread)), finished }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        !self.finished.load(Ordering::Acquire)
    }
}

impl Drop for Import {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// A scratch folder holding a fake card (`card\DCIM\100OMSYS`, …), a library to import into
    /// and a backup. Nothing here is the user's Pictures folder.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("windedit-import-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("card").join("DCIM").join("100OMSYS")).unwrap();
            Scratch { root }
        }

        fn card(&self) -> PathBuf {
            self.root.join("card")
        }

        fn lib(&self) -> PathBuf {
            self.root.join("library")
        }

        fn backup(&self) -> PathBuf {
            self.root.join("backup")
        }

        /// A file on the card whose size is 100 + `extra` bytes more than the EXIF it carries.
        fn shoot(&self, folder: &str, name: &str, date: &str, extra: usize) {
            let dir = self.card().join("DCIM").join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            let mut bytes = if name.to_lowercase().ends_with(".jpg") { exif::test_jpeg(date) } else { exif::test_tiff(date) };
            bytes.extend(vec![0xAB; extra]);
            std::fs::write(dir.join(name), bytes).unwrap();
        }

        fn write(&self, folder: &str, name: &str, bytes: &[u8]) {
            let dir = self.card().join("DCIM").join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), bytes).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn import(plan: &Plan, options: &Options) -> Summary {
        run(plan, options, &AtomicBool::new(false), &mut |_| {})
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn files_photos_by_the_day_they_were_taken_with_their_jpeg_and_sidecar() {
        let s = Scratch::new("days");
        s.shoot("100OMSYS", "P1.ORF", "2026:06:07 09:00:00", 10);
        s.shoot("100OMSYS", "P1.JPG", "2026:06:07 09:00:00", 5);
        s.write("100OMSYS", "P1.xmp", b"<x/>");
        s.shoot("100OMSYS", "P2.ORF", "2026:06:06 21:30:00", 10);
        s.shoot("101OMSYS", "P3.jpg", "2026:06:06 08:00:00", 3);
        s.write("100OMSYS", "readme.txt", b"not a photo");

        let plan = plan(&s.card(), &s.lib()).unwrap();
        assert_eq!(plan.items.len(), 3, "a raw, its JPEG and sidecar are one photo");
        let order: Vec<_> = plan.items.iter().map(|i| i.photo.name.as_str()).collect();
        assert_eq!(order, ["P3", "P2", "P1"], "in the order they were taken");
        assert_eq!(plan.new_count(), 3);

        let summary = import(&plan, &Options::default());
        assert_eq!((summary.photos, summary.folders.len(), summary.failures.len()), (3, 2, 0));
        assert_eq!(names(&s.lib().join("2026").join("2026-06-07")), ["P1.JPG", "P1.ORF", "P1.xmp"]);
        assert_eq!(names(&s.lib().join("2026").join("2026-06-06")), ["P2.ORF", "P3.jpg"]);
        assert_eq!(summary.text().lines().next().unwrap(), "Imported 3 photos to 2 folders");

        // The card is as it was, and no temporary files were left anywhere.
        assert_eq!(names(&s.card().join("DCIM").join("100OMSYS")), ["P1.JPG", "P1.ORF", "P1.xmp", "P2.ORF", "readme.txt"]);
        assert!(names(&s.lib().join("2026").join("2026-06-07")).iter().all(|n| !n.ends_with(".tmp")));
        // The sidecar to rate into is the one next to where the photos went.
        assert_eq!(plan.items[2].sidecar, s.lib().join("2026").join("2026-06-07").join("P1.xmp"));
    }

    #[test]
    fn a_photo_with_no_date_is_filed_by_its_modified_time() {
        let s = Scratch::new("nodate");
        s.write("100OMSYS", "P1.ORF", b"no exif in here");
        // Noon UTC is the same calendar day in every time zone but the two furthest out.
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_741_089_600); // 2025-03-04 12:00 UTC
        let file = std::fs::OpenOptions::new().write(true).open(s.card().join("DCIM/100OMSYS/P1.ORF")).unwrap();
        file.set_modified(modified).unwrap();
        drop(file);

        let plan = plan(&s.card(), &s.lib()).unwrap();
        assert_eq!(plan.items[0].dest_dir, s.lib().join("2025").join("2025-03-04"));
        assert_eq!(import(&plan, &Options::default()).photos, 1);
        assert!(s.lib().join("2025/2025-03-04/P1.ORF").exists());
    }

    #[test]
    fn photos_already_imported_are_skipped_and_a_partial_one_is_finished() {
        let s = Scratch::new("again");
        s.shoot("100OMSYS", "P1.ORF", "2026:06:06 10:00:00", 50);
        s.shoot("100OMSYS", "P1.JPG", "2026:06:06 10:00:00", 20);
        s.shoot("100OMSYS", "P2.ORF", "2026:06:06 10:05:00", 50);
        let first = plan(&s.card(), &s.lib()).unwrap();
        assert_eq!(import(&first, &Options::default()).photos, 2);

        let second = plan(&s.card(), &s.lib()).unwrap();
        assert_eq!((second.items.len(), second.new_count()), (2, 0), "all there already");
        let summary = import(&second, &Options::default());
        assert_eq!((summary.photos, summary.bytes), (0, 0));

        // An interrupted import left P1's JPEG behind: only that is copied, under the same name.
        let day = s.lib().join("2026/2026-06-06");
        std::fs::remove_file(day.join("P1.JPG")).unwrap();
        let third = plan(&s.card(), &s.lib()).unwrap();
        assert_eq!(third.new_count(), 1);
        let needed: Vec<_> = third.items.iter().flat_map(|i| &i.files).filter(|f| f.needed).map(|f| f.dest.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(needed, ["P1.JPG"]);
        assert_eq!(import(&third, &Options::default()).photos, 1);
        assert_eq!(names(&day), ["P1.JPG", "P1.ORF", "P2.ORF"]);
    }

    #[test]
    fn a_name_clash_with_a_different_file_gets_a_number_and_the_original_is_kept() {
        let s = Scratch::new("clash");
        s.shoot("100OMSYS", "P1.ORF", "2026:06:06 10:00:00", 50);
        s.shoot("100OMSYS", "P1.JPG", "2026:06:06 10:00:00", 20);
        // Another card's P1.ORF is in the library already, and so is a P1-1.
        let day = s.lib().join("2026/2026-06-06");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("P1.ORF"), b"somebody else's photo").unwrap();
        std::fs::write(day.join("P1-1.ORF"), b"and another").unwrap();
        // A second card folder numbers from the same name, shot a minute later.
        s.shoot("101OMSYS", "P1.ORF", "2026:06:06 10:01:00", 50);

        let plan = plan(&s.card(), &s.lib()).unwrap();
        let dests: Vec<_> = plan.items.iter().map(|i| i.files[0].dest.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(dests, ["P1-2.ORF", "P1-3.ORF"], "each photo gets a name nothing else has");
        assert_eq!(plan.items[0].files.iter().map(|f| f.dest.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>(), ["P1-2.ORF", "P1-2.JPG"]);
        let summary = import(&plan, &Options::default());
        assert_eq!((summary.photos, summary.failures.len()), (2, 0));
        assert_eq!(std::fs::read(day.join("P1.ORF")).unwrap(), b"somebody else's photo");
        assert_eq!(std::fs::read(day.join("P1-1.ORF")).unwrap(), b"and another");

        // Run again: the numbered copies are recognised as the card's, not copied afresh.
        let again = super::plan(&s.card(), &s.lib()).unwrap();
        assert_eq!(again.new_count(), 0);
        assert_eq!(again.items[0].files[0].dest, day.join("P1-2.ORF"));
    }

    #[test]
    fn a_failed_copy_is_reported_and_the_rest_carry_on() {
        let s = Scratch::new("failed");
        for (i, name) in ["P1", "P2", "P3"].iter().enumerate() {
            s.shoot("100OMSYS", &format!("{name}.ORF"), &format!("2026:06:06 10:0{i}:00"), 30);
        }
        // A folder where P2's temporary file must go makes that one copy impossible.
        let day = s.lib().join("2026/2026-06-06");
        std::fs::create_dir_all(day.join("P2.ORF.tmp")).unwrap();

        let plan = plan(&s.card(), &s.lib()).unwrap();
        let mut reports = 0;
        let summary = run(&plan, &Options::default(), &AtomicBool::new(false), &mut |r| {
            if let Report::Progress(p) = r {
                reports += 1;
                assert!(p.files_done <= p.files_total);
            }
        });
        assert!(reports >= 3);
        assert_eq!(summary.photos, 2);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.failures[0].name, "P2.ORF");
        assert!(summary.text().contains("!P2.ORF:"));
        assert!(day.join("P1.ORF").exists() && day.join("P3.ORF").exists());
        assert!(!day.join("P2.ORF").exists(), "no half-copied photo under its real name");
    }

    #[test]
    fn a_backup_gets_every_file_with_the_same_layout() {
        let s = Scratch::new("backup");
        s.shoot("100OMSYS", "P1.ORF", "2026:06:06 10:00:00", 3 * BUFFER / 2); // spans several pieces
        s.shoot("100OMSYS", "P1.JPG", "2026:06:06 10:00:00", 700);
        let plan = plan(&s.card(), &s.lib()).unwrap();
        let options = Options { backup_root: Some(s.backup()), verify: true };
        let summary = import(&plan, &options);
        assert_eq!((summary.photos, summary.failures.len()), (1, 0));
        for name in ["P1.ORF", "P1.JPG"] {
            let original = std::fs::read(s.card().join("DCIM/100OMSYS").join(name)).unwrap();
            assert_eq!(std::fs::read(s.lib().join("2026/2026-06-06").join(name)).unwrap(), original);
            assert_eq!(std::fs::read(s.backup().join("2026/2026-06-06").join(name)).unwrap(), original);
        }
        assert_eq!(names(&s.backup().join("2026/2026-06-06")).len(), 2, "no temporary files");
    }

    #[test]
    fn a_backup_that_cannot_be_written_is_reported_but_the_library_copy_stands() {
        let s = Scratch::new("badbackup");
        s.shoot("100OMSYS", "P1.ORF", "2026:06:06 10:00:00", 30);
        std::fs::create_dir_all(s.backup().join("2026/2026-06-06/P1.ORF.tmp")).unwrap();
        let plan = plan(&s.card(), &s.lib()).unwrap();
        let summary = import(&plan, &Options { backup_root: Some(s.backup()), verify: false });
        assert!(s.lib().join("2026/2026-06-06/P1.ORF").exists());
        assert_eq!(summary.failures.len(), 1);
        assert!(summary.failures[0].reason.starts_with("backup:"));
    }

    #[test]
    fn sidecars_are_never_overwritten() {
        let s = Scratch::new("sidecars");
        s.shoot("100OMSYS", "P1.ORF", "2026:06:06 10:00:00", 30);
        s.write("100OMSYS", "P1.xmp", b"the card's");
        s.shoot("100OMSYS", "P2.ORF", "2026:06:06 10:01:00", 30);
        s.write("100OMSYS", "P2.xmp", b"the card's");
        // Stars given while browsing the card were saved to P1's sidecar in the library.
        let day = s.lib().join("2026/2026-06-06");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("P1.xmp"), b"mine, with 4 stars").unwrap();

        let plan = plan(&s.card(), &s.lib()).unwrap();
        assert_eq!(plan.items[0].sidecar, day.join("P1.xmp"));
        let summary = import(&plan, &Options { backup_root: Some(s.backup()), verify: false });
        assert_eq!(summary.failures.len(), 0);
        assert_eq!(std::fs::read(day.join("P1.xmp")).unwrap(), b"mine, with 4 stars");
        assert_eq!(std::fs::read(day.join("P2.xmp")).unwrap(), b"the card's", "a sidecar with no counterpart is copied");

        // Even one that shows up during the copy: the plan said it was needed, the file system says no.
        std::fs::remove_file(day.join("P2.xmp")).unwrap();
        let stale = super::plan(&s.card(), &s.lib()).unwrap();
        std::fs::write(day.join("P2.xmp"), b"made meanwhile").unwrap();
        import(&stale, &Options::default());
        assert_eq!(std::fs::read(day.join("P2.xmp")).unwrap(), b"made meanwhile");
    }

    #[test]
    fn cancelling_leaves_finished_photos_and_no_partial_files() {
        let s = Scratch::new("cancel");
        for i in 0..4 {
            s.shoot("100OMSYS", &format!("P{i}.ORF"), &format!("2026:06:06 10:0{i}:00"), 2 * BUFFER);
        }
        let plan = plan(&s.card(), &s.lib()).unwrap();
        let cancel = AtomicBool::new(false);
        let summary = run(&plan, &Options::default(), &cancel, &mut |r| {
            // Stop once the second photo's first piece has been read.
            if let Report::Progress(p) = r
                && p.bytes_done > 2 * (2 * BUFFER as u64 + 100)
            {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert!(summary.cancelled);
        assert!(summary.photos >= 1 && summary.photos < 4);
        let day = s.lib().join("2026/2026-06-06");
        assert!(names(&day).iter().all(|n| !n.ends_with(".tmp")), "{:?}", names(&day));
        assert_eq!(names(&day).len(), summary.photos);
    }

    #[test]
    fn a_checksum_notices_a_changed_byte_and_ignores_nothing_else() {
        let a = vec![7u8; 100_003];
        let mut b = a.clone();
        assert_eq!(mix(0, &a), mix(0, &b));
        b[99_999] ^= 1;
        assert_ne!(mix(0, &a), mix(0, &b));
        b[99_999] ^= 1;
        b.push(0);
        assert_ne!(mix(0, &a), mix(0, &b));
    }

    #[test]
    fn a_card_without_dcim_is_an_error() {
        let s = Scratch::new("nodcim");
        std::fs::remove_dir_all(s.card().join("DCIM")).unwrap();
        assert!(plan(&s.card(), &s.lib()).is_err());
    }
}
