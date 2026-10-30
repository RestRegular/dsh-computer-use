//! Interactive chat panel inside the consent takeover mask.
//!
//! The human's physical mouse is discarded by the low-level hooks, so the panel
//! draws its own pointer at the physical position and hit-tests clicks itself.
//! The panel is click-through for the OS (and `SendInput`), so the agent keeps
//! operating the windows behind it. Clicking the message box activates the
//! panel and gives its multiline EDIT control the keyboard focus (that is what
//! makes the system IME work); while that focus is held the helper rejects
//! injected input with `HUMAN_COMPOSING`, and physical keys are forwarded to
//! the EDIT instead of the desktop. Enter sends the message to the host, the
//! "cancel takeover" button fails closed and ends the takeover.
//!
//! Everything here lives on the takeover overlay thread; only the snapshot
//! mutex, ack/debug queues and the window handles are shared with the main
//! thread.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, FrameRect, GetStockObject, HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, Polygon,
    SelectObject, SetBkColor, SetBkMode, SetTextColor, UpdateWindow, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, DT_VCENTER, FW_NORMAL, FW_SEMIBOLD, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
    WHITE_BRUSH,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::Ime::{ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPSTR};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_TAB,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    GetClientRect, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, KillTimer, PostMessageW, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindowAsync, GUITHREADINFO, GWLP_WNDPROC,
    HWND_TOPMOST, LWA_COLORKEY, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SW_HIDE,
    SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WM_APP, WM_CHAR, WM_COMMAND, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_TIMER, WNDCLASSEXW,
    WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ES_AUTOVSCROLL, ES_LEFT, ES_MULTILINE, ES_WANTRETURN, WNDPROC,
};

use crate::takeover;
use crate::util::wide;

const PANEL_CLASS: &str = "DshComputerUseTakeoverChat";
const POINTER_CLASS: &str = "DshComputerUseTakeoverPointer";

const WM_CHAT_CLICK: u32 = WM_APP + 2;
const WM_CHAT_ACK: u32 = WM_APP + 3;
const WM_CHAT_POINTER: u32 = WM_APP + 4;
const WM_CHAT_DEBUG: u32 = WM_APP + 5;

const TIMER_COMPOSE: usize = 1;
const TIMER_FOCUS: usize = 2;
const COMPOSE_WAIT_MS: u64 = 10_000;
const FOCUS_CHECK_MS: u32 = 600;

const EDIT_ID: i32 = 101;
const WM_CTLCOLOREDIT: u32 = 0x0133;
const WM_SETFONT: u32 = 0x0030;
const EM_SETLIMITTEXT: u32 = 0x00C5;
const EM_SETSEL: u32 = 0x00B1;
const EM_SETRECT: u32 = 0x00B3;
const EM_CHARFROMPOS: u32 = 0x00D7;
const EN_CHANGE: u32 = 0x0300;

const PANEL_HEIGHT: i32 = 172;
const PANEL_MIN_WIDTH: i32 = 420;
const PANEL_MAX_WIDTH: i32 = 700;

/// One-shot debug command for the native test harness. The main thread stores
/// it and posts `WM_CHAT_DEBUG`; the overlay thread executes it on its own
/// thread, exactly like a physical interaction would.
#[derive(Clone)]
pub enum DebugAction {
    Compose,
    Send,
    Text(String),
    Pointer(i32, i32),
    Click(i32, i32),
    Keys(String),
    Press(Vec<String>),
}

#[derive(Clone, Default)]
pub struct ChatSnapshot {
    pub visible: bool,
    pub composing: bool,
    pub pending_send: bool,
    pub text_length: usize,
    pub status: String,
}

static SNAPSHOT: Mutex<ChatSnapshot> = Mutex::new(ChatSnapshot {
    visible: false,
    composing: false,
    pending_send: false,
    text_length: 0,
    status: String::new(),
});
static ACK: Mutex<Option<(bool, String)>> = Mutex::new(None);
static DEBUG: Mutex<Option<DebugAction>> = Mutex::new(None);
static PANEL_HWND: AtomicIsize = AtomicIsize::new(0);
static POINTER_HWND: AtomicIsize = AtomicIsize::new(0);
static VISIBLE: AtomicBool = AtomicBool::new(false);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone)]
pub struct ChatConfig {
    pub title: String,
    pub hint: String,
    pub send: String,
    pub cancel: String,
    pub idle: String,
    pub composing: String,
    pub sending: String,
    pub sent: String,
    pub failed: String,
    pub waiting: String,
    pub empty: String,
    pub focus_failed: String,
    pub max_length: usize,
}

fn text(value: &Value, key: &str, fallback: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

impl ChatConfig {
    pub fn from_json(value: &Value) -> Option<Self> {
        if !value.is_object() {
            return None;
        }
        Some(Self {
            title: text(value, "title", "Message the agent"),
            hint: text(value, "hint", "Enter sends · Shift+Enter for a new line · Esc dismisses"),
            send: text(value, "send", "Send"),
            cancel: text(value, "cancel", "Cancel takeover"),
            idle: text(value, "idle", "Click the box to type"),
            composing: text(value, "composing", "Typing · Enter sends"),
            sending: text(value, "sending", "Sending…"),
            sent: text(value, "sent", "Sent"),
            failed: text(value, "failed", "Send failed"),
            waiting: text(value, "waiting", "The agent is working; one moment…"),
            empty: text(value, "empty", "The message is empty"),
            focus_failed: text(value, "focusFailed", "Could not focus the input; click it again"),
            max_length: value
                .get("maxLength")
                .and_then(Value::as_u64)
                .map(|value| value.clamp(1, 200_000) as usize)
                .unwrap_or(20_000),
        })
    }
}

fn color(red: u8, green: u8, blue: u8) -> COLORREF {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

fn face_for(text: &str) -> &'static str {
    if text.is_ascii() { "Segoe UI" } else { "Microsoft YaHei UI" }
}

fn create_font(height: i32, weight: u32, face: &str) -> HFONT {
    let face = wide(face_for(face));
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

struct Fonts {
    title: HFONT,
    small: HFONT,
    edit: HFONT,
    button: HFONT,
}

struct Brushes {
    background: HBRUSH,
    border: HBRUSH,
    input: HBRUSH,
    input_focus: HBRUSH,
    send: HBRUSH,
    send_hover: HBRUSH,
    send_disabled: HBRUSH,
    cancel: HBRUSH,
    cancel_hover: HBRUSH,
    accent: HBRUSH,
}

struct Ui {
    panel: HWND,
    edit: HWND,
    pointer: HWND,
    labels: ChatConfig,
    fonts: Fonts,
    brushes: Brushes,
    status: String,
    composing: bool,
    pending_compose: bool,
    compose_deadline: Option<Instant>,
    pending_send: bool,
    sent_text: Option<String>,
    prev_foreground: HWND,
    prev_focus: HWND,
    hover: i32,
    width: i32,
    height: i32,
    edit_rect: RECT,
    send_rect: RECT,
    cancel_rect: RECT,
}

impl Ui {
    fn hit(&self, local: POINT) -> u8 {
        // 1 = edit, 2 = send, 3 = cancel, 0 = panel body/outside
        if local.y < 0 || local.x < 0 || local.x >= self.width || local.y >= self.height {
            return 0;
        }
        if point_in(local, self.edit_rect) {
            return 1;
        }
        if point_in(local, self.send_rect) {
            return 2;
        }
        if point_in(local, self.cancel_rect) {
            return 3;
        }
        0
    }

    fn set_status(&mut self, status: String) {
        self.status = status;
        refresh_snapshot(self);
        unsafe { InvalidateRect(self.panel, ptr::null(), 0) };
    }
}

fn point_in(point: POINT, rect: RECT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

fn point(x: i32, y: i32) -> POINT {
    POINT { x, y }
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    static LAST_POINTER: Cell<POINT> = const { Cell::new(POINT { x: 0, y: 0 }) };
    static LAST_CLICK: Cell<POINT> = const { Cell::new(POINT { x: 0, y: 0 }) };
    static POINTER_POSTED: Cell<bool> = const { Cell::new(false) };
    static EDIT_PROC: Cell<isize> = const { Cell::new(0) };
}

/// Run `f` with the overlay UI. Re-entrant window messages are ignored instead
/// of panicking: the outer handler already refreshed the state.
fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|slot| match slot.try_borrow_mut() {
        Ok(mut borrow) => borrow.as_mut().map(f),
        Err(_) => None,
    })
}

/// Read-only access that tolerates re-entrancy (used from hook callbacks).
fn peek_ui<R>(f: impl FnOnce(&Ui) -> R) -> Option<R> {
    UI.with(|slot| match slot.try_borrow() {
        Ok(borrow) => borrow.as_ref().map(f),
        Err(_) => None,
    })
}

fn refresh_snapshot(ui: &Ui) {
    let mut snapshot = lock(&SNAPSHOT);
    snapshot.visible = true;
    snapshot.composing = ui.composing;
    snapshot.pending_send = ui.pending_send;
    snapshot.status = ui.status.clone();
    snapshot.text_length = if ui.edit.is_null() {
        0
    } else {
        unsafe { GetWindowTextLengthW(ui.edit).max(0) as usize }
    };
}

fn panel_bounds() -> (i32, i32, i32, i32) {
    unsafe {
        let screen_width = GetSystemMetrics(SM_CXSCREEN).max(320);
        let screen_height = GetSystemMetrics(SM_CYSCREEN).max(240);
        let width = (screen_width - 160)
            .clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH)
            .min(screen_width - 20)
            .max(320);
        let height = PANEL_HEIGHT;
        let x = (screen_width - width) / 2;
        let y = (screen_height - height - 44).max(8);
        (x, y, width, height)
    }
}

fn layout(ui: &mut Ui) {
    let width = ui.width;
    ui.edit_rect = RECT {
        left: 18,
        top: 52,
        right: width - 18,
        bottom: 52 + 56,
    };
    let button_height = 34;
    let button_top = 118;
    let send_width = 92;
    let cancel_width = 112;
    ui.send_rect = RECT {
        left: width - 18 - send_width,
        top: button_top,
        right: width - 18,
        bottom: button_top + button_height,
    };
    ui.cancel_rect = RECT {
        left: ui.send_rect.left - 10 - cancel_width,
        top: button_top,
        right: ui.send_rect.left - 10,
        bottom: button_top + button_height,
    };
}

fn focused_window(window: HWND) -> HWND {
    if window.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        let thread = GetWindowThreadProcessId(window, ptr::null_mut());
        if thread == 0 {
            return ptr::null_mut();
        }
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(thread, &mut info) == 0 {
            return ptr::null_mut();
        }
        info.hwndFocus
    }
}

fn attach_to(other: HWND) -> bool {
    if other.is_null() {
        return false;
    }
    unsafe {
        let other_thread = GetWindowThreadProcessId(other, ptr::null_mut());
        let this_thread = GetCurrentThreadId();
        if other_thread == 0 || other_thread == this_thread {
            return false;
        }
        AttachThreadInput(this_thread, other_thread, 1) != 0
    }
}

fn detach_from(other: HWND) {
    if other.is_null() {
        return;
    }
    unsafe {
        let other_thread = GetWindowThreadProcessId(other, ptr::null_mut());
        let this_thread = GetCurrentThreadId();
        if other_thread != 0 && other_thread != this_thread {
            AttachThreadInput(this_thread, other_thread, 0);
        }
    }
}

fn force_foreground(panel: HWND, edit: HWND) {
    unsafe {
        let previous = GetForegroundWindow();
        let attached = attach_to(previous);
        for _ in 0..3 {
            SetForegroundWindow(panel);
            BringWindowToTop(panel);
            SetFocus(edit);
            if GetForegroundWindow() == panel {
                break;
            }
            crate::util::sleep_ms(30);
        }
        if attached {
            detach_from(previous);
        }
    }
}

fn restore_foreground(ui: &mut Ui) {
    let previous = ui.prev_foreground;
    let focus = ui.prev_focus;
    ui.prev_foreground = ptr::null_mut();
    ui.prev_focus = ptr::null_mut();
    if previous.is_null() || previous == ui.panel || unsafe { IsWindow(previous) } == 0 {
        return;
    }
    unsafe {
        let attached = attach_to(previous);
        SetForegroundWindow(previous);
        if !focus.is_null() && IsWindow(focus) != 0 {
            SetFocus(focus);
        }
        if attached {
            detach_from(previous);
        }
    }
}

fn set_edit_rect(ui: &Ui) {
    if ui.edit.is_null() {
        return;
    }
    unsafe {
        let width = ui.edit_rect.right - ui.edit_rect.left;
        let height = ui.edit_rect.bottom - ui.edit_rect.top;
        let mut inner = RECT {
            left: 8,
            top: 6,
            right: width - 8,
            bottom: height - 6,
        };
        SendMessageW(ui.edit, EM_SETRECT, 0, &mut inner as *mut RECT as isize);
    }
}

fn edit_text(ui: &Ui) -> String {
    if ui.edit.is_null() {
        return String::new();
    }
    unsafe {
        let length = GetWindowTextLengthW(ui.edit).max(0);
        let mut buffer = vec![0u16; length as usize + 1];
        let written = GetWindowTextW(ui.edit, buffer.as_mut_ptr(), buffer.len() as i32);
        if written <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..written as usize])
    }
}

fn caret_to_end(ui: &Ui) {
    if ui.edit.is_null() {
        return;
    }
    unsafe {
        let length = GetWindowTextLengthW(ui.edit).max(0) as isize;
        SendMessageW(ui.edit, EM_SETSEL, length as usize, length);
    }
}

fn emit_composing(active: bool) {
    takeover::emit_ui_event(
        "takeover.composing",
        json!({ "active": active, "at": crate::util::now_ms() }),
    );
}

fn enter_compose(ui: &mut Ui) {
    if ui.composing {
        return;
    }
    ui.pending_compose = false;
    ui.compose_deadline = None;
    unsafe { KillTimer(ui.panel, TIMER_COMPOSE) };
    ui.composing = true;
    ui.prev_foreground = unsafe { GetForegroundWindow() };
    if ui.prev_foreground == ui.panel {
        ui.prev_foreground = ptr::null_mut();
    }
    ui.prev_focus = focused_window(ui.prev_foreground);
    force_foreground(ui.panel, ui.edit);
    if unsafe { GetForegroundWindow() } != ui.panel {
        ui.composing = false;
        takeover::finish_composing();
        ui.set_status(ui.labels.focus_failed.clone());
        return;
    }
    caret_to_end(ui);
    ui.status = ui.labels.composing.clone();
    refresh_snapshot(ui);
    let last = LAST_POINTER.with(|cell| cell.get());
    position_pointer(ui, last);
    unsafe {
        SetTimer(ui.panel, TIMER_FOCUS, FOCUS_CHECK_MS, None);
        InvalidateRect(ui.panel, ptr::null(), 0);
    }
    emit_composing(true);
}

fn leave_compose(ui: &mut Ui, restore: bool) {
    if !ui.composing {
        return;
    }
    ui.composing = false;
    takeover::finish_composing();
    unsafe { KillTimer(ui.panel, TIMER_FOCUS) };
    if restore {
        restore_foreground(ui);
    }
    if !ui.pending_send {
        ui.status = ui.labels.idle.clone();
    }
    refresh_snapshot(ui);
    unsafe { InvalidateRect(ui.panel, ptr::null(), 0) };
    emit_composing(false);
}

fn request_compose(ui: &mut Ui) {
    if ui.composing {
        caret_to_end(ui);
        unsafe { InvalidateRect(ui.panel, ptr::null(), 0) };
        return;
    }
    if takeover::try_begin_composing() {
        enter_compose(ui);
        return;
    }
    ui.pending_compose = true;
    ui.compose_deadline = Some(Instant::now() + Duration::from_millis(COMPOSE_WAIT_MS));
    ui.set_status(ui.labels.waiting.clone());
    unsafe { SetTimer(ui.panel, TIMER_COMPOSE, 120, None) };
}

fn submit(ui: &mut Ui) {
    if ui.pending_send {
        return;
    }
    let message = edit_text(ui).trim().to_string();
    if message.is_empty() {
        ui.set_status(ui.labels.empty.clone());
        return;
    }
    ui.pending_send = true;
    ui.sent_text = Some(message.clone());
    leave_compose(ui, true);
    ui.status = ui.labels.sending.clone();
    refresh_snapshot(ui);
    unsafe { InvalidateRect(ui.panel, ptr::null(), 0) };
    takeover::emit_ui_event(
        "takeover.message",
        json!({ "text": message, "at": crate::util::now_ms() }),
    );
}

fn cancel_takeover() {
    takeover::cancel_from_ui();
}

fn apply_ack(ui: &mut Ui) {
    let ack = lock(&ACK).take();
    let Some((ok, note)) = ack else { return };
    ui.pending_send = false;
    if ok {
        let current = edit_text(ui);
        let unchanged = ui.sent_text.as_deref() == Some(current.trim());
        if unchanged && !ui.edit.is_null() {
            let empty = wide("");
            unsafe { SetWindowTextW(ui.edit, empty.as_ptr()) };
        }
        ui.status = ui.labels.sent.clone();
    } else {
        let note = note.trim();
        ui.status = if note.is_empty() {
            ui.labels.failed.clone()
        } else {
            format!("{}: {}", ui.labels.failed, truncate(note, 120))
        };
    }
    ui.sent_text = None;
    refresh_snapshot(ui);
    unsafe { InvalidateRect(ui.panel, ptr::null(), 0) };
}

fn truncate(text: &str, limit: usize) -> String {
    let mut result: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        result.push('…');
    }
    result
}

fn update_hover(ui: &mut Ui, point: POINT) {
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(ui.panel, &mut rect) == 0 {
            return;
        }
        let local = POINT {
            x: point.x - rect.left,
            y: point.y - rect.top,
        };
        let hover = match ui.hit(local) {
            2 => 2,
            3 => 3,
            _ => 0,
        };
        if ui.hover != hover {
            ui.hover = hover;
            InvalidateRect(ui.panel, ptr::null(), 0);
        }
    }
}

fn handle_click(ui: &mut Ui, point: POINT) {
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(ui.panel, &mut rect) == 0 {
            return;
        }
        let local = POINT {
            x: point.x - rect.left,
            y: point.y - rect.top,
        };
        let hit = ui.hit(local);
        if ui.pending_compose {
            if hit == 3 {
                cancel_takeover();
                return;
            }
            ui.pending_compose = false;
            ui.compose_deadline = None;
            KillTimer(ui.panel, TIMER_COMPOSE);
        }
        if ui.composing {
            match hit {
                1 => {
                    // Position the caret the way a real click inside the EDIT
                    // would: map the panel point into the EDIT's client area.
                    let edit_local = POINT {
                        x: local.x - ui.edit_rect.left,
                        y: local.y - ui.edit_rect.top,
                    };
                    let index = SendMessageW(
                        ui.edit,
                        EM_CHARFROMPOS,
                        0,
                        ((edit_local.y as isize) << 16) | (edit_local.x as isize & 0xFFFF),
                    ) as isize;
                    SendMessageW(ui.edit, EM_SETSEL, index as usize, index);
                    SetFocus(ui.edit);
                    InvalidateRect(ui.panel, ptr::null(), 0);
                }
                2 => submit(ui),
                3 => cancel_takeover(),
                _ => leave_compose(ui, true),
            }
        } else {
            match hit {
                1 => request_compose(ui),
                2 => {
                    if edit_text(ui).trim().is_empty() {
                        request_compose(ui);
                    } else {
                        submit(ui);
                    }
                }
                3 => cancel_takeover(),
                _ => {}
            }
        }
    }
}

unsafe fn draw_text(dc: HDC, font: HFONT, text_color: COLORREF, value: &str, rect: &RECT, flags: u32) {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.is_empty() {
        return;
    }
    SelectObject(dc, font as HGDIOBJ);
    SetTextColor(dc, text_color);
    let mut target = *rect;
    DrawTextW(dc, units.as_ptr(), units.len() as i32, &mut target, flags);
}

unsafe fn paint_panel(ui: &Ui, hwnd: HWND) {
    let mut paint: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut paint);
    let mut client: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut client);
    FillRect(dc, &client, ui.brushes.background);
    FrameRect(dc, &client, ui.brushes.border);
    let accent = RECT {
        left: 0,
        top: 0,
        right: client.right,
        bottom: 3,
    };
    FillRect(dc, &accent, ui.brushes.accent);

    SetBkMode(dc, TRANSPARENT as i32);
    let title_rect = RECT {
        left: 18,
        top: 14,
        right: client.right - 18,
        bottom: 40,
    };
    draw_text(
        dc,
        ui.fonts.title,
        color(248, 250, 252),
        &ui.labels.title,
        &title_rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    let hint_rect = RECT {
        left: client.right / 2,
        top: 16,
        right: client.right - 18,
        bottom: 38,
    };
    draw_text(
        dc,
        ui.fonts.small,
        color(148, 163, 184),
        &ui.labels.hint,
        &hint_rect,
        DT_RIGHT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );

    let input_border = RECT {
        left: ui.edit_rect.left - 1,
        top: ui.edit_rect.top - 1,
        right: ui.edit_rect.right + 1,
        bottom: ui.edit_rect.bottom + 1,
    };
    let border_brush = if ui.composing {
        ui.brushes.input_focus
    } else {
        ui.brushes.border
    };
    FrameRect(dc, &input_border, border_brush);

    let status = if ui.pending_compose {
        ui.labels.waiting.clone()
    } else {
        ui.status.clone()
    };
    let status_rect = RECT {
        left: 18,
        top: 122,
        right: ui.cancel_rect.left - 12,
        bottom: 146,
    };
    draw_text(
        dc,
        ui.fonts.small,
        color(203, 213, 225),
        &status,
        &status_rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
    );

    let has_text = !edit_text(ui).trim().is_empty();
    let send_brush = if !has_text && !ui.pending_send {
        ui.brushes.send_disabled
    } else if ui.hover == 2 {
        ui.brushes.send_hover
    } else {
        ui.brushes.send
    };
    FillRect(dc, &ui.send_rect, send_brush);
    draw_text(
        dc,
        ui.fonts.button,
        color(248, 250, 252),
        &ui.labels.send,
        &ui.send_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    let cancel_brush = if ui.hover == 3 {
        ui.brushes.cancel_hover
    } else {
        ui.brushes.cancel
    };
    FillRect(dc, &ui.cancel_rect, cancel_brush);
    draw_text(
        dc,
        ui.fonts.button,
        color(252, 165, 165),
        &ui.labels.cancel,
        &ui.cancel_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );

    EndPaint(hwnd, &paint);
}

fn paint_pointer(hwnd: HWND) {
    let mut paint: PAINTSTRUCT = unsafe { std::mem::zeroed() };
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    let mut client: RECT = unsafe { std::mem::zeroed() };
    unsafe {
        GetClientRect(hwnd, &mut client);
        let key = CreateSolidBrush(color(255, 0, 255));
        FillRect(dc, &client, key);
        DeleteObject(key as HGDIOBJ);

        let outline = CreatePen(PS_SOLID, 1, color(15, 23, 42));
        let fill = GetStockObject(WHITE_BRUSH) as HBRUSH;
        let previous_pen = SelectObject(dc, outline as HGDIOBJ);
        let previous_brush = SelectObject(dc, fill as HGDIOBJ);
        let points = [
            point(1, 1),
            point(1, 20),
            point(6, 15),
            point(10, 24),
            point(13, 23),
            point(9, 14),
            point(16, 14),
        ];
        Polygon(dc, points.as_ptr(), points.len() as i32);
        SelectObject(dc, previous_brush);
        SelectObject(dc, previous_pen);
        DeleteObject(outline as HGDIOBJ);
        EndPaint(hwnd, &paint);
    }
}

unsafe extern "system" fn pointer_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_PAINT => {
            paint_pointer(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// True while the focused EDIT has an IME composition in progress. Enter/Esc
/// belong to the IME then, not to the message box (otherwise a Chinese user
/// would send the message while confirming a candidate).
fn ime_composing(hwnd: HWND) -> bool {
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_null() {
            return false;
        }
        let length = ImmGetCompositionStringW(context, GCS_COMPSTR, ptr::null_mut(), 0);
        ImmReleaseContext(hwnd, context);
        length > 0
    }
}

unsafe extern "system" fn edit_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let previous = EDIT_PROC.with(|cell| cell.get());
    match message {
        WM_KEYDOWN if ime_composing(hwnd) => {}
        WM_KEYDOWN => {
            let key = wparam as u32;
            if key == VK_RETURN as u32 {
                let shift = (GetKeyState(VK_SHIFT as i32) as u16 & 0x8000) != 0;
                if !shift {
                    with_ui(submit);
                    return 0;
                }
            } else if key == VK_ESCAPE as u32 {
                with_ui(|ui| leave_compose(ui, true));
                return 0;
            } else if key == VK_TAB as u32 {
                return 0;
            }
        }
        WM_CHAR if wparam as u32 == 13 => return 0,
        _ => {}
    }
    if previous != 0 {
        let proc: WNDPROC = std::mem::transmute(previous);
        CallWindowProcW(proc, hwnd, message, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

unsafe extern "system" fn panel_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_PAINT => {
            with_ui(|ui| paint_panel(ui, hwnd));
            0
        }
        WM_ERASEBKGND => 1,
        WM_CTLCOLOREDIT => {
            let brush = peek_ui(|ui| ui.brushes.input);
            let Some(brush) = brush else {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            };
            let dc = wparam as HDC;
            SetBkColor(dc, color(15, 23, 42));
            SetTextColor(dc, color(226, 232, 240));
            brush as LRESULT
        }
        WM_SETFOCUS => {
            with_ui(|ui| {
                if ui.composing {
                    SetFocus(ui.edit);
                }
            });
            0
        }
        WM_CHAT_POINTER => {
            POINTER_POSTED.with(|flag| flag.set(false));
            let location = LAST_POINTER.with(|cell| cell.get());
            with_ui(|ui| {
                update_hover(ui, location);
                position_pointer(ui, location);
            });
            0
        }
        WM_CHAT_CLICK => {
            let location = LAST_CLICK.with(|cell| cell.get());
            with_ui(|ui| handle_click(ui, location));
            0
        }
        WM_CHAT_ACK => {
            with_ui(apply_ack);
            0
        }
        WM_CHAT_DEBUG => {
            let action = lock(&DEBUG).take();
            if let Some(action) = action {
                match action {
                    DebugAction::Compose => {
                        with_ui(request_compose);
                    }
                    DebugAction::Send => {
                        with_ui(submit);
                    }
                    DebugAction::Text(text) => {
                        let value = wide(&text);
                        with_ui(|ui| {
                            if !ui.edit.is_null() {
                                SetWindowTextW(ui.edit, value.as_ptr());
                                refresh_snapshot(ui);
                                InvalidateRect(ui.panel, ptr::null(), 0);
                            }
                        });
                    }
                    DebugAction::Pointer(x, y) => {
                        remember_pointer(point(x, y));
                        post_pointer();
                    }
                    DebugAction::Click(x, y) => {
                        let location = point(x, y);
                        remember_pointer(location);
                        LAST_CLICK.with(|cell| cell.set(location));
                        with_ui(|ui| handle_click(ui, location));
                    }
                    DebugAction::Keys(keys) => {
                        // Test-only: inject physical keyboard events into the
                        // focused panel (the input guard is bypassed on purpose
                        // so an IME composition can be verified end to end).
                        let list: Vec<String> = keys.chars().map(|key| key.to_string()).collect();
                        let _ = crate::input::press(&list);
                    }
                    DebugAction::Press(keys) => {
                        let _ = crate::input::press(&keys);
                    }
                }
            }
            0
        }
        WM_TIMER => {
            with_ui(|ui| {
                if wparam == TIMER_COMPOSE && ui.pending_compose {
                    let expired = ui
                        .compose_deadline
                        .map(|deadline| Instant::now() > deadline)
                        .unwrap_or(true);
                    if expired {
                        ui.pending_compose = false;
                        ui.compose_deadline = None;
                        KillTimer(ui.panel, TIMER_COMPOSE);
                        ui.set_status(ui.labels.waiting.clone());
                    } else if takeover::try_begin_composing() {
                        enter_compose(ui);
                    }
                } else if wparam == TIMER_FOCUS && ui.composing {
                    if GetForegroundWindow() != ui.panel {
                        leave_compose(ui, false);
                    }
                }
            });
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as i32;
            let code = ((wparam >> 16) & 0xFFFF) as u32;
            if id == EDIT_ID && code == EN_CHANGE {
                with_ui(|ui| {
                    refresh_snapshot(ui);
                    InvalidateRect(ui.panel, ptr::null(), 0);
                });
            }
            0
        }
        WM_DISPLAYCHANGE | WM_SIZE => {
            with_ui(|ui| {
                let (x, y, width, height) = panel_bounds();
                ui.width = width;
                ui.height = height;
                layout(ui);
                SetWindowPos(ui.panel, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE);
                if !ui.edit.is_null() {
                    SetWindowPos(
                        ui.edit,
                        ptr::null_mut(),
                        ui.edit_rect.left,
                        ui.edit_rect.top,
                        ui.edit_rect.right - ui.edit_rect.left,
                        ui.edit_rect.bottom - ui.edit_rect.top,
                        SWP_NOACTIVATE,
                    );
                    set_edit_rect(ui);
                }
                InvalidateRect(ui.panel, ptr::null(), 0);
            });
            0
        }
        WM_DESTROY => 0,
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn position_pointer(ui: &Ui, location: POINT) {
    if ui.pointer.is_null() {
        return;
    }
    unsafe {
        SetWindowPos(
            ui.pointer,
            HWND_TOPMOST,
            location.x - 1,
            location.y - 1,
            20,
            28,
            SWP_NOACTIVATE,
        );
    }
}

fn register_classes(instance: windows_sys::Win32::Foundation::HINSTANCE) -> Result<(), String> {
    unsafe {
        let mut panel: WNDCLASSEXW = std::mem::zeroed();
        panel.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        panel.lpfnWndProc = Some(panel_proc);
        panel.hInstance = instance;
        let panel_name = wide(PANEL_CLASS);
        panel.lpszClassName = panel_name.as_ptr();
        if RegisterClassExW(&panel) == 0 && std::io::Error::last_os_error().raw_os_error() != Some(1410) {
            return Err(format!(
                "RegisterClassExW(chat) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut pointer: WNDCLASSEXW = std::mem::zeroed();
        pointer.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        pointer.lpfnWndProc = Some(pointer_proc);
        pointer.hInstance = instance;
        let pointer_name = wide(POINTER_CLASS);
        pointer.lpszClassName = pointer_name.as_ptr();
        if RegisterClassExW(&pointer) == 0 && std::io::Error::last_os_error().raw_os_error() != Some(1410) {
            return Err(format!(
                "RegisterClassExW(pointer) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

pub struct ChatSession {
    pub capture_excluded: bool,
}

pub fn create(config: &ChatConfig) -> Result<ChatSession, String> {
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance.is_null() {
        return Err("GetModuleHandleW failed".to_string());
    }
    register_classes(instance)?;
    destroy_if_present();

    let labels = config.clone();
    let (x, y, width, height) = panel_bounds();
    let panel = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_TRANSPARENT,
            wide(PANEL_CLASS).as_ptr(),
            wide("").as_ptr(),
            WS_POPUP,
            x,
            y,
            width,
            height,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if panel.is_null() {
        return Err(format!(
            "could not create the takeover chat panel: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut ui = match build_ui(panel, labels, width, height) {
        Ok(ui) => ui,
        Err(message) => {
            unsafe { DestroyWindow(panel) };
            return Err(message);
        }
    };
    layout(&mut ui);
    if let Err(message) = install_edit(&mut ui, instance, panel) {
        unsafe { DestroyWindow(panel) };
        free_resources(&mut ui);
        return Err(message);
    }
    set_edit_rect(&ui);
    let pointer = create_pointer(instance, x + width / 2, y + height / 2);
    ui.pointer = pointer;
    if pointer.is_null() {
        unsafe { DestroyWindow(panel) };
        free_resources(&mut ui);
        return Err(format!(
            "could not create the takeover pointer: {}",
            std::io::Error::last_os_error()
        ));
    }

    let panel_excluded = unsafe { SetWindowDisplayAffinity(panel, WDA_EXCLUDEFROMCAPTURE) } != 0;
    let pointer_excluded = unsafe { SetWindowDisplayAffinity(pointer, WDA_EXCLUDEFROMCAPTURE) } != 0;

    refresh_snapshot(&ui);
    PANEL_HWND.store(panel as isize, Ordering::SeqCst);
    POINTER_HWND.store(pointer as isize, Ordering::SeqCst);
    VISIBLE.store(true, Ordering::SeqCst);

    let mut cursor = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut cursor) };
    position_pointer(&ui, cursor);
    remember_pointer(cursor);

    UI.with(|slot| *slot.borrow_mut() = Some(ui));
    unsafe {
        ShowWindowAsync(panel, SW_SHOWNOACTIVATE);
        ShowWindowAsync(pointer, SW_SHOWNOACTIVATE);
        UpdateWindow(panel);
        UpdateWindow(pointer);
    }

    Ok(ChatSession {
        capture_excluded: panel_excluded && pointer_excluded,
    })
}

fn install_edit(ui: &mut Ui, instance: windows_sys::Win32::Foundation::HINSTANCE, panel: HWND) -> Result<(), String> {
    let edit = unsafe {
        CreateWindowExW(
            WS_EX_TRANSPARENT,
            wide("EDIT").as_ptr(),
            wide("").as_ptr(),
            WS_CHILD | WS_VISIBLE | ES_LEFT as u32 | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_WANTRETURN as u32,
            ui.edit_rect.left,
            ui.edit_rect.top,
            ui.edit_rect.right - ui.edit_rect.left,
            ui.edit_rect.bottom - ui.edit_rect.top,
            panel,
            EDIT_ID as usize as *mut c_void,
            instance,
            ptr::null(),
        )
    };
    if edit.is_null() {
        return Err(format!(
            "could not create the takeover message box: {}",
            std::io::Error::last_os_error()
        ));
    }
    ui.edit = edit;
    unsafe {
        SendMessageW(edit, WM_SETFONT, ui.fonts.edit as usize, 1);
        SendMessageW(edit, EM_SETLIMITTEXT, ui.labels.max_length as usize, 0);
        let previous = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);
        EDIT_PROC.with(|cell| cell.set(previous));
        let _ = GetWindowLongPtrW(edit, GWLP_WNDPROC);
    }
    Ok(())
}

fn build_ui(panel: HWND, labels: ChatConfig, width: i32, height: i32) -> Result<Ui, String> {
    let face = labels.title.clone();
    let fonts = Fonts {
        title: create_font(18, FW_SEMIBOLD as u32, &face),
        small: create_font(12, FW_NORMAL as u32, &face),
        edit: create_font(14, FW_NORMAL as u32, &face),
        button: create_font(13, FW_SEMIBOLD as u32, &face),
    };
    let brushes = unsafe {
        Brushes {
        background: CreateSolidBrush(color(12, 18, 32)),
        border: CreateSolidBrush(color(51, 65, 85)),
        input: CreateSolidBrush(color(15, 23, 42)),
        input_focus: CreateSolidBrush(color(96, 165, 250)),
        send: CreateSolidBrush(color(37, 99, 235)),
        send_hover: CreateSolidBrush(color(59, 130, 246)),
        send_disabled: CreateSolidBrush(color(30, 58, 138)),
        cancel: CreateSolidBrush(color(69, 10, 10)),
        cancel_hover: CreateSolidBrush(color(127, 29, 29)),
        accent: CreateSolidBrush(color(239, 68, 68)),
        }
    };
    if fonts.title.is_null() || fonts.small.is_null() || fonts.edit.is_null() || fonts.button.is_null() {
        return Err("could not create the takeover chat fonts".to_string());
    }
    let initial_status = labels.idle.clone();
    Ok(Ui {
        panel,
        edit: ptr::null_mut(),
        pointer: ptr::null_mut(),
        labels,
        fonts,
        brushes,
        status: initial_status,
        composing: false,
        pending_compose: false,
        compose_deadline: None,
        pending_send: false,
        sent_text: None,
        prev_foreground: ptr::null_mut(),
        prev_focus: ptr::null_mut(),
        hover: 0,
        width,
        height,
        edit_rect: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        send_rect: RECT { left: 0, top: 0, right: 0, bottom: 0 },
        cancel_rect: RECT { left: 0, top: 0, right: 0, bottom: 0 },
    })
}

fn create_pointer(instance: windows_sys::Win32::Foundation::HINSTANCE, x: i32, y: i32) -> HWND {
    let pointer = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            wide(POINTER_CLASS).as_ptr(),
            wide("").as_ptr(),
            WS_POPUP,
            x,
            y,
            20,
            28,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        )
    };
    if pointer.is_null() {
        return pointer;
    }
    unsafe {
        SetLayeredWindowAttributes(pointer, color(255, 0, 255), 0, LWA_COLORKEY);
    }
    pointer
}

fn free_resources(ui: &mut Ui) {
    unsafe {
        for handle in [
            ui.fonts.title as HGDIOBJ,
            ui.fonts.small as HGDIOBJ,
            ui.fonts.edit as HGDIOBJ,
            ui.fonts.button as HGDIOBJ,
        ] {
            if !handle.is_null() {
                DeleteObject(handle);
            }
        }
        for handle in [
            ui.brushes.background,
            ui.brushes.border,
            ui.brushes.input,
            ui.brushes.input_focus,
            ui.brushes.send,
            ui.brushes.send_hover,
            ui.brushes.send_disabled,
            ui.brushes.cancel,
            ui.brushes.cancel_hover,
            ui.brushes.accent,
        ] {
            if !handle.is_null() {
                DeleteObject(handle as HGDIOBJ);
            }
        }
    }
}

pub fn destroy() {
    if let Some(mut ui) = UI.with(|slot| slot.borrow_mut().take()) {
        takeover::finish_composing();
        if ui.composing {
            ui.composing = false;
            restore_foreground(&mut ui);
        }
        unsafe {
            KillTimer(ui.panel, TIMER_COMPOSE);
            KillTimer(ui.panel, TIMER_FOCUS);
            if !ui.panel.is_null() {
                DestroyWindow(ui.panel);
            }
            if !ui.pointer.is_null() {
                DestroyWindow(ui.pointer);
            }
        }
        free_resources(&mut ui);
    }
    PANEL_HWND.store(0, Ordering::SeqCst);
    POINTER_HWND.store(0, Ordering::SeqCst);
    VISIBLE.store(false, Ordering::SeqCst);
    let mut snapshot = lock(&SNAPSHOT);
    snapshot.visible = false;
    snapshot.composing = false;
    snapshot.pending_send = false;
    snapshot.text_length = 0;
    snapshot.status.clear();
}

fn destroy_if_present() {
    if UI.with(|slot| slot.borrow().is_some()) {
        destroy();
    }
}

pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::SeqCst)
}

/// Mirrors `LAST_POINTER` (a thread-local on the overlay thread) so the main
/// thread can report where the simulated pointer currently is.
static POINTER_SCREEN: Mutex<Option<POINT>> = Mutex::new(None);

fn remember_pointer(location: POINT) {
    LAST_POINTER.with(|cell| cell.set(location));
    *lock(&POINTER_SCREEN) = Some(location);
}

/// Move the simulated pointer (called from the takeover thread's raw input
/// handler with the accumulated hand position).
pub fn pointer_moved(location: POINT) {
    remember_pointer(location);
    post_pointer();
}

pub fn snapshot_value() -> Value {
    let snapshot = lock(&SNAPSHOT).clone();
    let panel = PANEL_HWND.load(Ordering::SeqCst);
    let bounds = if panel == 0 {
        Value::Null
    } else {
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        if unsafe { GetWindowRect(panel as HWND, &mut rect) } == 0 {
            Value::Null
        } else {
            json!({
                "x": rect.left,
                "y": rect.top,
                "width": rect.right - rect.left,
                "height": rect.bottom - rect.top,
            })
        }
    };
    let pointer_screen = lock(&POINTER_SCREEN).map(|point| json!({ "x": point.x, "y": point.y }));
    let pointer_window = POINTER_HWND.load(Ordering::SeqCst);
    let pointer_window = if pointer_window == 0 {
        Value::Null
    } else {
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        if unsafe { GetWindowRect(pointer_window as HWND, &mut rect) } == 0 {
            Value::Null
        } else {
            json!({ "x": rect.left + 1, "y": rect.top + 1 })
        }
    };
    json!({
        "visible": snapshot.visible,
        "composing": snapshot.composing,
        "pendingSend": snapshot.pending_send,
        "textLength": snapshot.text_length,
        "status": snapshot.status,
        "bounds": bounds,
        "pointerScreen": pointer_screen,
        "pointerWindow": pointer_window,
    })
}

/// Called from the mouse hook (overlay thread) for physical mouse messages.
pub fn on_physical_mouse(message: u32, location: POINT) {
    if !is_visible() {
        return;
    }
    match message {
        WM_MOUSEMOVE => {
            remember_pointer(location);
            post_pointer();
        }
        WM_LBUTTONDOWN => {
            remember_pointer(location);
            LAST_CLICK.with(|cell| cell.set(location));
            post_pointer();
            let panel = PANEL_HWND.load(Ordering::SeqCst);
            if panel != 0 {
                unsafe { PostMessageW(panel as HWND, WM_CHAT_CLICK, 0, 0) };
            }
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {}
        WM_RBUTTONDOWN => {
            // Right-click dismisses the editor without touching the desktop.
            with_ui(|ui| {
                if ui.composing {
                    leave_compose(ui, true);
                }
            });
        }
        _ => {}
    }
}

fn post_pointer() {
    if POINTER_POSTED.with(|flag| flag.get()) {
        return;
    }
    let panel = PANEL_HWND.load(Ordering::SeqCst);
    if panel == 0 {
        return;
    }
    POINTER_POSTED.with(|flag| flag.set(true));
    let posted = unsafe { PostMessageW(panel as HWND, WM_CHAT_POINTER, 0, 0) };
    if posted == 0 {
        POINTER_POSTED.with(|flag| flag.set(false));
    }
}

/// True when physical keys must be forwarded to the chat EDIT instead of being
/// discarded. This is only safe while the panel really owns the foreground:
/// otherwise a physical key could reach the app the agent is operating.
pub fn keyboard_should_pass() -> bool {
    peek_ui(|ui| ui.composing && unsafe { GetForegroundWindow() == ui.panel }).unwrap_or(false)
}

/// Called from the main thread after the host delivered (or failed to deliver)
/// a mask message. The overlay thread displays the result.
pub fn ack(ok: bool, note: &str) {
    *lock(&ACK) = Some((ok, note.to_string()));
    let panel = PANEL_HWND.load(Ordering::SeqCst);
    if panel != 0 {
        unsafe { PostMessageW(panel as HWND, WM_CHAT_ACK, 0, 0) };
    }
}

pub fn debug(action: DebugAction) {
    *lock(&DEBUG) = Some(action);
    let panel = PANEL_HWND.load(Ordering::SeqCst);
    if panel != 0 {
        unsafe { PostMessageW(panel as HWND, WM_CHAT_DEBUG, 0, 0) };
    }
}

pub fn debug_pointer(x: i32, y: i32) {
    debug(DebugAction::Pointer(x, y));
}

pub fn debug_click(x: i32, y: i32) {
    debug(DebugAction::Click(x, y));
}

pub fn hide_for_capture() -> bool {
    let mut hidden = false;
    for handle in [PANEL_HWND.load(Ordering::SeqCst), POINTER_HWND.load(Ordering::SeqCst)] {
        if handle != 0 {
            unsafe { ShowWindowAsync(handle as HWND, SW_HIDE) };
            hidden = true;
        }
    }
    hidden
}

pub fn finish_capture(hidden: bool) {
    if !hidden {
        return;
    }
    for handle in [PANEL_HWND.load(Ordering::SeqCst), POINTER_HWND.load(Ordering::SeqCst)] {
        if handle != 0 {
            unsafe { ShowWindowAsync(handle as HWND, SW_SHOWNOACTIVATE) };
        }
    }
}

pub fn set_capture_affinity(excluded: bool) -> bool {
    let affinity = if excluded { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE };
    let mut all = true;
    for handle in [PANEL_HWND.load(Ordering::SeqCst), POINTER_HWND.load(Ordering::SeqCst)] {
        if handle != 0 && unsafe { SetWindowDisplayAffinity(handle as HWND, affinity) } == 0 {
            all = false;
        }
    }
    all
}

