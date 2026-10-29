//! Top-level window discovery, focus and visibility state.

use std::ffi::c_void;

use serde::Serialize;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, EnumWindows, GetClassNameW,
    GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, SetForegroundWindow,
    ShowWindow, ASFW_ANY, GWL_EXSTYLE, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOW,
    WS_EX_TOOLWINDOW,
};

use crate::util::{from_wide, hwnd_from_str, hwnd_to_string, last_error, sleep_ms, wide};
use crate::{HelperError, HResult};

#[derive(Serialize, Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }
}

#[derive(Serialize, Clone)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub class: String,
    pub process: String,
    pub pid: u32,
    pub bounds: Rect,
    pub state: &'static str,
    pub foreground: bool,
}

pub fn rect_of(hwnd: HWND) -> HResult<Rect> {
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return Err(HelperError::new("WINDOW_NOT_FOUND", format!("GetWindowRect failed: {}", last_error())));
    }
    Ok(Rect {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

fn process_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return String::new();
        }
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length);
        windows_sys::Win32::Foundation::CloseHandle(handle);
        if ok == 0 {
            return String::new();
        }
        let path = from_wide(&buffer[..length as usize]);
        path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
    }
}

fn title_of(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; (length + 1) as usize];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return String::new();
    }
    from_wide(&buffer[..copied as usize])
}

fn class_of(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let copied = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return String::new();
    }
    from_wide(&buffer[..copied as usize])
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            &mut cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    ok == 0 && cloaked != 0
}

fn is_tool_window(hwnd: HWND) -> bool {
    unsafe { (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW) != 0 }
}

fn state_of(hwnd: HWND) -> &'static str {
    unsafe {
        if IsIconic(hwnd) != 0 {
            "minimized"
        } else if IsZoomed(hwnd) != 0 {
            "maximized"
        } else {
            "normal"
        }
    }
}

pub fn info(hwnd: HWND) -> HResult<WindowInfo> {
    if unsafe { IsWindow(hwnd) } == 0 {
        return Err(HelperError::new("WINDOW_NOT_FOUND", "the requested window no longer exists"));
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    Ok(WindowInfo {
        id: hwnd_to_string(hwnd),
        title: title_of(hwnd),
        class: class_of(hwnd),
        process: process_name(pid),
        pid,
        bounds: rect_of(hwnd)?,
        state: state_of(hwnd),
        foreground: unsafe { GetForegroundWindow() == hwnd },
    })
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = &mut *(lparam as *mut Vec<WindowInfo>);
    if windows.len() >= 200 {
        return 0;
    }
    if IsWindowVisible(hwnd) == 0 || is_tool_window(hwnd) || is_cloaked(hwnd) {
        return TRUE;
    }
    let title = title_of(hwnd);
    if title.trim().is_empty() {
        return TRUE;
    }
    let Ok(bounds) = rect_of(hwnd) else {
        return TRUE;
    };
    if bounds.is_empty() {
        return TRUE;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    windows.push(WindowInfo {
        id: hwnd_to_string(hwnd),
        title,
        class: class_of(hwnd),
        process: process_name(pid),
        pid,
        bounds,
        state: state_of(hwnd),
        foreground: GetForegroundWindow() == hwnd,
    });
    TRUE
}

pub fn list() -> HResult<serde_json::Value> {
    let mut windows: Vec<WindowInfo> = Vec::new();
    unsafe {
        EnumWindows(Some(enum_callback), &mut windows as *mut Vec<WindowInfo> as LPARAM);
    }
    let foreground = unsafe { GetForegroundWindow() };
    windows.sort_by_key(|window| !window.foreground);
    Ok(serde_json::json!({
        "windows": windows,
        "count": windows.len(),
        "foreground": hwnd_to_string(foreground),
    }))
}

pub fn focus(hwnd: HWND) -> HResult<WindowInfo> {
    if unsafe { IsWindow(hwnd) } == 0 {
        return Err(HelperError::new("WINDOW_NOT_FOUND", "the requested window no longer exists"));
    }
    if unsafe { IsIconic(hwnd) } != 0 {
        unsafe { ShowWindow(hwnd, SW_RESTORE) };
    } else {
        unsafe { ShowWindow(hwnd, SW_SHOW) };
    }
    let current = unsafe { GetCurrentThreadId() };
    let previous = unsafe { GetForegroundWindow() };
    let foreground_thread = unsafe { GetWindowThreadProcessId(previous, std::ptr::null_mut()) };
    let target_thread = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
    let attached_foreground = foreground_thread != 0 && foreground_thread != current;
    let attached_target = target_thread != 0 && target_thread != current;
    unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
        if attached_foreground {
            AttachThreadInput(current, foreground_thread, TRUE);
        }
        if attached_target {
            AttachThreadInput(current, target_thread, TRUE);
        }
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        if attached_target {
            AttachThreadInput(current, target_thread, 0);
        }
        if attached_foreground {
            AttachThreadInput(current, foreground_thread, 0);
        }
    }
    sleep_ms(150);
    if unsafe { GetForegroundWindow() } != hwnd {
        return Err(HelperError::new(
            "FOCUS_REFUSED",
            "Windows refused to move the requested window to the foreground; click it once and retry",
        ));
    }
    info(hwnd)
}

pub fn set_state(hwnd: HWND, state: &str) -> HResult<WindowInfo> {
    if unsafe { IsWindow(hwnd) } == 0 {
        return Err(HelperError::new("WINDOW_NOT_FOUND", "the requested window no longer exists"));
    }
    let command = match state.to_ascii_lowercase().as_str() {
        "minimize" | "minimized" => SW_MINIMIZE,
        "maximize" | "maximized" => SW_MAXIMIZE,
        "restore" | "normal" => SW_RESTORE,
        other => return Err(HelperError::new("BAD_ARGS", format!("unknown window state: {other}"))),
    };
    unsafe { ShowWindow(hwnd, command) };
    sleep_ms(80);
    info(hwnd)
}

/// Resolve a model-supplied window id. Kept in this module because the helper
/// treats window handles as the public identity.
pub fn parse(hwnd: &str) -> HResult<HWND> {
    hwnd_from_str(hwnd).map(|handle| handle as HWND).map_err(|message| HelperError::new("BAD_ARGS", message))
}

/// Small helper for code that needs the wide literal without importing util.
#[allow(dead_code)]
pub fn wide_title(value: &str) -> Vec<u16> {
    wide(value)
}


pub fn foreground() -> HResult<Option<WindowInfo>> {
    let handle = unsafe { GetForegroundWindow() };
    if handle.is_null() || unsafe { IsWindow(handle) } == 0 {
        return Ok(None);
    }
    match info(handle) {
        Ok(window) => Ok(Some(window)),
        Err(_) => Ok(None),
    }
}
