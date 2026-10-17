//! The engine as apps see it: open a folder, show a photo, zoom, edit. Calls return at once;
//! decoding happens on worker threads and drawing on the GPU thread, which reports each frame
//! through the event callback.

use std::path::Path;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::jobs::{self, Focus, Jobs};
use crate::library::{self, Photo};
use crate::pipeline::Zoom;
use crate::viewer::{self, Command, EventSink, GpuInfo};

pub struct Engine {
    jobs: Arc<Jobs>,
    viewer: Sender<Command>,
    gpu_thread: Option<JoinHandle<()>>,
    pub info: GpuInfo,
    view: Mutex<ViewState>,
}

/// What decides which decodes are wanted.
struct ViewState {
    index: Option<usize>,
    direction: i32,
    view_size: (u32, u32),
    actual_size: bool,
    edit_mode: bool,
}

impl Engine {
    /// Starts the decode workers and the GPU thread. `events` is called on the GPU thread after
    /// every frame drawn.
    pub fn new(events: EventSink) -> Result<Engine, String> {
        let (tx, rx) = channel();
        let ready = tx.clone();
        let jobs = Jobs::new(Box::new(move |index, kind| {
            let _ = ready.send(Command::Ready(index, kind));
        }));
        let (gpu_thread, info) = match viewer::spawn(jobs.clone(), events, tx.clone(), rx) {
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
            view: Mutex::new(ViewState {
                index: None,
                direction: 1,
                view_size: (2560, 1440),
                actual_size: false,
                edit_mode: false,
            }),
        })
    }

    /// Opens a folder; returns how many photos it has. Shows nothing until `show`.
    pub fn open_folder(&self, dir: &Path) -> std::io::Result<usize> {
        let photos = library::scan_folder(dir)?;
        let count = photos.len();
        self.view.lock().unwrap().index = None;
        let _ = self.viewer.send(Command::Folder);
        self.jobs.set_photos(Arc::new(photos));
        Ok(count)
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

    pub fn set_exposure(&self, stops: f32) {
        let _ = self.viewer.send(Command::Exposure(stops));
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

    pub fn jobs(&self) -> &Arc<Jobs> {
        &self.jobs
    }

    fn refocus(&self, v: &ViewState) {
        let Some(index) = v.index else { return };
        self.jobs.set_focus(Focus {
            index,
            direction: v.direction,
            loupe_edge: v.view_size.0.max(v.view_size.1),
            need_raw: v.edit_mode || v.actual_size,
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
    }
}
