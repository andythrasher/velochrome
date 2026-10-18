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

/// A file's modified time as the wall clock read in the user's time zone then, for filing a photo
/// that has no EXIF date under the day it was made.
pub fn local_stamp(time: std::time::SystemTime) -> Option<crate::exif::Stamp> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // FILETIME counts 100 ns ticks from 1601; the Unix epoch is 11 644 473 600 s after that.
    let since_unix = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    let ticks = (since_unix.as_secs() + 11_644_473_600) * 10_000_000 + since_unix.subsec_nanos() as u64 / 100;
    let file = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        FileTimeToSystemTime(&file, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(crate::exif::Stamp {
        year: local.wYear,
        month: local.wMonth as u8,
        day: local.wDay as u8,
        hour: local.wHour as u8,
        minute: local.wMinute as u8,
        second: local.wSecond as u8,
    })
}
