//! Windows-specific platform integrations for Wallora.

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
