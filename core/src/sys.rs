//! Small Windows system calls: memory size and background thread scheduling.

use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadInformation, SetThreadPriority, THREAD_POWER_THROTTLING_CURRENT_VERSION,
    THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_BELOW_NORMAL,
    THREAD_PRIORITY_NORMAL, ThreadPowerThrottling,
};

/// Installed memory, in bytes (8 GB if it can't be read).
pub fn physical_memory_bytes() -> usize {
    let mut status = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    match unsafe { GlobalMemoryStatusEx(&mut status) } {
        Ok(()) => status.ullTotalPhys as usize,
        Err(_) => 8 << 30,
    }
}

/// Marks the calling thread as background work (or back to normal). Background threads run
/// below normal priority with EcoQoS, which Windows schedules on efficient cores at low clocks:
/// on a hybrid CPU that keeps the performance cores free for what the user is waiting on, and
/// saves battery.
pub fn set_background(background: bool) {
    if std::env::var_os("WINDEDIT_NO_ECO").is_some() {
        return; // for measuring what EcoQoS costs
    }
    unsafe {
        let thread = GetCurrentThread();
        let state = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: if background { THREAD_POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
        };
        let _ = SetThreadInformation(
            thread,
            ThreadPowerThrottling,
            &state as *const _ as *const _,
            size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
        let _ = SetThreadPriority(thread, if background { THREAD_PRIORITY_BELOW_NORMAL } else { THREAD_PRIORITY_NORMAL });
    }
}
