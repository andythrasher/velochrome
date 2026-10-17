//! The GPU thread. It owns the Direct3D device and the swap chain, and draws whatever is the
//! best thing available for the photo in view: one of the camera's JPEGs while culling, the
//! developed raw when editing or at 100% when no full-size JPEG exists. When something better
//! arrives from the decode workers it simply draws again.
//!
//! Nothing here waits for decoding, and the app's UI thread never waits for the GPU (only
//! resizing the view waits for a reply).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::core::Interface;

use crate::gpu::Gpu;
use crate::jobs::{Jobs, Kind};
use crate::pipeline::{Edit, Image, ImageMemory, Layer, PreviewTexture, Slot, Target, Zoom, render, warm_up};
use crate::present::Presenter;
use crate::preview::Preview;

pub enum Command {
    /// Show this photo (index into the folder).
    Show(usize),
    Zoom(Zoom),
    /// The adjustments of the photo in view.
    SetEdit(Edit),
    /// Edit mode shows the developed raw; Cull mode shows camera JPEGs.
    EditMode(bool),
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
    Shutdown,
}

/// What a drawn frame showed, sent to the app after every frame.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameEvent {
    /// `EVENT_FRAME` or `EVENT_THUMBNAIL`.
    pub kind: u32,
    pub photo: i32,
    /// What was drawn: 1 quick preview, 2 loupe preview, 3 full-size preview, 4 the raw.
    pub layer: u32,
    /// The photo's full size after rotation (the raw's once it's decoded, before that its biggest
    /// JPEG's): what fractions in `we_set_zoom` are relative to.
    pub width: u32,
    pub height: u32,
    /// The raw's sensor size, once decoded (else 0): what `we_prepare` takes.
    pub raw_width: u32,
    pub raw_height: u32,
    /// From the request to show this photo until this frame was handed to the screen.
    pub since_show_ms: f64,
    pub gpu_ms: f64,
    /// Wall clock of drawing and presenting this frame.
    pub cpu_ms: f64,
    /// Uploading and demosaicing the raw, when this frame did it (else 0).
    pub open_ms: f64,
}

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
            let allocator = spawn_allocator(&gpu, tx);
            let _ = ready_tx.send(Ok(GpuInfo { adapter_name: gpu.adapter_name.clone(), uma: gpu.uma, warm_up_ms }));
            Viewer::new(gpu, jobs, events, allocator).run(rx);
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
    memory: ImageMemory,
    presenter: Option<Presenter>,
    offscreen: Option<Target>,
    focus: Option<usize>,
    shown_at: Instant,
    zoom: Zoom,
    edit: Edit,
    edit_mode: bool,
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
    fn new(gpu: Gpu, jobs: Arc<Jobs>, events: EventSink, allocator: Sender<(u32, u32)>) -> Viewer {
        Viewer {
            gpu,
            jobs,
            events,
            allocator,
            allocating: HashSet::new(),
            memory: ImageMemory::new(),
            presenter: None,
            offscreen: None,
            focus: None,
            shown_at: Instant::now(),
            zoom: Zoom::Fit,
            edit: Edit::default(),
            edit_mode: false,
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
            let first = if self.full_preview_to_upload().is_some() {
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
            Command::EditMode(on) => {
                self.edit_mode = on;
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
            Command::Folder => {
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
        if self.edit_mode {
            return &[Kind::Raw, Kind::Full, Kind::Loupe, Kind::Fast];
        }
        match self.zoom {
            Zoom::Fit => &[Kind::Loupe, Kind::Full, Kind::Fast],
            Zoom::Actual { .. } if self.has_full_size_jpeg(index) => &[Kind::Full, Kind::Raw, Kind::Loupe, Kind::Fast],
            Zoom::Actual { .. } => &[Kind::Raw, Kind::Full, Kind::Loupe, Kind::Fast],
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

        let layer = match slot {
            None => Layer::Raw(&self.image.as_ref().unwrap().1),
            Some(s) => Layer::Preview(self.previews[s].texture.as_ref().unwrap()),
        };
        let (width, height) = photo_size;
        let (raw_width, raw_height) = self.jobs.raw(index).map_or((0, 0), |r| (r.width, r.height));
        let (edit, zoom) = (self.edit, self.zoom);
        let timings = match &mut self.presenter {
            Some(p) => p.present(&mut self.gpu, |gpu, target| render(gpu, layer, &edit, zoom, photo_size, target))?,
            None => {
                let target = self.offscreen.as_ref().unwrap();
                let start = Instant::now();
                self.gpu.begin()?;
                let a = self.gpu.timestamp();
                render(&self.gpu, layer, &edit, zoom, photo_size, target);
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
            since_show_ms: self.shown_at.elapsed().as_secs_f64() * 1000.0,
            gpu_ms: timings.gpu_ms,
            cpu_ms: timings.cpu_ms,
            open_ms,
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
