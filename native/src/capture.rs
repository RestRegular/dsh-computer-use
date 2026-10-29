//! Screen, monitor, window and region capture with GDI, plus PNG/JPEG encoding.

use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilterType, PngEncoder};
use image::imageops::FilterType as ResizeFilter;
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use serde::Serialize;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDCW, DeleteDC, DeleteObject,
    EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW, SRCCOPY,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetSystemMetrics, IsIconic, CURSORINFO, CURSOR_SHOWING, DI_NORMAL,
    MONITORINFOF_PRIMARY, PW_RENDERFULLCONTENT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::util::{from_wide, last_error, wide};
use crate::window::{self, Rect};
use crate::{HelperError, HResult};

#[derive(Serialize, Clone)]
pub struct Monitor {
    pub index: usize,
    pub id: String,
    pub primary: bool,
    pub bounds: Rect,
    pub work: Rect,
}

#[derive(Serialize, Clone, Copy)]
pub struct CursorInfo {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

struct Bitmap {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

pub fn virtual_screen() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        }
    }
}

unsafe extern "system" fn monitor_callback(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam as *mut Vec<Monitor>);
    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) == 0 {
        return 1;
    }
    let bounds = Rect {
        x: info.monitorInfo.rcMonitor.left,
        y: info.monitorInfo.rcMonitor.top,
        width: info.monitorInfo.rcMonitor.right - info.monitorInfo.rcMonitor.left,
        height: info.monitorInfo.rcMonitor.bottom - info.monitorInfo.rcMonitor.top,
    };
    let work = Rect {
        x: info.monitorInfo.rcWork.left,
        y: info.monitorInfo.rcWork.top,
        width: info.monitorInfo.rcWork.right - info.monitorInfo.rcWork.left,
        height: info.monitorInfo.rcWork.bottom - info.monitorInfo.rcWork.top,
    };
    monitors.push(Monitor {
        index: monitors.len(),
        id: from_wide(&info.szDevice),
        primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
        bounds,
        work,
    });
    1
}

fn monitor_list() -> HResult<Vec<Monitor>> {
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(monitor_callback),
            &mut list as *mut Vec<Monitor> as LPARAM,
        );
    }
    list.sort_by_key(|monitor| (monitor.bounds.x, monitor.bounds.y));
    for (index, monitor) in list.iter_mut().enumerate() {
        monitor.index = index;
    }
    Ok(list)
}

pub fn monitors() -> HResult<serde_json::Value> {
    let list = monitor_list()?;
    let primary = list.iter().find(|monitor| monitor.primary).map(|monitor| monitor.index).unwrap_or(0);
    Ok(serde_json::json!({
        "monitors": list,
        "primary": primary,
        "virtualScreen": virtual_screen(),
    }))
}

fn cursor_info() -> CursorInfo {
    let mut info: CURSORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
    let ok = unsafe { GetCursorInfo(&mut info) };
    CursorInfo {
        x: info.ptScreenPos.x,
        y: info.ptScreenPos.y,
        visible: ok != 0 && (info.flags & CURSOR_SHOWING) != 0,
    }
}

pub fn cursor() -> CursorInfo {
    cursor_info()
}

fn draw_cursor(dc: HDC, origin: Rect, cursor: &CursorInfo) {
    if !cursor.visible {
        return;
    }
    if cursor.x < origin.x || cursor.y < origin.y || cursor.x >= origin.x + origin.width || cursor.y >= origin.y + origin.height {
        return;
    }
    let mut info: CURSORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
    if unsafe { GetCursorInfo(&mut info) } == 0 || info.hCursor.is_null() {
        return;
    }
    unsafe {
        DrawIconEx(
            dc,
            cursor.x - origin.x,
            cursor.y - origin.y,
            info.hCursor,
            0,
            0,
            0,
            std::ptr::null_mut(),
            DI_NORMAL,
        );
    }
}

fn bitmap_from_dc(dc: HDC, bitmap: HBITMAP, width: i32, height: i32) -> Result<Bitmap, String> {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut buffer = vec![0u8; (width as usize) * (height as usize) * 4];
    let rows = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            buffer.as_mut_ptr() as *mut _,
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if rows != height {
        return Err(format!("GetDIBits copied {rows}/{height} rows: {}", last_error()));
    }
    let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for pixel in buffer.chunks_exact(4) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }
    Ok(Bitmap { width: width as u32, height: height as u32, rgb })
}

fn screen_region(rect: Rect, draw_pointer: bool) -> Result<Bitmap, String> {
    if rect.width <= 0 || rect.height <= 0 {
        return Err("capture rectangle is empty".to_string());
    }
    let cursor = cursor_info();
    let driver = wide("DISPLAY");
    let screen = unsafe { CreateDCW(driver.as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null()) };
    if screen.is_null() {
        return Err(format!("CreateDC(DISPLAY) failed: {}", last_error()));
    }
    let memory = unsafe { CreateCompatibleDC(screen) };
    if memory.is_null() {
        unsafe { DeleteDC(screen) };
        return Err(format!("CreateCompatibleDC failed: {}", last_error()));
    }
    let bitmap = unsafe { CreateCompatibleBitmap(screen, rect.width, rect.height) };
    if bitmap.is_null() {
        unsafe {
            DeleteDC(memory);
            DeleteDC(screen);
        }
        return Err(format!("CreateCompatibleBitmap failed: {}", last_error()));
    }
    let previous = unsafe { SelectObject(memory, bitmap) };
    let copied = unsafe {
        BitBlt(
            memory,
            0,
            0,
            rect.width,
            rect.height,
            screen,
            rect.x,
            rect.y,
            SRCCOPY | CAPTUREBLT,
        )
    };
    if copied == 0 {
        unsafe {
            SelectObject(memory, previous);
            DeleteObject(bitmap);
            DeleteDC(memory);
            DeleteDC(screen);
        }
        return Err(format!("BitBlt failed: {}", last_error()));
    }
    if draw_pointer {
        draw_cursor(memory, rect, &cursor);
    }
    unsafe { SelectObject(memory, previous) };
    let result = bitmap_from_dc(memory, bitmap, rect.width, rect.height);
    unsafe {
        DeleteObject(bitmap);
        DeleteDC(memory);
        DeleteDC(screen);
    }
    result
}

fn print_window(hwnd: HWND, rect: Rect, draw_pointer: bool) -> Result<Bitmap, String> {
    if rect.width <= 0 || rect.height <= 0 {
        return Err("window rectangle is empty".to_string());
    }
    let cursor = cursor_info();
    let screen = unsafe { GetDC(std::ptr::null_mut()) };
    if screen.is_null() {
        return Err(format!("GetDC failed: {}", last_error()));
    }
    let memory = unsafe { CreateCompatibleDC(screen) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen, rect.width, rect.height) };
    if memory.is_null() || bitmap.is_null() {
        unsafe {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            if !memory.is_null() {
                DeleteDC(memory);
            }
            ReleaseDC(std::ptr::null_mut(), screen);
        }
        return Err("CreateCompatibleDC/Bitmap failed".to_string());
    }
    let previous = unsafe { SelectObject(memory, bitmap) };
    let ok = unsafe { PrintWindow(hwnd, memory, PW_RENDERFULLCONTENT) };
    if ok == 0 {
        unsafe {
            SelectObject(memory, previous);
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
        return Err(format!("PrintWindow failed: {}", last_error()));
    }
    if draw_pointer {
        draw_cursor(memory, rect, &cursor);
    }
    unsafe { SelectObject(memory, previous) };
    let result = bitmap_from_dc(memory, bitmap, rect.width, rect.height);
    unsafe {
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
    }
    result
}

fn mostly_black(bitmap: &Bitmap) -> bool {
    if bitmap.rgb.is_empty() {
        return true;
    }
    let mut bright = 0usize;
    let mut samples = 0usize;
    for pixel in bitmap.rgb.chunks_exact(3).step_by(4093) {
        samples += 1;
        if pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32 > 30 {
            bright += 1;
        }
    }
    samples > 0 && bright * 200 < samples
}

fn resize_if_needed(bitmap: Bitmap, max_edge: u32) -> Result<Bitmap, String> {
    if max_edge == 0 || bitmap.width.max(bitmap.height) <= max_edge {
        return Ok(bitmap);
    }
    let scale = max_edge as f64 / bitmap.width.max(bitmap.height) as f64;
    let width = (bitmap.width as f64 * scale).round().max(1.0) as u32;
    let height = (bitmap.height as f64 * scale).round().max(1.0) as u32;
    let image = RgbImage::from_raw(bitmap.width, bitmap.height, bitmap.rgb)
        .ok_or_else(|| "could not rebuild the captured image".to_string())?;
    let resized = image::imageops::resize(&image, width, height, ResizeFilter::Lanczos3);
    Ok(Bitmap { width, height, rgb: resized.into_raw() })
}

fn encode(bitmap: Bitmap, format: &str, quality: u8) -> Result<(Vec<u8>, &'static str), String> {
    let image = RgbImage::from_raw(bitmap.width, bitmap.height, bitmap.rgb)
        .ok_or_else(|| "could not rebuild the captured image".to_string())?;
    let (width, height) = (image.width(), image.height());
    let mut bytes = Vec::new();
    if format.eq_ignore_ascii_case("jpeg") || format.eq_ignore_ascii_case("jpg") {
        JpegEncoder::new_with_quality(&mut bytes, quality.clamp(40, 100))
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgb8)
            .map_err(|error| format!("JPEG encode failed: {error}"))?;
        Ok((bytes, "image/jpeg"))
    } else {
        PngEncoder::new_with_quality(&mut bytes, CompressionType::Fast, PngFilterType::Adaptive)
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgb8)
            .map_err(|error| format!("PNG encode failed: {error}"))?;
        Ok((bytes, "image/png"))
    }
}

fn arg_i32(params: &serde_json::Value, name: &str) -> HResult<i32> {
    params
        .get(name)
        .and_then(serde_json::Value::as_i64)
        .map(|value| value as i32)
        .ok_or_else(|| HelperError::new("BAD_ARGS", format!("missing or invalid integer: {name}")))
}

fn rect_from_value(value: &serde_json::Value) -> HResult<Rect> {
    Ok(Rect {
        x: arg_i32(value, "x")?,
        y: arg_i32(value, "y")?,
        width: arg_i32(value, "width")?,
        height: arg_i32(value, "height")?,
    })
}

fn monitor_rect(index: usize) -> HResult<Rect> {
    let list = monitor_list()?;
    list.get(index)
        .map(|monitor| monitor.bounds)
        .ok_or_else(|| HelperError::new("BAD_ARGS", format!("monitor index {index} does not exist")))
}

pub fn screenshot(params: &serde_json::Value) -> HResult<serde_json::Value> {
    let target = params.get("target").and_then(serde_json::Value::as_str).unwrap_or("screen");
    let include_cursor = params
        .get("include_cursor")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let format = params.get("format").and_then(serde_json::Value::as_str).unwrap_or("png");
    let max_edge = params.get("max_edge").and_then(serde_json::Value::as_u64).unwrap_or(1600) as u32;
    let quality = params.get("jpeg_quality").and_then(serde_json::Value::as_u64).unwrap_or(85) as u8;
    let method = params.get("capture_method").and_then(serde_json::Value::as_str).unwrap_or("auto");
    let cursor = cursor_info();

    let (rect, hwnd, window_info) = match target {
        "region" => {
            let region = params
                .get("region")
                .ok_or_else(|| HelperError::new("BAD_ARGS", "target=region requires a region object"))?;
            (rect_from_value(region)?, None, None)
        }
        "window" => {
            let id = params
                .get("window_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| HelperError::new("BAD_ARGS", "target=window requires window_id"))?;
            let hwnd = window::parse(id)?;
            if unsafe { IsIconic(hwnd) } != 0 {
                return Err(HelperError::new("WINDOW_MINIMIZED", "the requested window is minimized; restore it first"));
            }
            let rect = window::rect_of(hwnd)?;
            let info = window::info(hwnd)?;
            (rect, Some(hwnd), Some(info))
        }
        "monitor" => {
            let index = params.get("monitor").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
            (monitor_rect(index)?, None, None)
        }
        _ => {
            if let Some(index) = params.get("monitor").and_then(serde_json::Value::as_u64) {
                (monitor_rect(index as usize)?, None, None)
            } else {
                (virtual_screen(), None, None)
            }
        }
    };

    let (bitmap, used_method) = if let Some(hwnd) = hwnd {
        if method.eq_ignore_ascii_case("gdi") {
            (screen_region(rect, include_cursor).map_err(|message| HelperError::new("CAPTURE_FAILED", message))?, "gdi")
        } else {
            match print_window(hwnd, rect, include_cursor) {
                Ok(bitmap) if !mostly_black(&bitmap) => (bitmap, "printwindow"),
                Ok(_) | Err(_) if method.eq_ignore_ascii_case("printwindow") => {
                    return Err(HelperError::new("CAPTURE_FAILED", "PrintWindow could not capture this window; try capture_method=gdi"))
                }
                _ => (screen_region(rect, include_cursor).map_err(|message| HelperError::new("CAPTURE_FAILED", message))?, "gdi"),
            }
        }
    } else {
        (screen_region(rect, include_cursor).map_err(|message| HelperError::new("CAPTURE_FAILED", message))?, "gdi")
    };

    let captured_width = bitmap.width;
    let captured_height = bitmap.height;
    let resized = resize_if_needed(bitmap, max_edge).map_err(|message| HelperError::new("CAPTURE_FAILED", message))?;
    let image_width = resized.width;
    let image_height = resized.height;
    let (bytes, media_type) = encode(resized, format, quality).map_err(|message| HelperError::new("ENCODE_FAILED", message))?;

    Ok(serde_json::json!({
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "mediaType": media_type,
        "width": image_width,
        "height": image_height,
        "capturedWidth": captured_width,
        "capturedHeight": captured_height,
        "capture": rect,
        "captureMethod": used_method,
        "cursor": cursor,
        "window": window_info,
        "bytes": bytes.len(),
        "at": crate::util::now_ms(),
    }))
}
