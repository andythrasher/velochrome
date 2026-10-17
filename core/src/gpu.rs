//! The Direct3D 12 device and the small set of helpers the pipeline needs. Phase 0 keeps this
//! deliberately simple: one queue, one command list, and every submission waits for the GPU,
//! which also makes the timings honest end-to-end numbers.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, Result};

use crate::tone::BASE_EDGE;

/// A compute kernel: its root signature (compiled into the shader) and pipeline state.
pub struct Kernel {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

pub struct Kernels {
    pub demosaic: Kernel,
    pub downsample: Kernel,
    pub develop: Kernel,
    pub tone_small: Kernel,
    pub tone_guide: Kernel,
    pub tone_stats: Kernel,
}

/// How many timestamps one submission can record.
pub const MAX_TIMESTAMPS: u32 = 64;
/// Size of the shader-visible descriptor heap.
const DESCRIPTORS: u32 = 256;

pub struct Gpu {
    pub factory: IDXGIFactory6,
    pub adapter_name: String,
    /// The GPU shares memory with the CPU (integrated graphics), so the CPU can write straight
    /// into memory the GPU reads.
    pub uma: bool,
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    pub list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    fence_value: u64,
    event: HANDLE,
    heap: ID3D12DescriptorHeap,
    increment: u32,
    queries: ID3D12QueryHeap,
    query_readback: ID3D12Resource,
    timestamps_used: u32,
    /// GPU timestamp ticks per second.
    pub timestamp_freq: u64,
    pub kernels: Kernels,
    /// The tone parameters the shaders read: in upload memory, mapped for good, and rewritten
    /// for each frame (safe, as every submission waits for the GPU).
    params: ID3D12Resource,
    params_ptr: *mut u8,
    /// What the tone pre-pass works in. Small and fixed in size whatever the photo, so they are
    /// made once here.
    pub tone: ToneResources,
}

/// Room for the tone parameters.
const PARAMS_BYTES: u64 = 256;

/// Textures and a buffer for the tone pre-pass, which reduces the photo to a copy about
/// `BASE_EDGE` pixels across. All stay in the unordered access state, except while the develop
/// pass reads `coefficients` and `stats`.
pub struct ToneResources {
    /// The photo's tones, one pixel per block.
    pub small: ID3D12Resource,
    /// Two quantities side by side per pixel, for the guided filter's intermediate steps.
    pub scratch_a: ID3D12Resource,
    pub scratch_b: ID3D12Resource,
    /// The photo's tones as shot (before Exposure and the tone sliders), one pixel per block:
    /// what the percentiles are taken from.
    pub scene: ID3D12Resource,
    /// The guided filter's a and b.
    pub coefficients: ID3D12Resource,
    /// The 99.5th and 25th percentiles of the tones.
    pub stats: ID3D12Resource,
}

impl ToneResources {
    fn new(device: &ID3D12Device) -> Result<ToneResources> {
        let edge = BASE_EDGE as u32;
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        Ok(ToneResources {
            small: new_texture(device, edge, edge, 1, DXGI_FORMAT_R32_FLOAT, uav)?,
            scratch_a: new_texture(device, edge * 2, edge, 1, DXGI_FORMAT_R32_FLOAT, uav)?,
            scratch_b: new_texture(device, edge * 2, edge, 1, DXGI_FORMAT_R32_FLOAT, uav)?,
            scene: new_texture(device, edge, edge, 1, DXGI_FORMAT_R32_FLOAT, uav)?,
            coefficients: new_texture(device, edge, edge, 1, DXGI_FORMAT_R32G32_FLOAT, uav)?,
            stats: new_uav_buffer(device, 16)?,
        })
    }
}

impl Gpu {
    /// Opens the fastest GPU (the discrete one on a machine with both).
    pub fn new() -> Result<Gpu> {
        unsafe {
            if std::env::var_os("WINDEDIT_D3D_DEBUG").is_some() {
                let mut debug: Option<ID3D12Debug> = None;
                if D3D12GetDebugInterface(&mut debug).is_ok()
                    && let Some(debug) = debug
                {
                    debug.EnableDebugLayer();
                }
            }
            let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)?;
            let desc = adapter.GetDesc1()?;
            let name_len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let adapter_name = String::from_utf16_lossy(&desc.Description[..name_len]);

            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            let device = device.unwrap();

            let mut arch = D3D12_FEATURE_DATA_ARCHITECTURE::default();
            device.CheckFeatureSupport(
                D3D12_FEATURE_ARCHITECTURE,
                &mut arch as *mut _ as *mut _,
                size_of::<D3D12_FEATURE_DATA_ARCHITECTURE>() as u32,
            )?;

            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
            list.Close()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;

            let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: DESCRIPTORS,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?;
            let increment = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);

            let mut queries: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: MAX_TIMESTAMPS,
                    NodeMask: 0,
                },
                &mut queries,
            )?;
            let timestamp_freq = queue.GetTimestampFrequency()?;

            let kernels = Kernels {
                demosaic: kernel(&device, include_bytes!(concat!(env!("OUT_DIR"), "/demosaic.dxil")))?,
                downsample: kernel(&device, include_bytes!(concat!(env!("OUT_DIR"), "/downsample.dxil")))?,
                develop: kernel(&device, include_bytes!(concat!(env!("OUT_DIR"), "/develop.dxil")))?,
                tone_small: kernel(&device, include_bytes!(concat!(env!("OUT_DIR"), "/tonesmall.dxil")))?,
                tone_guide: kernel(&device, include_bytes!(concat!(env!("OUT_DIR"), "/toneguide.dxil")))?,
                tone_stats: kernel(&device, include_bytes!(concat!(env!("OUT_DIR"), "/tonestats.dxil")))?,
            };

            let query_readback =
                new_buffer(&device, MAX_TIMESTAMPS as u64 * 8, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let params = new_buffer(&device, PARAMS_BYTES, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let mut params_ptr = std::ptr::null_mut();
            params.Map(0, Some(&D3D12_RANGE::default()), Some(&mut params_ptr))?;
            let tone = ToneResources::new(&device)?;
            Ok(Gpu {
                factory,
                adapter_name,
                uma: arch.UMA.as_bool(),
                device,
                queue,
                allocator,
                list,
                fence,
                fence_value: 0,
                event,
                heap,
                increment,
                queries: queries.unwrap(),
                query_readback,
                timestamps_used: 0,
                timestamp_freq,
                kernels,
                params,
                params_ptr: params_ptr as *mut u8,
                tone,
            })
        }
    }

    /// Starts recording a batch of GPU work.
    pub fn begin(&mut self) -> Result<()> {
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            self.list.SetDescriptorHeaps(&[Some(self.heap.clone())]);
        }
        self.timestamps_used = 0;
        Ok(())
    }

    /// Records a GPU timestamp; returns its index for `submit`'s result.
    pub fn timestamp(&mut self) -> u32 {
        let index = self.timestamps_used;
        assert!(index < MAX_TIMESTAMPS);
        unsafe { self.list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, index) };
        self.timestamps_used += 1;
        index
    }

    /// Runs what was recorded and waits for it. Returns the timestamps, in milliseconds since
    /// the first one.
    pub fn submit(&mut self) -> Result<Vec<f64>> {
        unsafe {
            let n = self.timestamps_used;
            if n > 0 {
                self.list
                    .ResolveQueryData(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0, n, &self.query_readback, 0);
            }
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.wait_idle()?;
            if n == 0 {
                return Ok(Vec::new());
            }
            let mut ptr = std::ptr::null_mut();
            let range = D3D12_RANGE { Begin: 0, End: n as usize * 8 };
            self.query_readback.Map(0, Some(&range), Some(&mut ptr))?;
            let ticks = std::slice::from_raw_parts(ptr as *const u64, n as usize);
            let ms = ticks
                .iter()
                .map(|&t| t.wrapping_sub(ticks[0]) as f64 * 1000.0 / self.timestamp_freq as f64)
                .collect();
            self.query_readback.Unmap(0, Some(&D3D12_RANGE::default()));
            Ok(ms)
        }
    }

    pub fn wait_idle(&mut self) -> Result<()> {
        unsafe {
            self.fence_value += 1;
            self.queue.Signal(&self.fence, self.fence_value)?;
            if self.fence.GetCompletedValue() < self.fence_value {
                self.fence.SetEventOnCompletion(self.fence_value, self.event)?;
                WaitForSingleObject(self.event, INFINITE);
            }
        }
        Ok(())
    }

    pub fn cpu_handle(&self, slot: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let start = unsafe { self.heap.GetCPUDescriptorHandleForHeapStart() };
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr: start.ptr + (slot * self.increment) as usize }
    }

    fn gpu_handle(&self, slot: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE { ptr: start.ptr + (slot * self.increment) as u64 }
    }

    /// Records a kernel run. `table` is the descriptor slot of its input; its output is the
    /// next slot. `threads` is the grid size, in threads (groups are 16×16).
    pub fn dispatch<C: Copy>(&self, kernel: &Kernel, constants: &C, table: u32, threads: (u32, u32)) {
        self.dispatch_groups(kernel, constants, table, (threads.0.div_ceil(16), threads.1.div_ceil(16)));
    }

    /// Like `dispatch`, but for a grid given in thread groups.
    pub fn dispatch_groups<C: Copy>(&self, kernel: &Kernel, constants: &C, table: u32, groups: (u32, u32)) {
        let words = size_of::<C>() / 4;
        assert!(size_of::<C>().is_multiple_of(4) && words <= 52);
        unsafe {
            self.list.SetComputeRootSignature(&kernel.root);
            self.list.SetPipelineState(&kernel.pso);
            self.list
                .SetComputeRoot32BitConstants(0, words as u32, constants as *const C as *const _, 0);
            self.list.SetComputeRootDescriptorTable(1, self.gpu_handle(table));
            self.list.Dispatch(groups.0, groups.1, 1);
        }
    }

    pub fn buffer(&self, size: u64, heap: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES) -> Result<ID3D12Resource> {
        new_buffer(&self.device, size, heap, state)
    }

    pub fn texture(
        &self,
        width: u32,
        height: u32,
        mips: u16,
        format: DXGI_FORMAT,
        state: D3D12_RESOURCE_STATES,
    ) -> Result<ID3D12Resource> {
        new_texture(&self.device, width, height, mips, format, state)
    }

    pub fn srv_texture(&self, resource: &ID3D12Resource, format: DXGI_FORMAT, mip: u32, mips: u32, slot: u32) {
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV { MostDetailedMip: mip, MipLevels: mips, ..Default::default() },
            },
        };
        unsafe { self.device.CreateShaderResourceView(resource, Some(&desc), self.cpu_handle(slot)) };
    }

    /// Writes the tone parameters for the next passes.
    pub fn set_params<T: Copy>(&self, value: &T) {
        assert!(size_of::<T>() as u64 <= PARAMS_BYTES);
        unsafe { std::ptr::copy_nonoverlapping(value as *const T as *const u8, self.params_ptr, size_of::<T>()) };
    }

    /// A view of the parameters, which `set_params` wrote as a `T`.
    pub fn srv_params<T>(&self, slot: u32) {
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 0,
                    NumElements: 1,
                    StructureByteStride: size_of::<T>() as u32,
                    Flags: D3D12_BUFFER_SRV_FLAG_NONE,
                },
            },
        };
        unsafe { self.device.CreateShaderResourceView(&self.params, Some(&desc), self.cpu_handle(slot)) };
    }

    pub fn srv_buffer(&self, resource: &ID3D12Resource, format: DXGI_FORMAT, elements: u32, slot: u32) {
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV { FirstElement: 0, NumElements: elements, ..Default::default() },
            },
        };
        unsafe { self.device.CreateShaderResourceView(resource, Some(&desc), self.cpu_handle(slot)) };
    }

    pub fn uav_texture(&self, resource: &ID3D12Resource, format: DXGI_FORMAT, mip: u32, slot: u32) {
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_UAV { MipSlice: mip, PlaneSlice: 0 } },
        };
        unsafe { self.device.CreateUnorderedAccessView(resource, None, Some(&desc), self.cpu_handle(slot)) };
    }

    pub fn uav_buffer(&self, resource: &ID3D12Resource, format: DXGI_FORMAT, elements: u32, slot: u32) {
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: format,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV { FirstElement: 0, NumElements: elements, ..Default::default() },
            },
        };
        unsafe { self.device.CreateUnorderedAccessView(resource, None, Some(&desc), self.cpu_handle(slot)) };
    }

    /// Makes the writes of the last dispatches visible to the next ones.
    pub fn uav_barrier(&self) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: ManuallyDrop::new(None) }),
            },
        };
        unsafe { self.list.ResourceBarrier(&[barrier]) };
    }

    pub fn transition(&self, resource: &ID3D12Resource, sub: u32, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    // Borrowed for the call, without touching the reference count.
                    pResource: unsafe { std::mem::transmute_copy(resource) },
                    Subresource: sub,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        };
        unsafe { self.list.ResourceBarrier(&[barrier]) };
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let _ = self.wait_idle();
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

fn kernel(device: &ID3D12Device, dxil: &[u8]) -> Result<Kernel> {
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, dxil)?;
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr() as *const _, BytecodeLength: dxil.len() },
            ..Default::default()
        };
        let pso = device.CreateComputePipelineState(&desc);
        drop(ManuallyDrop::into_inner(desc.pRootSignature));
        Ok(Kernel { root, pso: pso? })
    }
}

/// Resource creation needs only the device, which Direct3D 12 lets any thread use, so these can
/// run off the GPU thread.
pub fn new_texture(
    device: &ID3D12Device,
    width: u32,
    height: u32,
    mips: u16,
    format: DXGI_FORMAT,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: width as u64,
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: mips,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    committed(device, D3D12_HEAP_TYPE_DEFAULT, &desc, state)
}

pub fn new_buffer(
    device: &ID3D12Device,
    size: u64,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    committed(device, heap, &desc, state)
}

/// A small buffer a kernel writes and later passes read.
pub fn new_uav_buffer(device: &ID3D12Device, size: u64) -> Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    committed(device, D3D12_HEAP_TYPE_DEFAULT, &desc, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)
}

fn committed(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource> {
    let props = D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() };
    let mut resource: Option<ID3D12Resource> = None;
    unsafe { device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, desc, state, None, &mut resource)? };
    Ok(resource.unwrap())
}
