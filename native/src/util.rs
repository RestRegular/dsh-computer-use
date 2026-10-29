//! Small shared helpers for the native Windows helper.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Encode a Rust string as a NUL-terminated UTF-16 buffer for Win32 W APIs.
pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a Rust string from a NUL-terminated UTF-16 buffer.
pub fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// Parse a `0x...` window handle string.
pub fn hwnd_from_str(value: &str) -> Result<*mut c_void, String> {
    let text = value.trim();
    let text = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    let number = u64::from_str_radix(text, 16).map_err(|_| format!("invalid window id: {value}"))?;
    if number == 0 {
        return Err("window id must not be zero".to_string());
    }
    Ok(number as usize as *mut c_void)
}

/// Format a window handle as `0x00000000`.
pub fn hwnd_to_string(hwnd: *mut c_void) -> String {
    format!("0x{:08X}", hwnd as usize)
}

/// Last Win32 error formatted for a response.
pub fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Sleep while keeping the function name readable.
pub fn sleep_ms(ms: u64) {
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// Current wall clock in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

const DESKTOP_SWITCHDESKTOP: u32 = 0x0100;

#[link(name = "user32")]
extern "system" {
    fn OpenInputDesktop(dw_flags: u32, f_inherit: i32, dw_desired_access: u32) -> isize;
    fn CloseDesktop(h_desktop: isize) -> i32;
}

/// True when the interactive input desktop is not reachable (lock screen, UAC
/// secure prompt, or a disconnected session).
pub fn is_secure_desktop() -> bool {
    unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP);
        if desktop == 0 {
            return true;
        }
        CloseDesktop(desktop);
        false
    }
}

/// True when the helper itself runs with an elevated token. Elevated target
/// applications will silently reject injected input from a non-elevated helper.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut length = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}
