//! Windows-specific platform integrations for Wallora.

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::FILETIME;
#[cfg(target_os = "windows")]
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
#[cfg(target_os = "windows")]
use windows::core::PCWSTR;

/// Shows a native Windows error dialog box. Used by the application panic hook.
pub fn show_error_dialog(title: &str, message: &str) {
    #[cfg(target_os = "windows")]
    {
        let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let message_wide: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: Both buffers are valid, null-terminated UTF-16 slices alive for the call.
        unsafe {
            let _ = MessageBoxW(
                None,
                PCWSTR(message_wide.as_ptr()),
                PCWSTR(title_wide.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (title, message);
    }
}

/// Returns the current process working set (RSS) in bytes on Windows.
pub fn current_process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: `GetCurrentProcess()` returns a pseudo-handle; `counters` is properly sized.
        let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
        if ok.is_ok() {
            Some(counters.WorkingSetSize as u64)
        } else {
            None
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// Returns cumulative kernel + user CPU time of the current process in milliseconds.
pub fn current_process_cpu_time_ms() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: `GetCurrentProcess()` returns a pseudo-handle and all FILETIME pointers are valid.
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        if ok.is_ok() {
            let to_100ns =
                |ft: FILETIME| ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64);
            let total_100ns = to_100ns(kernel).saturating_add(to_100ns(user));
            Some(total_100ns / 10_000)
        } else {
            None
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}
