//! Consent takeover: a click-through semi-transparent mask over the whole
//! virtual desktop plus low-level mouse/keyboard hooks that discard physical
//! input while the agent is operating.
//!
//! Injected input (`SendInput` sets `LLKHF_INJECTED`/`LLMHF_INJECTED`) always
//! passes through, so the agent keeps working behind the mask. The emergency
//! hotkey is deliberately *not* discarded: its keys are forwarded to the OS so
//! the polling hotkey thread can detect it and end the takeover immediately.
//!
//! The overlay windows and the hooks live on a dedicated thread with a message
//! loop, so a stuck capture can never block the mask and Windows automatically
//! removes the hooks if the helper dies.

use std::collections::HashSet;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, EndPaint,
    FillRect, FrameRect, GetDC, GetStockObject, GetTextExtentPoint32W, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, UpdateWindow, BLACK_BRUSH, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DEFAULT_PITCH,
    DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_NORMAL, FW_SEMIBOLD, HBRUSH, HDC, HFONT,
    HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageW, GetSystemMetrics, GetWindowLongPtrW, PostThreadMessageW, RegisterClassExW,
    SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos,
    ShowWindowAsync, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, LLMHF_INJECTED, LWA_ALPHA, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_PAINT, WM_SYSKEYDOWN, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    GWLP_USERDATA, HWND_TOPMOST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, HHOOK};

use crate::util::{now_ms, sleep_ms, wide};
use crate::{HelperError, HResult};

const WM_TAKEOVER_END: u32 = WM_APP + 1;
const TINT_CLASS: &str = "DshComputerUseTakeoverTint";
const BANNER_CLASS: &str = "DshComputerUseTakeoverBanner";

/// How long `takeover.end` waits for the overlay thread to dismantle itself.
const END_TIMEOUT_MS: u64 = 5000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCELLED: AtomicBool = AtomicBool::new(false);
static INPUT_REQUIRED: AtomicBool = AtomicBool::new(false);
static CAPTURE_EXCLUDED: AtomicBool = AtomicBool::new(false);
static PREVIEW: AtomicBool = AtomicBool::new(false);
static SINCE: AtomicU64 = AtomicU64::new(0);
static THREAD_ID: AtomicU32 = AtomicU32::new(0);
static OVERLAY: AtomicIsize = AtomicIsize::new(0);
static BANNER: AtomicIsize = AtomicIsize::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
static HOOK_KEYS_BLOCKED: AtomicU64 = AtomicU64::new(0);
static HOOK_MOUSE_BLOCKED: AtomicU64 = AtomicU64::new(0);
static DOWN_KEYS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
static CONTROL: Mutex<()> = Mutex::new(());
static THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static END_REASON: Mutex<Option<String>> = Mutex::new(None);
static END_TX: Mutex<Option<mpsc::Sender<(bool, String)>>> = Mutex::new(None);
static EVENT_SINK: OnceLock<fn(&str, Value)> = OnceLock::new();

fn down_keys() -> &'static Mutex<HashSet<u32>> {
    DOWN_KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Route helper-to-host events ("takeover.ended") to the protocol writer.
pub fn set_event_sink(sink: fn(&str, Value)) {
    let _ = EVENT_SINK.set(sink);
}

fn emit(event: &str, data: Value) {
    if let Some(sink) = EVENT_SINK.get() {
        sink(event, data);
    }
}

#[derive(Clone)]
struct TakeoverConfig {
    opacity: u8,
    title: String,
    hint: String,
    banner: bool,
}

struct InitInfo {
    overlay: bool,
    banner: bool,
    capture_excluded: bool,
    monitors: usize,
}

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

pub fn input_required() -> bool {
    INPUT_REQUIRED.load(Ordering::SeqCst)
}

pub fn set_input_required(value: bool) {
    INPUT_REQUIRED.store(value, Ordering::SeqCst);
}

/// True after an involuntary end (emergency hotkey, helper error). Cleared by
/// the next `begin`; injected input is refused while it is set.
pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

pub fn status_value() -> Value {
    let since = SINCE.load(Ordering::SeqCst);
    let reason = lock(&END_REASON).clone();
    json!({
        "active": is_active(),
        "cancelled": is_cancelled(),
        "inputRequired": input_required(),
        "since": if since == 0 { Value::Null } else { json!(since) },
        "endedReason": reason,
        "hotkey": crate::hotkey::current_label(),
        "hooks": {
            "keysBlocked": HOOK_KEYS_BLOCKED.load(Ordering::SeqCst),
            "mouseBlocked": HOOK_MOUSE_BLOCKED.load(Ordering::SeqCst),
        },
    })
}

fn virtual_bounds() -> RECT {
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
        RECT { left, top, right: left + width, bottom: top + height }
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

fn font_face(text: &str) -> &'static str {
    if text.is_ascii() { "Segoe UI" } else { "Microsoft YaHei UI" }
}

fn create_font(height: i32, weight: u32, text: &str) -> HFONT {
    let face = wide(font_face(text));
    unsafe {
        CreateFontW(
            -height,
            0,
            0,
            0,
            weight as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            face.as_ptr(),
        )
    }
}

fn text_extent(dc: HDC, font: HFONT, text: &[u16]) -> i32 {
    if text.is_empty() {
        return 0;
    }
    unsafe {
        let previous = if font.is_null() { ptr::null_mut() } else { SelectObject(dc, font as HGDIOBJ) };
        let mut size = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut size);
        if !previous.is_null() {
            SelectObject(dc, previous);
        }
        size.cx
    }
}

fn register_classes() -> Result<(), String> {
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance.is_null() {
        return Err("GetModuleHandleW failed".to_string());
    }
    let class_name = wide(TINT_CLASS);
    let mut tint: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    tint.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    tint.lpfnWndProc = Some(tint_proc);
    tint.hInstance = instance;
    tint.hbrBackground = unsafe { GetStockObject(BLACK_BRUSH) } as HBRUSH;
    tint.lpszClassName = class_name.as_ptr();
    if unsafe { RegisterClassExW(&tint) } == 0 {
        let error = std::io::Error::last_os_error();
        // Error 1410 is ERROR_CLASS_ALREADY_EXISTS: another takeover already
        // registered the class in this process, which is what we want.
        if error.raw_os_error() != Some(1410) {
            return Err(format!("RegisterClassExW(tint) failed: {error}"));
        }
    }

    let banner_name = wide(BANNER_CLASS);
    let mut banner: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    banner.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    banner.lpfnWndProc = Some(banner_proc);
    banner.hInstance = instance;
    banner.hbrBackground = unsafe { GetStockObject(BLACK_BRUSH) } as HBRUSH;
    banner.lpszClassName = banner_name.as_ptr();
    if unsafe { RegisterClassExW(&banner) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(1410) {
            return Err(format!("RegisterClassExW(banner) failed: {error}"));
        }
    }
    Ok(())
}

struct BannerData {
    title: Vec<u16>,
    hint: Vec<u16>,
    title_font: HFONT,
    hint_font: HFONT,
    background: HBRUSH,
    accent: HBRUSH,
    dot: HBRUSH,
    title_color: COLORREF,
    hint_color: COLORREF,
}

unsafe fn free_banner_data(pointer: *mut BannerData) {
    if pointer.is_null() {
        return;
    }
    let data = Box::from_raw(pointer);
    for handle in [
        data.title_font as HGDIOBJ,
        data.hint_font as HGDIOBJ,
        data.background as HGDIOBJ,
        data.accent as HGDIOBJ,
        data.dot as HGDIOBJ,
    ] {
        if !handle.is_null() {
            DeleteObject(handle);
        }
    }
}

fn banner_dimensions(title: &[u16], hint: &[u16], title_font: HFONT, hint_font: HFONT) -> (i32, i32) {
    let dc = unsafe { GetDC(ptr::null_mut()) };
    if dc.is_null() {
        return (520, 96);
    }
    let title_width = text_extent(dc, title_font, title);
    let hint_width = text_extent(dc, hint_font, hint);
    unsafe { ReleaseDC(ptr::null_mut(), dc) };
    let text_width = title_width.max(hint_width);
    let width = (text_width + 92).clamp(420, 980);
    (width, 96)
}

unsafe fn create_banner(config: &TakeoverConfig) -> Result<(HWND, bool), String> {
    let class_name = wide(BANNER_CLASS);
    let instance = GetModuleHandleW(ptr::null());
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
        class_name.as_ptr(),
        wide("").as_ptr(),
        WS_POPUP,
        0,
        0,
        10,
        10,
        ptr::null_mut(),
        ptr::null_mut(),
        instance,
        ptr::null(),
    );
    if hwnd.is_null() {
        return Err(format!("could not create the takeover banner: {}", std::io::Error::last_os_error()));
    }

    let title_font = create_font(20, FW_SEMIBOLD as u32, &config.title);
    let hint_font = create_font(13, FW_NORMAL as u32, &config.hint);
    let background = CreateSolidBrush(rgb(12, 18, 30));
    let accent = CreateSolidBrush(rgb(239, 68, 68));
    let dot = CreateSolidBrush(rgb(248, 113, 113));
    let title = config.title.encode_utf16().collect::<Vec<u16>>();
    let hint = config.hint.encode_utf16().collect::<Vec<u16>>();
    let data = Box::new(BannerData {
        title: title.clone(),
        hint: hint.clone(),
        title_font,
        hint_font,
        background,
        accent,
        dot,
        title_color: rgb(248, 250, 252),
        hint_color: rgb(203, 213, 225),
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(data) as isize);

    let (width, height) = banner_dimensions(&title, &hint, title_font, hint_font);
    let screen_width = GetSystemMetrics(SM_CXSCREEN);
    let x = (screen_width - width) / 2;
    SetWindowPos(hwnd, HWND_TOPMOST, x, 28, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    SetLayeredWindowAttributes(hwnd, 0, 238, LWA_ALPHA);
    let display_affinity = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) != 0;
    ShowWindowAsync(hwnd, SW_SHOWNOACTIVATE);
    UpdateWindow(hwnd);
    Ok((hwnd, display_affinity))
}

unsafe extern "system" fn tint_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetClientRect(hwnd, &mut client);
            let black = GetStockObject(BLACK_BRUSH) as HBRUSH;
            FillRect(dc, &client, black);
            EndPaint(hwnd, &paint);
            0
        }
        WM_DISPLAYCHANGE => {
            let bounds = virtual_bounds();
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe extern "system" fn banner_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_PAINT => {
            let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BannerData;
            if pointer.is_null() {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            let data = &*pointer;
            let mut paint: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            GetClientRect(hwnd, &mut client);
            FillRect(dc, &client, data.background);
            let bar = RECT { left: 0, top: 0, right: client.right, bottom: 4 };
            FillRect(dc, &bar, data.accent);
            FrameRect(dc, &client, data.accent);

            let center = (client.top + client.bottom) / 2;
            let previous = SelectObject(dc, data.dot as HGDIOBJ);
            Ellipse(dc, 22, center - 5, 32, center + 5);
            SelectObject(dc, previous);

            SetBkMode(dc, TRANSPARENT as i32);
            if !data.title_font.is_null() {
                SelectObject(dc, data.title_font as HGDIOBJ);
            }
            SetTextColor(dc, data.title_color);
            let mut title_rect = RECT { left: 46, top: 14, right: client.right - 18, bottom: 48 };
            DrawTextW(
                dc,
                data.title.as_ptr(),
                data.title.len() as i32,
                &mut title_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
            );
            if !data.hint_font.is_null() {
                SelectObject(dc, data.hint_font as HGDIOBJ);
            }
            SetTextColor(dc, data.hint_color);
            let mut hint_rect = RECT { left: 46, top: 48, right: client.right - 18, bottom: 80 };
            DrawTextW(
                dc,
                data.hint.as_ptr(),
                data.hint.len() as i32,
                &mut hint_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
            );
            EndPaint(hwnd, &paint);
            0
        }
        WM_DESTROY => {
            let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BannerData;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            free_banner_data(pointer);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(ptr::null_mut(), code, wparam, lparam);
    }
    let info = &*(lparam as *const KBDLLHOOKSTRUCT);
    if !ACTIVE.load(Ordering::SeqCst) || (info.flags & LLKHF_INJECTED) != 0 {
        return CallNextHookEx(ptr::null_mut(), code, wparam, lparam);
    }
    // Never discard the emergency hotkey: forward every key that belongs to
    // the combination so the polling hotkey thread still sees the press.
    if crate::hotkey::codes().contains(&(info.vkCode as i32)) {
        if wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN {
            lock(down_keys()).insert(info.vkCode);
        } else {
            lock(down_keys()).remove(&info.vkCode);
        }
        return CallNextHookEx(ptr::null_mut(), code, wparam, lparam);
    }
    HOOK_KEYS_BLOCKED.fetch_add(1, Ordering::SeqCst);
    1
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(ptr::null_mut(), code, wparam, lparam);
    }
    let info = &*(lparam as *const MSLLHOOKSTRUCT);
    if ACTIVE.load(Ordering::SeqCst) && (info.flags & LLMHF_INJECTED) == 0 {
        HOOK_MOUSE_BLOCKED.fetch_add(1, Ordering::SeqCst);
        return 1;
    }
    CallNextHookEx(ptr::null_mut(), code, wparam, lparam)
}

fn install_hooks(instance: windows_sys::Win32::Foundation::HINSTANCE) -> Result<(), String> {
    // Previous hooks may have leaked if a cleanup was interrupted.
    uninstall_hooks();
    unsafe {
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), instance, 0);
        if keyboard.is_null() {
            return Err(format!("SetWindowsHookExW(WH_KEYBOARD_LL) failed: {}", std::io::Error::last_os_error()));
        }
        KEYBOARD_HOOK.store(keyboard as isize, Ordering::SeqCst);
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), instance, 0);
        if mouse.is_null() {
            let error = std::io::Error::last_os_error();
            uninstall_hooks();
            return Err(format!("SetWindowsHookExW(WH_MOUSE_LL) failed: {error}"));
        }
        MOUSE_HOOK.store(mouse as isize, Ordering::SeqCst);
    }
    Ok(())
}

fn uninstall_hooks() {
    let keyboard = KEYBOARD_HOOK.swap(0, Ordering::SeqCst);
    if keyboard != 0 {
        unsafe { UnhookWindowsHookEx(keyboard as HHOOK) };
    }
    let mouse = MOUSE_HOOK.swap(0, Ordering::SeqCst);
    if mouse != 0 {
        unsafe { UnhookWindowsHookEx(mouse as HHOOK) };
    }
}

struct Session {
    tint: HWND,
    banner: HWND,
    capture_excluded: bool,
}

impl Session {
    fn create(config: TakeoverConfig) -> Result<Session, String> {
        register_classes()?;
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        if instance.is_null() {
            return Err("GetModuleHandleW failed".to_string());
        }
        let bounds = virtual_bounds();
        let class_name = wide(TINT_CLASS);
        let tint = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                class_name.as_ptr(),
                wide("").as_ptr(),
                WS_POPUP,
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if tint.is_null() {
            return Err(format!("could not create the takeover overlay: {}", std::io::Error::last_os_error()));
        }
        unsafe {
            SetLayeredWindowAttributes(tint, 0, config.opacity, LWA_ALPHA);
            let mut capture_excluded = SetWindowDisplayAffinity(tint, WDA_EXCLUDEFROMCAPTURE) != 0;
            ShowWindowAsync(tint, SW_SHOWNOACTIVATE);
            UpdateWindow(tint);
            if config.banner {
                match create_banner(&config) {
                    Ok((banner, banner_excluded)) => {
                        capture_excluded = capture_excluded && banner_excluded;
                        if install_hooks(instance).is_err() {
                            DestroyWindow(banner);
                            DestroyWindow(tint);
                            return Err("could not install the input hooks".to_string());
                        }
                        return Ok(Session {
                            tint,
                            banner,
                            capture_excluded,
                        });
                    }
                    Err(error) => {
                        // The tint and hooks are the safety-critical part; a
                        // missing banner must not leave the desktop usable.
                        let _ = error;
                    }
                }
            }
            if let Err(error) = install_hooks(instance) {
                DestroyWindow(tint);
                return Err(error);
            }
            Ok(Session { tint, banner: ptr::null_mut(), capture_excluded })
        }
    }

    fn destroy(mut self) {
        uninstall_hooks();
        unsafe {
            if !self.banner.is_null() {
                DestroyWindow(self.banner);
                self.banner = ptr::null_mut();
            }
            if !self.tint.is_null() {
                DestroyWindow(self.tint);
                self.tint = ptr::null_mut();
            }
        }
    }
}

fn takeover_thread(config: TakeoverConfig, init: mpsc::Sender<Result<InitInfo, String>>) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    down_keys().lock().unwrap_or_else(|error| error.into_inner()).clear();
    HOOK_KEYS_BLOCKED.store(0, Ordering::SeqCst);
    HOOK_MOUSE_BLOCKED.store(0, Ordering::SeqCst);

    let session = match Session::create(config) {
        Ok(session) => {
            OVERLAY.store(session.tint as isize, Ordering::SeqCst);
            BANNER.store(session.banner as isize, Ordering::SeqCst);
            CAPTURE_EXCLUDED.store(session.capture_excluded, Ordering::SeqCst);
            ACTIVE.store(true, Ordering::SeqCst);
            CANCELLED.store(false, Ordering::SeqCst);
            SINCE.store(now_ms(), Ordering::SeqCst);
            let monitors = crate::capture::monitor_count().unwrap_or(0);
            let info = InitInfo {
                overlay: true,
                banner: !session.banner.is_null(),
                capture_excluded: session.capture_excluded,
                monitors,
            };
            if init.send(Ok(info)).is_err() {
                session.destroy();
                reset_generation(generation);
                return;
            }
            session
        }
        Err(message) => {
            reset_generation(generation);
            let _ = init.send(Err(message));
            return;
        }
    };

    message_loop();

    let reason = lock(&END_REASON).take().unwrap_or_else(|| "host".to_string());
    ACTIVE.store(false, Ordering::SeqCst);
    session.destroy();
    reset_generation(generation);
    let since = SINCE.load(Ordering::SeqCst);
    if reason == "hotkey" || reason == "error" || reason == "debug" {
        emit(
            "takeover.ended",
            json!({ "reason": reason, "since": since, "at": now_ms(), "hotkey": crate::hotkey::current_label() }),
        );
    }
    if let Some(tx) = lock(&END_TX).take() {
        let _ = tx.send((true, reason));
    }
}

fn message_loop() {
    loop {
        let mut message: MSG = unsafe { std::mem::zeroed() };
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        if message.message == WM_TAKEOVER_END {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn reset_generation(generation: u64) {
    if GENERATION.load(Ordering::SeqCst) == generation {
        OVERLAY.store(0, Ordering::SeqCst);
        BANNER.store(0, Ordering::SeqCst);
        CAPTURE_EXCLUDED.store(false, Ordering::SeqCst);
        PREVIEW.store(false, Ordering::SeqCst);
        THREAD_ID.store(0, Ordering::SeqCst);
    }
}

fn parse_opacity(value: Option<&Value>) -> u8 {
    let opacity = value.and_then(Value::as_f64).unwrap_or(0.18).clamp(0.05, 0.9);
    (opacity * 255.0).round() as u8
}

/// Start the mask and the input hooks. Called by the host only after the human
/// approved the takeover for this turn.
pub fn begin(params: &Value) -> HResult<Value> {
    let _guard = lock(&CONTROL);
    if ACTIVE.load(Ordering::SeqCst) {
        return Ok(json!({ "active": true, "already": true, "status": status_value() }));
    }
    if crate::hotkey::is_paused() {
        return Err(HelperError::new(
            "PAUSED",
            "computer use is paused by the emergency hotkey; ask the human to press it again before taking over",
        ));
    }
    let config = TakeoverConfig {
        opacity: parse_opacity(params.get("opacity")),
        title: params
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("AI is operating this computer")
            .to_string(),
        hint: params
            .get("hint")
            .and_then(Value::as_str)
            .unwrap_or("Press Ctrl+Alt+Shift+F12 to take back control")
            .to_string(),
        banner: params.get("banner").and_then(Value::as_bool).unwrap_or(true),
    };

    // Join a previous overlay thread that has already ended (for example after
    // the hotkey) before touching the globals it owns.
    if !ACTIVE.load(Ordering::SeqCst) {
        if let Some(handle) = lock(&THREAD).take() {
            let _ = handle.join();
        }
    }

    let (tx, rx) = mpsc::channel::<Result<InitInfo, String>>();
    let handle = thread::Builder::new()
        .name("dsh-computer-use-takeover".to_string())
        .spawn(move || takeover_thread(config, tx))
        .map_err(|error| HelperError::new("TAKEOVER_FAILED", format!("could not start the takeover thread: {error}")))?;
    *lock(&THREAD) = Some(handle);

    let info = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(info)) => info,
        Ok(Err(message)) => {
            if let Some(handle) = lock(&THREAD).take() {
                let _ = handle.join();
            }
            return Err(HelperError::new("TAKEOVER_FAILED", message));
        }
        Err(_) => {
            return Err(HelperError::new("TAKEOVER_FAILED", "the takeover overlay did not start in time"));
        }
    };
    // A human may still be holding a modifier or a mouse button from the click
    // that approved the takeover. Release that physical state now (the key-up
    // events themselves are swallowed by the hooks) so the agent's injected
    // input cannot be altered by a stuck Ctrl/Shift/Alt/Win or button.
    let _ = crate::input::release_all();
    Ok(json!({
        "active": true,
        "since": SINCE.load(Ordering::SeqCst),
        "hotkey": crate::hotkey::current_label(),
        "overlay": info.overlay,
        "banner": info.banner,
        "captureExcluded": info.capture_excluded,
        "monitors": info.monitors,
    }))
}

fn request_end(reason: &str) {
    *lock(&END_REASON) = Some(reason.to_string());
    let thread_id = THREAD_ID.load(Ordering::SeqCst);
    if thread_id != 0 {
        unsafe { PostThreadMessageW(thread_id, WM_TAKEOVER_END, 0, 0) };
    }
}

/// End the takeover from the host and wait until the mask and hooks are gone.
pub fn end(reason: &str) -> HResult<Value> {
    let _guard = lock(&CONTROL);
    if !ACTIVE.load(Ordering::SeqCst) {
        if let Some(handle) = lock(&THREAD).take() {
            let _ = handle.join();
        }
        return Ok(json!({ "active": false, "already": true, "reason": reason }));
    }
    let (tx, rx) = mpsc::channel::<(bool, String)>();
    *lock(&END_TX) = Some(tx);
    request_end(reason);
    // The thread clears ACTIVE just before sending its final frame, and the
    // hotkey path may have already finished before this request arrived. Poll
    // both so an emergency end does not stall a turn for the full timeout.
    let deadline = std::time::Instant::now() + Duration::from_millis(END_TIMEOUT_MS);
    let mut outcome = None;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(value) => {
                outcome = Some(value);
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !ACTIVE.load(Ordering::SeqCst) {
                    break;
                }
            }
        }
    }
    if let Some(handle) = lock(&THREAD).take() {
        let _ = handle.join();
    }
    match outcome {
        Some((true, actual_reason)) => Ok(json!({
            "active": false,
            "reason": actual_reason,
            "requested": reason,
        })),
        Some((false, message)) => Err(HelperError::new("TAKEOVER_FAILED", message)),
        None if ACTIVE.load(Ordering::SeqCst) => {
            Err(HelperError::new("TAKEOVER_TIMEOUT", "the takeover overlay did not stop in time"))
        }
        None => Ok(json!({ "active": false, "reason": reason })),
    }
}

/// End the takeover because the human pressed the emergency hotkey. The helper
/// fails closed: it also pauses input and aborts any in-flight input loop.
pub fn emergency_stop(reason: &str) {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    CANCELLED.store(true, Ordering::SeqCst);
    crate::hotkey::set_paused(true);
    let _ = crate::input::release_all();
    request_end(reason);
}

/// Hide the overlay for a screenshot when display-affinity exclusion is not
/// available on this Windows version. Returns whether anything was hidden.
pub fn hide_for_capture() -> bool {
    if !ACTIVE.load(Ordering::SeqCst)
        || PREVIEW.load(Ordering::SeqCst)
        || CAPTURE_EXCLUDED.load(Ordering::SeqCst)
    {
        return false;
    }
    let tint = OVERLAY.load(Ordering::SeqCst);
    if tint != 0 {
        unsafe { ShowWindowAsync(tint as HWND, SW_HIDE) };
    }
    let banner = BANNER.load(Ordering::SeqCst);
    if banner != 0 {
        unsafe { ShowWindowAsync(banner as HWND, SW_HIDE) };
    }
    sleep_ms(70);
    true
}

pub fn finish_capture(hidden: bool) {
    if !hidden || !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let tint = OVERLAY.load(Ordering::SeqCst);
    if tint != 0 {
        unsafe { ShowWindowAsync(tint as HWND, SW_SHOWNOACTIVATE) };
    }
    let banner = BANNER.load(Ordering::SeqCst);
    if banner != 0 {
        unsafe { ShowWindowAsync(banner as HWND, SW_SHOWNOACTIVATE) };
    }
    sleep_ms(30);
}

/// Stop the overlay on shutdown without surfacing protocol errors.
pub fn shutdown() {
    if ACTIVE.load(Ordering::SeqCst) {
        let _ = end("shutdown");
    } else if let Some(handle) = lock(&THREAD).take() {
        let _ = handle.join();
    }
}

/// Temporarily let screenshots include the mask, so a human can inspect (and
/// document) what the overlay looks like. Only the debug protocol uses this.
pub fn disable_capture_exclusion() {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    PREVIEW.store(true, Ordering::SeqCst);
    let tint = OVERLAY.load(Ordering::SeqCst);
    if tint != 0 {
        unsafe { SetWindowDisplayAffinity(tint as HWND, windows_sys::Win32::UI::WindowsAndMessaging::WDA_NONE) };
    }
    let banner = BANNER.load(Ordering::SeqCst);
    if banner != 0 {
        unsafe { SetWindowDisplayAffinity(banner as HWND, windows_sys::Win32::UI::WindowsAndMessaging::WDA_NONE) };
    }
}

pub fn restore_capture_exclusion() {
    PREVIEW.store(false, Ordering::SeqCst);
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let mut excluded = true;
    for handle in [OVERLAY.load(Ordering::SeqCst), BANNER.load(Ordering::SeqCst)] {
        if handle != 0 && unsafe { SetWindowDisplayAffinity(handle as HWND, WDA_EXCLUDEFROMCAPTURE) } == 0 {
            excluded = false;
        }
    }
    CAPTURE_EXCLUDED.store(excluded, Ordering::SeqCst);
}
