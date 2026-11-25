//! Windows-specific platform integrations for Wallora.

pub mod credentials;
pub mod file_publish;

pub use credentials::{DEFAULT_SERVICE_NAME, WindowsCredentialStore};
pub use file_publish::WindowsFilePublisher;

use url::Url;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::FILETIME;
#[cfg(target_os = "windows")]
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
#[cfg(target_os = "windows")]
use windows::Win32::UI::Shell::ShellExecuteW;
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW, SW_SHOWNORMAL};
#[cfg(target_os = "windows")]
use windows::core::PCWSTR;

/// Validates that an external hand-off URL points strictly to `https://wallhaven.cc/...` with
/// no embedded credentials.
pub fn validate_wallhaven_handoff_url(raw: &str) -> Result<Url, String> {
    let parsed = Url::parse(raw).map_err(|_| "invalid external URL".to_string())?;
    if !parsed.scheme().eq_ignore_ascii_case("https") {
        return Err("external hand-off URL must use HTTPS".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("external hand-off URL must not contain credentials".to_string());
    }
    let host_ok = parsed
        .host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case("wallhaven.cc"));
    if !host_ok {
        return Err("external hand-off URL must point to wallhaven.cc".to_string());
    }
    Ok(parsed)
}

/// Opens a validated `https://wallhaven.cc/...` page in the user's default Windows browser.
pub fn open_wallhaven_in_browser(raw_url: &str) -> Result<(), String> {
    let parsed = validate_wallhaven_handoff_url(raw_url)?;
    #[cfg(target_os = "windows")]
    {
        let verb_wide: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        let url_wide: Vec<u16> = parsed
            .as_str()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: Both buffers are valid, null-terminated UTF-16 slices alive for the call.
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb_wide.as_ptr()),
                PCWSTR(url_wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if (result.0 as isize) > 32 {
            Ok(())
        } else {
            Err("Windows could not launch the default browser".to_string())
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = parsed;
        Err("external browser hand-off is only supported on Windows".to_string())
    }
}

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

#[cfg(test)]
mod tests {
    use super::validate_wallhaven_handoff_url;

    #[test]
    fn handoff_url_validation_allows_only_https_wallhaven() {
        assert!(validate_wallhaven_handoff_url("https://wallhaven.cc/settings").is_ok());
        assert!(validate_wallhaven_handoff_url("http://wallhaven.cc/settings").is_err());
        assert!(validate_wallhaven_handoff_url("https://evil.example/settings").is_err());
        assert!(validate_wallhaven_handoff_url("https://user:pass@wallhaven.cc/settings").is_err());
        assert!(validate_wallhaven_handoff_url("cmd.exe /c calc").is_err());
    }
}
