//! The GPU thread. It owns the Direct3D device and the swap chain, and draws whatever is the
//! best thing available for the photo in view: one of the camera's JPEGs while culling, the
//! developed raw when editing or at 100% when no full-size JPEG exists. When something better
//! arrives from the decode workers it simply draws again.
//!
//! Nothing here waits for decoding, and the app's UI thread never waits for the GPU (only
//! resizing the view waits for a reply).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, TrySendError, channel, sync_channel};
use std::thread::JoinHandle;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::core::Interface;

use crate::decode::{self, RawFrame};
use crate::export::{self, Encoded, Request, Strip, TileRenderer};
use crate::geometry;
use crate::gpu::Gpu;
use crate::jobs::{Jobs, Kind};
use crate::masks::Masks;
use crate::pipeline::{Edit, Image, ImageMemory, Layer, MaskInput, PreviewTexture, Slot, Target, Zoom, render_masked, warm_up};
use crate::present::Presenter;
use crate::preview::Preview;

pub enum Command {
    /// Show this photo (index into the folder).
    Show(usize),
    Zoom(Zoom),
    /// The adjustments of the photo in view.
    SetEdit(Edit),
    /// The masks of the photo in view.
    SetMasks(Box<Masks>),
    /// The mask to show as a red overlay, if any (the Masks panel's selection).
    MaskOverlay(Option<usize>),
    /// Edit mode shows the developed raw; Cull mode shows camera JPEGs.
    EditMode(bool),
    /// Crop mode shows the whole frame at Fit, with the photo turned beneath it (and no zoom).
    CropMode(bool),
    /// In crop mode, the point of the frame (fractions) at the view's centre.
    CropView(f32, f32),
    /// The colour of the empty canvas around the photo.
    Canvas([f32; 3]),
    /// Create or resize the swap chain. Replies with the IDXGISwapChain pointer.
    Resize { width: u32, height: u32, scale: f32, reply: Sender<Result<usize, String>> },
    /// Draw into an offscreen target of this size instead of a swap chain (the benchmark).
    Offscreen { width: u32, height: u32 },
    /// A decode finished.
    Ready(usize, Kind),
    /// GPU memory for a photo size, allocated on the helper thread.
    Allocated(Slot),
    /// Allocate memory for this photo size ahead of time.
    Prepare(u32, u32),
    /// The folder changed: forget the current photo.
    Folder,
    /// Read back the view (offscreen, or the swap chain's last frame) as RGBA, for tests.
    Snapshot(Sender<Option<(u32, u32, Vec<u8>)>>),
    /// Pick a white balance from a click on the view (physical pixels): temperature and tint.
    Pick { x: f32, y: f32, reply: Sender<Result<(f32, f32), String>> },
    /// Export a photo as a full-size JPEG. Jobs run one after another.
    Export(ExportJob),
    /// A raw decoded for an export that had to wait for it (job id).
    ExportDecoded(u32, Result<Arc<RawFrame>, String>),
    /// An export's file is written, or failed (job id).
    ExportFinished(u32, Result<Encoded, String>),
    Shutdown,
}

/// A photo to export. The adjustments are those at the time of the request.
pub struct ExportJob {
    pub id: u32,
    pub index: usize,
    pub raw: PathBuf,
    /// Where to write; a taken name gets -2, -3, … (see `export::write_new_file`).
    pub path: PathBuf,
    pub edit: Edit,
    pub masks: Box<Masks>,
    pub request: Request,
}

/// What a drawn frame showed, sent to the app after every frame.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FrameEvent {
    /// `EVENT_FRAME`, `EVENT_THUMBNAIL` or `EVENT_EXPORT`.
    pub kind: u32,
    pub photo: i32,
    /// What was drawn: 1 quick preview, 2 loupe preview, 3 full-size preview, 4 the raw.
    pub layer: u32,
    /// The size in photo pixels of what the view shows: the crop, or in crop mode the whole frame
    /// (the raw's once it's decoded, before that its biggest JPEG's): what fractions in
    /// `we_set_zoom` are relative to.
    pub width: u32,
    pub height: u32,
    /// The raw's sensor size, once decoded (else 0): what `we_prepare` takes.
    pub raw_width: u32,
    pub raw_height: u32,
    /// Where that picture sits on the view, in the view's pixels: left, top, width, height. In
    /// crop mode, the frame; the crop box is a part of it.
    pub view_x: f32,
    pub view_y: f32,
    pub view_w: f32,
    pub view_h: f32,
    /// The frame's size in photo pixels, whatever is shown (`width` and `height` are of the crop).
    pub frame_width: u32,
    pub frame_height: u32,
    /// From the request to show this photo until this frame was handed to the screen.
    pub since_show_ms: f64,
    pub gpu_ms: f64,
    /// Wall clock of drawing and presenting this frame.
    pub cpu_ms: f64,
    /// Uploading and demosaicing the raw, when this frame did it (else 0).
    pub open_ms: f64,
    /// For `EVENT_EXPORT` (`photo` is the photo's index): the job's id, how far it is, and its
    /// state (`EXPORT_*`). `text` is a pointer (as an integer, so events can be sent between threads) to UTF-16, NUL-terminated, valid only during the callback:
    /// the file written for `EXPORT_DONE`, the problem for `EXPORT_FAILED`, else null. On done,
    /// `since_show_ms` is the time from starting the job to the file, `gpu_ms` developing,
    /// `cpu_ms` reading back, `open_ms` encoding (it overlaps the rest).
    pub export_id: u32,
    pub export_state: u32,
    pub progress: f32,
    pub text: usize,
    /// Where the raw's texture coordinates (0..1, in the sensor's own orientation: where masks live)
    /// are on the view: the coordinates at pixel position (0, 0), then the change per pixel to the
    /// right and per pixel down, as x, y pairs (`geometry::ViewMap`). All 0 until the raw is decoded.
    pub map: [f32; 6],
}

impl Default for FrameEvent {
    fn default() -> Self {
        FrameEvent {
            kind: 0,
            photo: 0,
            layer: 0,
            width: 0,
            height: 0,
            raw_width: 0,
            raw_height: 0,
            view_x: 0.0,
            view_y: 0.0,
            view_w: 0.0,
            view_h: 0.0,
            frame_width: 0,
            frame_height: 0,
            since_show_ms: 0.0,
            gpu_ms: 0.0,
            cpu_ms: 0.0,
            open_ms: 0.0,
            export_id: 0,
            export_state: 0,
            progress: 0.0,
            text: 0,
            map: [0.0; 6],
        }
    }
}

/// `FrameEvent::export_state`: rendering (with `progress` 0–1), saved, or failed.
pub const EXPORT_PROGRESS: u32 = 0;
pub const EXPORT_DONE: u32 = 1;
pub const EXPORT_FAILED: u32 = 2;

pub fn layer_code(kind: Kind) -> u32 {
    match kind {
        Kind::Fast => 1,
        Kind::Loupe => 2,
        Kind::Full => 3,
        Kind::Raw => 4,
        Kind::Thumb => 0,
    }
}

/// `FrameEvent::kind`: a frame was drawn.
pub const EVENT_FRAME: u32 = 1;
/// `FrameEvent::kind`: the photo's thumbnail is ready (only `photo` is set).
pub const EVENT_THUMBNAIL: u32 = 2;
/// `FrameEvent::kind`: an export made progress, finished or failed.
pub const EVENT_EXPORT: u32 = 3;
/// `FrameEvent::kind`: a card import made progress or ended (see `engine::emit_import` for the fields).
pub const EVENT_IMPORT: u32 = 4;

pub type EventSink = Box<dyn Fn(&FrameEvent) + Send>;

pub struct GpuInfo {
    pub adapter_name: String,
    pub uma: bool,
    pub warm_up_ms: f64,
}

/// Starts the GPU thread. Returns once the GPU is set up and warmed up.
pub fn spawn(
    jobs: Arc<Jobs>,
    events: EventSink,
    tx: Sender<Command>,
    rx: Receiver<Command>,
) -> Result<(JoinHandle<()>, GpuInfo), String> {
    let (ready_tx, ready_rx) = channel();
    let handle = std::thread::Builder::new()
        .name("windedit gpu".into())
        .spawn(move || {
            let mut gpu = match Gpu::new() {
                Ok(gpu) => gpu,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("no usable Direct3D 12 GPU: {e}")));
                    return;
                }
            };
            let warm_up_ms = match warm_up(&mut gpu) {
                Ok(ms) => ms,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("GPU warm-up failed: {e}")));
                    return;
                }
            };
            let allocator = spawn_allocator(&gpu, tx.clone());
            let _ = ready_tx.send(Ok(GpuInfo { adapter_name: gpu.adapter_name.clone(), uma: gpu.uma, warm_up_ms }));
            Viewer::new(gpu, jobs, events, allocator, tx).run(rx);
        })
        .map_err(|e| e.to_string())?;
    let info = ready_rx.recv().map_err(|_| "the GPU thread stopped".to_string())??;
    Ok((handle, info))
}

/// The device can be used from any thread (Direct3D 12 guarantees it).
struct SendDevice(ID3D12Device);
unsafe impl Send for SendDevice {}

/// A thread that allocates GPU memory for photo sizes on request, so the 50–170 ms it takes on
/// some laptops never holds up drawing.
fn spawn_allocator(gpu: &Gpu, viewer: Sender<Command>) -> Sender<(u32, u32)> {
    let (tx, rx) = channel::<(u32, u32)>();
    let device = SendDevice(gpu.device.clone());
    let uma = gpu.uma;
    std::thread::Builder::new()
        .name("windedit gpu memory".into())
        .spawn(move || {
            let device = device;
            crate::sys::set_background(true);
            for (width, height) in rx {
                if let Ok(slot) = Slot::allocate(&device.0, uma, width, height)
                    && viewer.send(Command::Allocated(slot)).is_err()
                {
                    return;
                }
            }
        })
        .expect("couldn't start the GPU memory thread");
    tx
}

struct Viewer {
    gpu: Gpu,
    jobs: Arc<Jobs>,
    events: EventSink,
    allocator: Sender<(u32, u32)>,
    allocating: HashSet<(u32, u32)>,
    /// To post commands to this thread from helper threads (decoding and encoding for exports).
    commands: Sender<Command>,
    /// Exports waiting their turn, with whether the folder changed since each was asked for.
    exports: VecDeque<(ExportJob, bool)>,
    export: Option<ActiveExport>,
    /// The export is waiting for a helper thread, so there is no point stepping it at once.
    export_waiting: bool,
    /// Exports whose rendering is done and whose file is being written: photo, start, timings.
    export_meta: HashMap<u32, (usize, Instant, f64, f64)>,
    memory: ImageMemory,
    presenter: Option<Presenter>,
    offscreen: Option<Target>,
    focus: Option<usize>,
    shown_at: Instant,
    zoom: Zoom,
    edit: Edit,
    masks: Masks,
    mask_overlay: Option<usize>,
    edit_mode: bool,
    crop_mode: bool,
    crop_center: (f32, f32),
    /// Previews on the GPU, one per tier (quick, loupe, full size). Separate, so moving between
    /// tiers (sharpening up, zooming in and out) never swaps one texture between two sizes,
    /// which would reallocate it each time (50–170 ms on some laptops). Each is reused for the
    /// next photo, as a camera's previews are all the same size.
    previews: [PreviewSlot; 3],
    /// The developed raw on the GPU, and which photo it is.
    image: Option<(usize, Image)>,
    dirty: bool,
    shutdown: bool,
}

impl Viewer {
    fn new(gpu: Gpu, jobs: Arc<Jobs>, events: EventSink, allocator: Sender<(u32, u32)>, commands: Sender<Command>) -> Viewer {
        Viewer {
            gpu,
            jobs,
            events,
            allocator,
            allocating: HashSet::new(),
            commands,
            exports: VecDeque::new(),
            export: None,
            export_waiting: false,
            export_meta: HashMap::new(),
            memory: ImageMemory::new(),
            presenter: None,
            offscreen: None,
            focus: None,
            shown_at: Instant::now(),
            zoom: Zoom::Fit,
            edit: Edit::default(),
            masks: Masks::default(),
            mask_overlay: None,
            edit_mode: false,
            crop_mode: false,
            crop_center: (0.5, 0.5),
            previews: Default::default(),
            image: None,
            dirty: false,
            shutdown: false,
        }
    }

    fn run(mut self, rx: Receiver<Command>) {
        while !self.shutdown {
            // Sleep until there's something to do, then take everything queued, so a burst of
            // slider moves or key repeats becomes one frame. If the photo's full-size preview
            // could be uploaded ahead, do it once things have been quiet for a moment: never
            // while the user is moving through photos.
            let first = if self.export.is_some() || !self.exports.is_empty() {
                // An export takes a tile's worth of work between the commands that arrive, so
                // the window keeps drawing; while it waits on a helper thread, it just polls.
                let next = if self.export_waiting { rx.recv_timeout(Duration::from_millis(5)) } else { match rx.try_recv() {
                    Ok(command) => Ok(command),
                    Err(TryRecvError::Empty) => Err(RecvTimeoutError::Timeout),
                    Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
                } };
                match next {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => {
                        self.export_step();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else if self.full_preview_to_upload().is_some() {
                match rx.recv_timeout(Duration::from_millis(150)) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => {
                        if let Err(e) = self.upload_full_preview() {
                            eprintln!("windedit: uploading a preview failed: {e}");
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                let Ok(command) = rx.recv() else { break };
                command
            };
            self.handle(first);
            while let Ok(next) = rx.try_recv() {
                self.handle(next);
            }
            if self.dirty && !self.shutdown {
                if let Some(p) = &self.presenter {
                    p.wait_for_frame(100);
                    // Whatever arrived while waiting goes into this frame too.
                    while let Ok(next) = rx.try_recv() {
                        self.handle(next);
                    }
                }
                self.dirty = false;
                if let Err(e) = self.draw() {
                    eprintln!("windedit: drawing failed: {e}");
                }
            }
        }
        let _ = self.gpu.wait_idle();
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Show(index) => {
                if self.focus != Some(index) {
                    self.focus = Some(index);
                    self.shown_at = Instant::now();
                }
                self.dirty = true;
            }
            Command::Zoom(zoom) => {
                self.zoom = zoom;
                self.dirty = true;
            }
            Command::SetEdit(edit) => {
                if edit != self.edit {
                    self.edit = edit;
                    self.dirty = true;
                }
            }
            Command::SetMasks(masks) => {
                if *masks != self.masks {
                    self.masks = *masks;
                    self.dirty = true;
                }
            }
            Command::MaskOverlay(index) => {
                if index != self.mask_overlay {
                    self.mask_overlay = index;
                    self.dirty = true;
                }
            }
            Command::EditMode(on) => {
                self.edit_mode = on;
                self.dirty = true;
            }
            Command::CropView(x, y) => {
                self.crop_center = (x, y);
                self.dirty = true;
            }
            Command::CropMode(on) => {
                self.crop_mode = on;
                self.crop_center = (0.5, 0.5);
                self.dirty = true;
            }
            Command::Canvas(rgb) => {
                crate::pipeline::set_canvas_color(rgb);
                self.dirty = true;
            }
            Command::Resize { width, height, scale, reply } => {
                let result = self.resize(width, height, scale);
                let _ = reply.send(result.map_err(|e| e.to_string()));
                self.dirty = true;
            }
            Command::Offscreen { width, height } => {
                self.offscreen = Target::new(&self.gpu, width, height).ok();
                self.dirty = true;
            }
            Command::Ready(index, Kind::Thumb) => {
                (self.events)(&FrameEvent { kind: EVENT_THUMBNAIL, photo: index as i32, ..Default::default() });
            }
            Command::Ready(index, kind) => {
                if kind == Kind::Raw
                    && let Some(frame) = self.jobs.raw(index)
                {
                    self.prepare(frame.width, frame.height);
                }
                if Some(index) == self.focus {
                    self.dirty = true;
                }
            }
            Command::Allocated(slot) => {
                self.allocating.remove(&(slot.width, slot.height));
                self.memory.insert(slot);
            }
            Command::Prepare(width, height) => self.prepare(width, height),
            Command::Export(job) => self.exports.push_back((job, false)),
            Command::ExportDecoded(id, frame) => self.export_decoded(id, frame),
            Command::ExportFinished(id, result) => self.export_finished(id, result),
            Command::Folder => {
                // Photo indexes mean something else now; queued exports carry on by file.
                self.exports.iter_mut().for_each(|(_, detached)| *detached = true);
                if let Some(a) = &mut self.export {
                    a.detached = true;
                }
                self.focus = None;
                self.image = None;
                for slot in &mut self.previews {
                    slot.of = None;
                }
            }
            Command::Snapshot(reply) => {
                let target = self.offscreen.as_ref().or(self.presenter.as_ref().map(|p| &p.target));
                let shot = match target {
                    Some(t) => crate::pipeline::read_target(&mut self.gpu, t).ok().map(|px| (t.width, t.height, px)),
                    None => None,
                };
                let _ = reply.send(shot);
            }
            Command::Pick { x, y, reply } => {
                let _ = reply.send(self.pick(x, y));
            }
            Command::Shutdown => self.shutdown = true,
        }
    }

    fn resize(&mut self, width: u32, height: u32, scale: f32) -> windows::core::Result<usize> {
        match &mut self.presenter {
            Some(p) => p.resize(&mut self.gpu, width, height, scale)?,
            None => self.presenter = Some(Presenter::new(&self.gpu, width, height, scale)?),
        }
        Ok(self.presenter.as_ref().unwrap().chain.as_raw() as usize)
    }

    /// Asks the helper thread for memory for this photo size, unless it's there or coming.
    fn prepare(&mut self, width: u32, height: u32) {
        if !self.memory.has(width, height) && self.allocating.insert((width, height)) {
            let _ = self.allocator.send((width, height));
        }
    }

    /// What to draw for the photo in view, best first. Cull mode sticks to the camera's JPEGs;
    /// the raw comes in for editing, and at 100% when there's no full-size JPEG to check focus.
    fn preferences(&self, index: usize) -> &'static [Kind] {
        if self.edit_mode || self.crop_mode {
            return &[Kind::Raw, Kind::Full, Kind::Loupe, Kind::Fast];
        }
        match self.zoom {
            Zoom::Fit => &[Kind::Loupe, Kind::Full, Kind::Fast],
            Zoom::Actual { .. } if self.has_full_size_jpeg(index) => &[Kind::Full, Kind::Raw, Kind::Loupe, Kind::Fast],
            Zoom::Actual { .. } | Zoom::Region { .. } | Zoom::Frame { .. } => &[Kind::Raw, Kind::Full, Kind::Loupe, Kind::Fast],
        }
    }

    /// Whether the photo's biggest JPEG is (nearly) as big as the sensor. Without the raw to
    /// compare with, 4000 pixels counts as full size: true of every camera so far.
    fn has_full_size_jpeg(&self, index: usize) -> bool {
        let Some(full) = self.jobs.preview(index, Kind::Full) else { return false };
        let jpeg_edge = full.source_width.max(full.source_height) as f32;
        match self.jobs.raw(index) {
            Some(raw) => jpeg_edge >= 0.9 * raw.width.max(raw.height) as f32,
            None => jpeg_edge >= 4000.0,
        }
    }

    /// The photo's full size after rotation, which 100% zoom is relative to: the raw's if it's
    /// been decoded, else the biggest JPEG's.
    fn photo_size(&self, index: usize) -> Option<(u32, u32)> {
        if let Some(raw) = self.jobs.raw(index) {
            let (w, h) = (raw.width, raw.height);
            return Some(if raw.orientation.swaps_axes() { (h, w) } else { (w, h) });
        }
        [Kind::Full, Kind::Loupe, Kind::Fast].iter().find_map(|&k| {
            self.jobs.preview(index, k).map(|p| {
                let (w, h) = (p.source_width, p.source_height);
                if p.orientation.swaps_axes() { (h, w) } else { (w, h) }
            })
        })
    }

    fn draw(&mut self) -> windows::core::Result<()> {
        let Some(index) = self.focus else { return Ok(()) };
        if self.presenter.is_none() && self.offscreen.is_none() {
            return Ok(());
        }
        let Some(photo_size) = self.photo_size(index) else { return Ok(()) };

        // The best layer that's ready, put on the GPU if it isn't already.
        let mut open_ms = 0.0;
        let mut chosen = None;
        for &kind in self.preferences(index) {
            if kind == Kind::Raw {
                if matches!(&self.image, Some((i, _)) if *i == index) {
                    chosen = Some((Kind::Raw, None));
                    break;
                }
                if let Some(frame) = self.jobs.raw(index) {
                    self.image = None; // its memory may be about to be reused
                    let (image, t) = Image::open(&mut self.gpu, &mut self.memory, &frame)?;
                    open_ms = t.total_ms;
                    self.image = Some((index, image));
                    chosen = Some((Kind::Raw, None));
                    break;
                }
            } else if let Some(p) = self.jobs.preview(index, kind) {
                let slot = self.put_preview(index, &p, slot_for(kind))?;
                chosen = Some((kind, Some(slot)));
                break;
            }
        }
        let Some((kind, slot)) = chosen else { return Ok(()) };
        // Noise reduction needs the photo's noise level, measured once per photo.
        if slot.is_none() {
            let d = self.edit.detail();
            if (d.luminance > 0.0 || d.color > 0.0) && let Some((_, image)) = &self.image {
                image.ensure_noise(&mut self.gpu)?;
            }
        }

        let layer = match slot {
            None => Layer::Raw(&self.image.as_ref().unwrap().1),
            Some(s) => Layer::Preview(self.previews[s].texture.as_ref().unwrap()),
        };
        let (raw_width, raw_height) = self.jobs.raw(index).map_or((0, 0), |r| (r.width, r.height));
        let edit = self.edit;
        let masks = self.masks;
        let mask_input = MaskInput { masks: &masks, overlay: self.mask_overlay };
        let zoom = self.view_zoom();
        // What the view shows: the crop, or in crop mode the whole frame, and where on the view.
        let frame = geometry::frame_size(photo_size, edit.rotation);
        let shown = if self.crop_mode { [0.0, 0.0, frame.0 as f64, frame.1 as f64] } else { geometry::window(frame, edit.crop) };
        let (width, height) = (shown[2].round().max(1.0) as u32, shown[3].round().max(1.0) as u32);
        let out = self.presenter.as_ref().map(|p| p.size()).or(self.offscreen.as_ref().map(|t| (t.width, t.height))).unwrap_or((1, 1));
        let rect = geometry::view_map((1, 1), 0, frame, edit.angle, edit.crop, zoom, out).clip;
        let map = self.jobs.raw(index).map_or([0.0; 6], |r| {
            let m = geometry::view_map((r.width, r.height), geometry::turns(r.orientation, edit.rotation), frame, edit.angle, edit.crop, zoom, out);
            [m.origin[0], m.origin[1], m.step_x[0], m.step_x[1], m.step_y[0], m.step_y[1]]
        });
        let timings = match &mut self.presenter {
            Some(p) => p.present(&mut self.gpu, |gpu, target| render_masked(gpu, layer, &edit, &mask_input, zoom, photo_size, target))?,
            None => {
                let target = self.offscreen.as_ref().unwrap();
                let start = Instant::now();
                self.gpu.begin()?;
                let a = self.gpu.timestamp();
                render_masked(&self.gpu, layer, &edit, &mask_input, zoom, photo_size, target);
                let b = self.gpu.timestamp();
                let ts = self.gpu.submit()?;
                let gpu_ms = ts[b as usize] - ts[a as usize];
                crate::present::FrameTimings { develop_ms: gpu_ms, gpu_ms, cpu_ms: start.elapsed().as_secs_f64() * 1000.0 }
            }
        };
        (self.events)(&FrameEvent {
            kind: EVENT_FRAME,
            photo: index as i32,
            layer: layer_code(kind),
            width,
            height,
            raw_width,
            raw_height,
            view_x: rect[0],
            view_y: rect[1],
            view_w: rect[2] - rect[0],
            view_h: rect[3] - rect[1],
            frame_width: frame.0,
            frame_height: frame.1,
            since_show_ms: self.shown_at.elapsed().as_secs_f64() * 1000.0,
            gpu_ms: timings.gpu_ms,
            cpu_ms: timings.cpu_ms,
            open_ms,
            map,
            ..Default::default()
        });
        Ok(())
    }
}

const FULL: usize = 2;

fn slot_for(kind: Kind) -> usize {
    match kind {
        Kind::Fast => 0,
        Kind::Loupe => 1,
        _ => FULL,
    }
}

/// A preview texture on the GPU, and which photo and decode it holds.
#[derive(Default)]
struct PreviewSlot {
    texture: Option<PreviewTexture>,
    of: Option<(usize, *const Preview)>,
}

impl Viewer {
    /// Makes sure this preview is on the GPU and returns the slot holding it: whichever slot
    /// already has it (a 3200-pixel JPEG can be the quick, loupe and full-size preview at once),
    /// else `slot`, whose texture is reused when the size matches.
    fn put_preview(&mut self, index: usize, preview: &Arc<Preview>, slot: usize) -> windows::core::Result<usize> {
        let key = (index, Arc::as_ptr(preview));
        if let Some(s) = self.previews.iter().position(|s| s.of == Some(key)) {
            return Ok(s);
        }
        let reuse = self.previews[slot].texture.take();
        self.previews[slot].of = None;
        self.previews[slot].texture = Some(PreviewTexture::upload(&mut self.gpu, reuse, &preview.pixels, preview.orientation)?);
        self.previews[slot].of = Some(key);
        Ok(slot)
    }

    /// The full-size preview of the photo in view, if it's decoded, not on the GPU yet, and the
    /// photo is shown fitted in Cull mode (so a zoom to 100% would want it next).
    fn full_preview_to_upload(&self) -> Option<(usize, Arc<Preview>)> {
        let index = self.focus?;
        if self.edit_mode || !matches!(self.zoom, Zoom::Fit) {
            return None;
        }
        let full = self.jobs.preview(index, Kind::Full)?;
        let key = (index, Arc::as_ptr(&full));
        (!self.previews.iter().any(|s| s.of == Some(key))).then_some((index, full))
    }

    fn upload_full_preview(&mut self) -> windows::core::Result<()> {
        if let Some((index, full)) = self.full_preview_to_upload() {
            self.put_preview(index, &full, FULL)?;
        }
        Ok(())
    }
}

/// The export being worked on.
struct ActiveExport {
    job: ExportJob,
    /// The folder changed since it was asked for: its index no longer means this photo, so it
    /// decodes its own raw and keeps to a place in the GPU thread's image slot no photo in view uses.
    detached: bool,
    started: Instant,
    /// Waiting for the raw to decode on a helper thread.
    decoding: bool,
    frame: Option<Arc<RawFrame>>,
    run: Option<ExportRun>,
}

/// Rendering is under way: the tiles, and the way to the encoder thread.
struct ExportRun {
    renderer: TileRenderer,
    strips: SyncSender<Strip>,
    /// A finished band the encoder had no room for yet.
    pending: Option<Strip>,
}

/// What an export step found.
enum Next {
    /// More to do now.
    Again,
    /// Waiting for a helper thread.
    Wait,
    Finished,
}

impl Viewer {
    /// Does one small piece of the export at the front of the queue: opening the photo, one
    /// tile, or handing a band to the encoder.
    fn export_step(&mut self) {
        if self.export.is_none() {
            let Some((job, detached)) = self.exports.pop_front() else { return };
            let (id, index) = (job.id, job.index);
            self.export = Some(ActiveExport { job, detached, started: Instant::now(), decoding: false, frame: None, run: None });
            self.emit_export(id, index, EXPORT_PROGRESS, 0.0, None);
        }
        let mut active = self.export.take().unwrap();
        match self.advance_export(&mut active) {
            Ok(Next::Again) => {
                self.export_waiting = false;
                self.export = Some(active);
            }
            Ok(Next::Wait) => {
                self.export_waiting = true;
                self.export = Some(active);
            }
            Ok(Next::Finished) => {
                // The encoder thread finishes the file and reports; dropping the run closes its input.
                let run = active.run.take().unwrap();
                let (render_ms, readback_ms) = (run.renderer.render_ms, run.renderer.readback_ms);
                self.export_meta.insert(active.job.id, (active.job.index, active.started, render_ms, readback_ms));
                self.export_waiting = false;
                self.dirty = true; // the export may have taken the picture's place on the GPU
            }
            Err(message) => {
                self.export_waiting = false;
                self.emit_export(active.job.id, active.job.index, EXPORT_FAILED, 0.0, Some(&message));
                self.dirty = true;
            }
        }
    }

    fn advance_export(&mut self, a: &mut ActiveExport) -> Result<Next, String> {
        if a.run.is_none() {
            if !self.ensure_export_image(a)? {
                return Ok(Next::Wait);
            }
            let image = &self.image.as_ref().unwrap().1;
            let window = export::window(image.oriented_size(), &a.job.edit);
            let renderer = TileRenderer::new(&self.gpu, window, &a.job.edit).map_err(|e| e.to_string())?;
            let (strips, bands) = sync_channel(1);
            let (id, quality, raw, path, commands) =
                (a.job.id, a.job.request.quality, a.job.raw.clone(), a.job.path.clone(), self.commands.clone());
            std::thread::Builder::new()
                .name("windedit export".into())
                .spawn(move || {
                    let result = export::encode(bands, (window.width, window.height), quality, &raw, &path);
                    let _ = commands.send(Command::ExportFinished(id, result));
                })
                .map_err(|e| e.to_string())?;
            a.run = Some(ExportRun { renderer, strips, pending: None });
            return Ok(Next::Again);
        }

        {
            let run = a.run.as_mut().unwrap();
            if let Some(strip) = run.pending.take() {
                match run.strips.try_send(strip) {
                    Ok(()) => {}
                    Err(TrySendError::Full(strip)) => {
                        run.pending = Some(strip);
                        return Ok(Next::Wait);
                    }
                    Err(TrySendError::Disconnected(_)) => return Err("the encoder stopped".into()),
                }
            }
            if run.renderer.done() {
                return Ok(Next::Finished);
            }
        }
        // Another photo may have taken the GPU memory between tiles (the user moved on in the
        // Edit view): open this one again.
        if !self.ensure_export_image(a)? {
            return Ok(Next::Wait);
        }
        let run = a.run.as_mut().unwrap();
        let image = &self.image.as_ref().unwrap().1;
        run.pending = run.renderer.step_masked(&mut self.gpu, image, &a.job.edit, &a.job.masks).map_err(|e| e.to_string())?;
        let progress = run.renderer.tiles_done() as f32 / run.renderer.tiles() as f32;
        self.emit_export(a.job.id, a.job.index, EXPORT_PROGRESS, progress, None);
        Ok(Next::Again)
    }

    /// Makes sure the GPU holds the photo being exported, opening it from the cache, from a raw
    /// decoded for it, or (asking a helper thread to decode it) not yet. Returns whether it's there.
    fn ensure_export_image(&mut self, a: &mut ActiveExport) -> Result<bool, String> {
        let key = if a.detached { usize::MAX } else { a.job.index };
        if matches!(&self.image, Some((k, _)) if *k == key) {
            return Ok(true);
        }
        let cached = if a.detached { None } else { self.jobs.raw(a.job.index) };
        let Some(frame) = a.frame.clone().or(cached) else {
            if !a.decoding {
                a.decoding = true;
                let (id, path, commands) = (a.job.id, a.job.raw.clone(), self.commands.clone());
                std::thread::Builder::new()
                    .name("windedit export decode".into())
                    .spawn(move || {
                        let frame = decode::decode_file(&path).map(Arc::new).map_err(|e| format!("couldn't read {}: {e}", path.display()));
                        let _ = commands.send(Command::ExportDecoded(id, frame));
                    })
                    .map_err(|e| e.to_string())?;
            }
            return Ok(false);
        };
        self.image = None; // its memory may be about to be reused
        let (image, _) = Image::open(&mut self.gpu, &mut self.memory, &frame).map_err(|e| e.to_string())?;
        self.image = Some((key, image));
        Ok(true)
    }

    fn export_decoded(&mut self, id: u32, frame: Result<Arc<RawFrame>, String>) {
        let Some(a) = &mut self.export else { return };
        if a.job.id != id {
            return;
        }
        a.decoding = false;
        self.export_waiting = false;
        match frame {
            Ok(frame) => a.frame = Some(frame),
            Err(message) => {
                let (id, index) = (a.job.id, a.job.index);
                self.export = None;
                self.emit_export(id, index, EXPORT_FAILED, 0.0, Some(&message));
            }
        }
    }

    fn export_finished(&mut self, id: u32, result: Result<Encoded, String>) {
        // Not in the list when rendering failed, which has been reported already.
        let Some((index, started, render_ms, readback_ms)) = self.export_meta.remove(&id) else { return };
        let (state, text, encode_ms) = match result {
            Ok(encoded) => (EXPORT_DONE, encoded.path.display().to_string(), encoded.encode_ms),
            Err(message) => (EXPORT_FAILED, message, 0.0),
        };
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        (self.events)(&FrameEvent {
            kind: EVENT_EXPORT,
            photo: index as i32,
            since_show_ms: started.elapsed().as_secs_f64() * 1000.0,
            gpu_ms: render_ms,
            cpu_ms: readback_ms,
            open_ms: encode_ms,
            export_id: id,
            export_state: state,
            progress: 1.0,
            text: wide.as_ptr() as usize,
            ..Default::default()
        });
    }

    fn emit_export(&self, id: u32, index: usize, state: u32, progress: f32, text: Option<&str>) {
        let wide: Vec<u16> = text.map(|t| t.encode_utf16().chain(Some(0)).collect()).unwrap_or_default();
        (self.events)(&FrameEvent {
            kind: EVENT_EXPORT,
            photo: index as i32,
            export_id: id,
            export_state: state,
            progress,
            text: if text.is_some() { wide.as_ptr() as usize } else { 0 },
            ..Default::default()
        });
    }
}

impl Viewer {
    /// How the view is drawn: the user's zoom, or in crop mode the whole frame.
    fn view_zoom(&self) -> Zoom {
        if self.crop_mode { Zoom::Frame { x: self.crop_center.0, y: self.crop_center.1 } } else { self.zoom }
    }
}

impl Viewer {
    /// The white balance that makes the spot at (`x`, `y`) on the view neutral, read from the raw
    /// of the photo in view through the same transform the last frame drew it with.
    fn pick(&self, x: f32, y: f32) -> Result<(f32, f32), String> {
        let index = self.focus.ok_or("There is no photo to pick from.")?;
        let frame = self.jobs.raw(index).ok_or("The raw is still being read.")?;
        let out = self.presenter.as_ref().map(|p| p.size()).or(self.offscreen.as_ref().map(|t| (t.width, t.height))).unwrap_or((1, 1));
        let zoom = self.view_zoom();
        crate::pick::pick(&frame, &self.edit, zoom, out, (x, y)).map_err(|e| e.to_string())
    }
}
