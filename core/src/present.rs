//! Getting developed pixels onto the screen through a composition swap chain, which a WinUI
//! SwapChainPanel (or any DirectComposition visual) can show.

use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, Result};

use crate::gpu::Gpu;
use crate::pipeline::{Edit, Image, TARGET_FORMAT, Target, Zoom, render};

const BUFFERS: u32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameTimings {
    /// GPU time of the develop pass.
    pub develop_ms: f64,
    /// GPU time of develop plus the copy to the screen buffer.
    pub gpu_ms: f64,
    /// Wall clock from the start of the call until the frame is handed to the compositor.
    pub cpu_ms: f64,
}

pub struct Presenter {
    pub chain: IDXGISwapChain3,
    buffers: Vec<ID3D12Resource>,
    target: Target,
}

impl Presenter {
    /// `width` and `height` in physical pixels; `scale` is the panel's DPI scale, undone with
    /// the swap chain's transform so one buffer pixel is one screen pixel.
    pub fn new(gpu: &Gpu, width: u32, height: u32, scale: f32) -> Result<Presenter> {
        let (width, height) = (width.max(1), height.max(1));
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: TARGET_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFERS,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        let chain: IDXGISwapChain3 = unsafe { gpu.factory.CreateSwapChainForComposition(&gpu.queue, &desc, None)? }.cast()?;
        let presenter = Presenter { buffers: buffers(&chain)?, target: Target::new(gpu, width, height)?, chain };
        presenter.set_scale(scale)?;
        Ok(presenter)
    }

    pub fn resize(&mut self, gpu: &mut Gpu, width: u32, height: u32, scale: f32) -> Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if width != self.target.width || height != self.target.height {
            gpu.wait_idle()?;
            self.buffers.clear();
            unsafe { self.chain.ResizeBuffers(BUFFERS, width, height, TARGET_FORMAT, DXGI_SWAP_CHAIN_FLAG(0))? };
            self.buffers = buffers(&self.chain)?;
            self.target = Target::new(gpu, width, height)?;
        }
        self.set_scale(scale)
    }

    fn set_scale(&self, scale: f32) -> Result<()> {
        let inverse = 1.0 / scale.max(0.1);
        let matrix = DXGI_MATRIX_3X2_F { _11: inverse, _22: inverse, ..Default::default() };
        unsafe { self.chain.SetMatrixTransform(&matrix) }
    }

    pub fn present(&mut self, gpu: &mut Gpu, image: &Image, edit: &Edit, zoom: Zoom) -> Result<FrameTimings> {
        let start = Instant::now();
        let back = &self.buffers[unsafe { self.chain.GetCurrentBackBufferIndex() } as usize];
        gpu.begin()?;
        let t0 = gpu.timestamp();
        render(gpu, image, edit, zoom, &self.target);
        let t1 = gpu.timestamp();
        gpu.transition(&self.target.texture, 0, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        gpu.transition(back, 0, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_COPY_DEST);
        unsafe { gpu.list.CopyResource(back, &self.target.texture) };
        gpu.transition(back, 0, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_PRESENT);
        gpu.transition(&self.target.texture, 0, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        let t2 = gpu.timestamp();
        let ts = gpu.submit()?;
        unsafe { self.chain.Present(1, DXGI_PRESENT(0)).ok()? };
        Ok(FrameTimings {
            develop_ms: ts[t1 as usize] - ts[t0 as usize],
            gpu_ms: ts[t2 as usize] - ts[t0 as usize],
            cpu_ms: start.elapsed().as_secs_f64() * 1000.0,
        })
    }
}

fn buffers(chain: &IDXGISwapChain3) -> Result<Vec<ID3D12Resource>> {
    (0..BUFFERS).map(|i| unsafe { chain.GetBuffer(i) }).collect()
}
