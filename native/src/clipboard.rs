//! Clipboard access through the Win32 clipboard APIs.

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

const CF_UNICODETEXT: u32 = 13;

use crate::util::{last_error, sleep_ms};
use crate::{HelperError, HResult};

fn open() -> HResult<()> {
    for _ in 0..8 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return Ok(());
        }
        sleep_ms(25);
    }
    Err(HelperError::new("CLIPBOARD_BUSY", format!("could not open the clipboard: {}", last_error())))
}

pub fn get() -> HResult<String> {
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT as u32) } == 0 {
        open()?;
        unsafe { CloseClipboard() };
        return Ok(String::new());
    }
    open()?;
    let result = (|| {
        let handle: HANDLE = unsafe { GetClipboardData(CF_UNICODETEXT as u32) };
        if handle.is_null() {
            return Err(HelperError::new("CLIPBOARD_READ_FAILED", format!("GetClipboardData failed: {}", last_error())));
        }
        let pointer = unsafe { GlobalLock(handle) } as *const u16;
        if pointer.is_null() {
            return Err(HelperError::new("CLIPBOARD_READ_FAILED", "GlobalLock returned null"));
        }
        let mut length = 0usize;
        while unsafe { *pointer.add(length) } != 0 && length < 16 * 1024 * 1024 {
            length += 1;
        }
        let units = unsafe { std::slice::from_raw_parts(pointer, length) };
        let text = String::from_utf16_lossy(units);
        unsafe {
            GlobalUnlock(handle);
        }
        Ok(text)
    })();
    unsafe { CloseClipboard() };
    result
}

pub fn set(text: &str) -> HResult<()> {
    open()?;
    let result = (|| {
        unsafe { EmptyClipboard() };
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = units.len() * std::mem::size_of::<u16>();
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if handle.is_null() {
            return Err(HelperError::new("CLIPBOARD_WRITE_FAILED", format!("GlobalAlloc failed: {}", last_error())));
        }
        let pointer = unsafe { GlobalLock(handle) } as *mut u16;
        if pointer.is_null() {
            unsafe { GlobalFree(handle) };
            return Err(HelperError::new("CLIPBOARD_WRITE_FAILED", "GlobalLock returned null"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(units.as_ptr(), pointer, units.len());
            GlobalUnlock(handle);
        }
        if unsafe { SetClipboardData(CF_UNICODETEXT as u32, handle) }.is_null() {
            unsafe { GlobalFree(handle) };
            return Err(HelperError::new("CLIPBOARD_WRITE_FAILED", format!("SetClipboardData failed: {}", last_error())));
        }
        Ok(())
    })();
    unsafe { CloseClipboard() };
    result
}
