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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, EndPaint,
    FillRect, FrameRect, GetDC, GetStockObject, GetTextExtentPoint32W, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, UpdateWindow, BLACK_BRUSH, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DEFAULT_PITCH,
    DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_NORMAL, FW_SEMIBOLD, HBRUSH, HDC, HFONT,
    HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, PeekMessageW, PostThreadMessageW,
    RegisterClassExW, SetCursorPos, SetLayeredWindowAttributes, SetWindowDisplayAffinity,
    SetWindowLongPtrW, SetWindowPos, ShowWindowAsync, TranslateMessage, UnhookWindowsHookEx,
    KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, LWA_ALPHA, MSG, MSLLHOOKSTRUCT, PM_REMOVE,
    SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_DESTROY, WM_DISPLAYCHANGE, WM_INPUT, WM_KEYDOWN,
    WM_MOUSEMOVE, WM_PAINT, WM_SYSKEYDOWN, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, GWL_EXSTYLE,
    GWLP_USERDATA, HWND_TOPMOST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, HHOOK};

use crate::util::{now_ms, sleep_ms, wide};
use crate::{HelperError, HResult};

const WM_TAKEOVER_END: u32 = WM_APP + 1;
/// Posted to the overlay thread when the physical pointer dragged the system
/// cursor away from the position the agent put it at.
const WM_TAKEOVER_PIN: u32 = WM_APP + 0x21;
const TINT_CLASS: &str = "DshComputerUseTakeoverTint";
const BANNER_CLASS: &str = "DshComputerUseTakeoverBanner";

/// How long `takeover.end` waits for the overlay thread to dismantle itself.
const END_TIMEOUT_MS: u64 = 5000;

/// Encoded `POINT` the agent's injected moves last asked for. The low-level
/// mouse hook discards the human's mouse messages, but Windows still moves the
/// system cursor for every physical move *before* the hook runs, so the mask
/// has to pull it back here or the agent's pointer is constantly yanked to
/// wherever the human's hand is.
static POINTER_TARGET: AtomicI64 = AtomicI64::new(i64::MIN);
/// Only one pull-back may be queued at a time; otherwise a 1000 Hz mouse floods
/// the overlay thread's message queue.
static PIN_PENDING: AtomicBool = AtomicBool::new(false);

/// Where the human's hand is, as opposed to where the OS cursor is. Pinning the
/// cursor with `SetCursorPos` makes the low-level hook's `pt` meaningless (it
/// reports a jitter around the pin), so the mask accumulates Raw Input deltas
/// instead to draw a simulated pointer that can cross the screen.
struct PointerState {
    /// Accumulated physical pointer position (what the mask draws).
    hand: POINT,
}
static POINTER_STATE: Mutex<Option<PointerState>> = Mutex::new(None);

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
static DOWN_KEYS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
static HOTKEY_FIRED: AtomicBool = AtomicBool::new(false);
static CONTROL: Mutex<()> = Mutex::new(());
static THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static END_REASON: Mutex<Option<String>> = Mutex::new(None);
static END_TX: Mutex<Option<mpsc::Sender<(bool, String)>>> = Mutex::new(None);
static EVENT_SINK: OnceLock<fn(&str, Value)> = OnceLock::new();

/// Input dispatch and the mask chat panel share this gate. A running input
/// command takes `busy`; the panel may only take the foreground while `busy`
/// is zero, and after it does, every new input command is refused with
/// `HUMAN_COMPOSING`. That closes the race where injected keys could land in
/// the panel's EDIT control instead of the app the agent is operating.
struct InputGate {
    busy: u32,
    composing: bool,
}
static INPUT_GATE: Mutex<InputGate> = Mutex::new(InputGate { busy: 0, composing: false });

pub struct InputGuard;

impl InputGuard {
    pub fn acquire() -> HResult<Self> {
        {
            let mut gate = lock(&INPUT_GATE);
            if gate.composing {
                return Err(HelperError::new(
                    "HUMAN_COMPOSING",
                    "the human is typing a message in the takeover panel; wait for them to finish or cancel the takeover, then retry",
                ));
            }
            gate.busy += 1;
        }
        // The mask is hit-testable so pointer-based input (precision touchpad
        // scrolling, mouse-in-pointer apps) lands on it and is swallowed. That
        // must not eat the agent's own injected clicks, so the mask goes
        // click-through for the duration of an input command.
        set_click_through(true);
        Ok(InputGuard)
    }
}

impl Drop for InputGuard {
    fn drop(&mut self) {
        let remaining = {
            let mut gate = lock(&INPUT_GATE);
            gate.busy = gate.busy.saturating_sub(1);
            gate.busy
        };
        if remaining == 0 {
            // Injected messages are hit-tested by the raw input thread a
            // moment after SendInput returns; keep the mask click-through for
            // a little longer so a click is not swallowed by its own shield.
            let _ = thread::Builder::new()
                .name("dsh-computer-use-unshield".to_string())
                .spawn(|| {
                    sleep_ms(60);
                    if lock(&INPUT_GATE).busy == 0 {
                        set_click_through(false);
                    }
                });
        }
    }
}

pub fn input_guard() -> HResult<InputGuard> {
    InputGuard::acquire()
}

pub fn is_composing() -> bool {
    lock(&INPUT_GATE).composing
}

/// Give the chat panel the foreground for editing. Returns false while an
/// input command is still running; the panel retries on a short timer.
pub fn try_begin_composing() -> bool {
    let mut gate = lock(&INPUT_GATE);
    if gate.composing {
        return true;
    }
    if gate.busy > 0 {
        return false;
    }
    gate.composing = true;
    true
}

pub fn finish_composing() {
    lock(&INPUT_GATE).composing = false;
}


fn down_keys() -> &'static Mutex<HashSet<i32>> {
    DOWN_KEYS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn same_point(left: &POINT, right: &POINT) -> bool {
    left.x == right.x && left.y == right.y
}

fn pack_point(point: POINT) -> i64 {
    ((point.x as u32 as i64) << 32) | (point.y as u32 as i64)
}

fn unpack_point(value: i64) -> POINT {
    POINT { x: (value >> 32) as i32, y: value as u32 as i32 }
}

/// Remember where the agent's pointer must stay. Called for every injected
/// move that passes the hook, so a human drag is always undone back to the
/// last position the agent injected.
pub fn set_pointer_target(point: POINT) {
    POINTER_TARGET.store(pack_point(point), Ordering::SeqCst);
}

pub fn pointer_target() -> Option<POINT> {
    let value = POINTER_TARGET.load(Ordering::SeqCst);
    if value == i64::MIN { None } else { Some(unpack_point(value)) }
}

fn clear_pointer_target() {
    POINTER_TARGET.store(i64::MIN, Ordering::SeqCst);
    PIN_PENDING.store(false, Ordering::SeqCst);
    *lock(&POINTER_STATE) = None;
}

fn current_cursor() -> Option<POINT> {
    let mut point = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut point) } == 0 { None } else { Some(point) }
}

/// Seed the hand position at takeover start.
fn reset_pointer_state(cursor: POINT) {
    *lock(&POINTER_STATE) = Some(PointerState { hand: cursor });
}

/// Fold one raw device delta (hardware counts, unaffected by the pinned cursor)
/// into the hand position. This is the only reliable way to track a physical
/// pointer while `SetCursorPos` keeps snapping the cursor back: the low-level
/// hook only reports positions derived from that same cursor, and Windows drops
/// the accumulated ballistics on every snap.
fn advance_hand_raw(dx: i32, dy: i32) {
    let mut state = lock(&POINTER_STATE);
    let entry = state.get_or_insert(PointerState {
        hand: current_cursor().unwrap_or(POINT { x: 0, y: 0 }),
    });
    entry.hand.x = entry.hand.x.saturating_add(dx).clamp(-100_000, 100_000);
    entry.hand.y = entry.hand.y.saturating_add(dy).clamp(-100_000, 100_000);
}

/// Absolute devices report 0..65535 across the virtual desktop.
fn advance_hand_absolute(x: i32, y: i32) {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    let point = POINT {
        x: left + (x as i64 * width as i64 / 65535) as i32,
        y: top + (y as i64 * height as i64 / 65535) as i32,
    };
    if let Some(state) = lock(&POINTER_STATE).as_mut() {
        state.hand = point;
    }
}

/// Whether to record a trace of physical events for diagnosis.
static TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_trace_enabled(value: bool) {
    TRACE_ENABLED.store(value, Ordering::Relaxed);
}

/// Record where the OS cursor is after an out-of-band move. Kept for the debug
/// protocol: raw input does not depend on it.
pub fn note_cursor_position(_point: POINT) {}

/// The simulated pointer position, for status and tests.
pub fn hand_position() -> Option<POINT> {
    lock(&POINTER_STATE).as_ref().map(|state| state.hand)
}

/// Queue a cursor pull-back on the overlay thread. Runs inside the low-level
/// hook, so it must only read state and post a message: calling `SetCursorPos`
/// from the hook callback can deadlock the raw input thread that is waiting on
/// the hook to return.
fn pin_pointer() {
    let Some(target) = pointer_target() else { return };
    let Some(current) = current_cursor() else { return };
    if same_point(&current, &target) {
        return;
    }
    if PIN_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let thread = THREAD_ID.load(Ordering::SeqCst);
    if thread == 0 || unsafe { PostThreadMessageW(thread, WM_TAKEOVER_PIN, 0, 0) } == 0 {
        PIN_PENDING.store(false, Ordering::SeqCst);
    }
}

/// Undo a human cursor move. Runs on the overlay thread from its message loop,
/// never from the hook itself.
fn apply_pointer_pin() {
    PIN_PENDING.store(false, Ordering::SeqCst);
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let Some(target) = pointer_target() else { return };
    if let Some(current) = current_cursor() {
        if !same_point(&current, &target) {
            unsafe { SetCursorPos(target.x, target.y) };
        }
    }
}

/// Test hook: run the deferred pull-back immediately, the way the message loop
/// would after the physical pointer moved.
pub fn debug_pin() {
    apply_pointer_pin();
}

/// Test hook: queue the pull-back through the real hook path (compare, post,
/// message loop, `SetCursorPos`) without needing a physical mouse.
pub fn debug_post_pin() {
    pin_pointer();
}

/// Make the mask swallow input (`false`) or let it through (`true`).
///
/// The mask has to be hit-testable: precision touchpads deliver two-finger
/// scrolling as `WM_POINTERWHEEL`, and Chromium-style apps take mouse input in
/// pointer mode, so neither ever reaches a low-level mouse hook. With a
/// hit-testable mask those messages land on it and are discarded. The agent's
/// own injected clicks are classic input and must reach the app underneath, so
/// the mask becomes click-through for the duration of an input command.
pub fn set_click_through(value: bool) {
    let tint = OVERLAY.load(Ordering::SeqCst);
    if tint == 0 {
        return;
    }
    let hwnd = tint as HWND;
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let desired = if value { style | WS_EX_TRANSPARENT } else { style & !WS_EX_TRANSPARENT };
    if desired != style {
        unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, desired as isize) };
    }
}

/// Every low-level mouse event seen by [`probe_mouse_events`]:
/// `(at_ms, message, x, y, flags, mouse_data, extra_info)`.
static PROBE_EVENTS: Mutex<Vec<(u64, u32, i32, i32, u32, u32, u64)>> = Mutex::new(Vec::new());

/// Ring buffer of the last physical events while a takeover is active, so a
/// live run can be explained afterwards. `WM_INPUT` entries carry the raw delta
/// in `point`; hook entries carry the (pinned) cursor position.
#[derive(Clone)]
struct TraceEntry {
    at: u64,
    message: u32,
    point: POINT,
    expected: POINT,
    target: Option<POINT>,
    hand: POINT,
}
static TRACE: Mutex<Vec<TraceEntry>> = Mutex::new(Vec::new());

fn push_trace(entry: TraceEntry) {
    let mut trace = lock(&TRACE);
    if trace.len() >= 1000 {
        trace.remove(0);
    }
    trace.push(entry);
}

pub fn hook_trace(limit: usize) -> Value {
    let trace = lock(&TRACE).clone();
    let start = trace.len().saturating_sub(limit.clamp(1, 1000));
    Value::Array(
        trace[start..]
            .iter()
            .map(|entry| {
                json!({
                    "at": entry.at,
                    "msg": format!("0x{:04X}", entry.message),
                    "pt": { "x": entry.point.x, "y": entry.point.y },
                    "expected": { "x": entry.expected.x, "y": entry.expected.y },
                    "target": entry.target.map(|point| json!({ "x": point.x, "y": point.y })),
                    "hand": { "x": entry.hand.x, "y": entry.hand.y },
                })
            })
            .collect(),
    )
}

pub fn clear_trace() {
    lock(&TRACE).clear();
}

unsafe extern "system" fn probe_mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        let mut events = lock(&PROBE_EVENTS);
        if events.len() < 30_000 {
            events.push((
                now_ms(),
                wparam as u32,
                info.pt.x,
                info.pt.y,
                info.flags,
                info.mouseData,
                info.dwExtraInfo as u64,
            ));
        }
    }
    CallNextHookEx(ptr::null_mut(), code, wparam, lparam)
}

/// Diagnostics: record what the low-level mouse hook actually receives for a
/// while, so the host can tell a real mouse apart from input that never
/// reaches `WH_MOUSE_LL` (precision touchpads deliver `WM_POINTER`).
pub fn probe_mouse_events(ms: u64, path: Option<&str>) -> Value {
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(probe_mouse_proc), instance, 0) };
    if hook.is_null() {
        return json!({ "error": format!("SetWindowsHookExW failed: {}", std::io::Error::last_os_error()) });
    }
    lock(&PROBE_EVENTS).clear();
    let deadline = now_ms() + ms.clamp(100, 120_000);
    let mut message: MSG = unsafe { std::mem::zeroed() };
    while now_ms() < deadline {
        unsafe {
            while PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        sleep_ms(2);
    }
    unsafe { UnhookWindowsHookEx(hook) };
    let events = lock(&PROBE_EVENTS).clone();

    let mut messages: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    let mut extra_info: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    let mut injected = 0u32;
    let mut moved = 0u32;
    for (_, message, _, _, flags, _, extra) in &events {
        *messages.entry(format!("0x{message:04X}")).or_insert(0) += 1;
        *extra_info.entry(format!("0x{extra:X}")).or_insert(0) += 1;
        if flags & LLMHF_INJECTED != 0 {
            injected += 1;
        }
        if *message == WM_MOUSEMOVE {
            moved += 1;
        }
    }
    if let Some(path) = path {
        let mut text = String::with_capacity(events.len() * 64);
        for (at, message, x, y, flags, data, extra) in &events {
            text.push_str(&format!(
                "{{\"at\":{at},\"msg\":\"0x{message:04X}\",\"x\":{x},\"y\":{y},\"flags\":{flags},\"data\":{data},\"extra\":\"0x{extra:X}\"}}\n"
            ));
        }
        let _ = std::fs::write(path, text);
    }
    let first = events.first().map(|event| json!({ "x": event.2, "y": event.3 }));
    let last = events.last().map(|event| json!({ "x": event.2, "y": event.3 }));
    json!({
        "ms": ms,
        "total": events.len(),
        "injected": injected,
        "moved": moved,
        "messages": messages,
        "extraInfo": extra_info,
        "first": first,
        "last": last,
        "cursor": current_cursor().map(|point| json!({ "x": point.x, "y": point.y })),
    })
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

/// Let the chat panel report human actions to the host.
pub(crate) fn emit_ui_event(event: &str, data: Value) {
    emit(event, data);
}

#[derive(Clone)]
struct TakeoverConfig {
    opacity: u8,
    title: String,
    hint: String,
    banner: bool,
    chat: Option<crate::chat::ChatConfig>,
}

struct InitInfo {
    overlay: bool,
    banner: bool,
    chat: bool,
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
        "composing": is_composing(),
        "chat": crate::chat::snapshot_value(),
        "pointer": pointer_target().map(|point| json!({ "x": point.x, "y": point.y })),
        "hand": hand_position().map(|point| json!({ "x": point.x, "y": point.y })),
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
        WM_INPUT => {
            handle_raw_mouse(lparam);
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

/// Receive physical mouse deltas from the raw input stack. These are hardware
/// counts, so `SetCursorPos` pinning cannot erase them.
unsafe fn handle_raw_mouse(lparam: LPARAM) {
    let mut buffer = [0u8; 128];
    let mut size = buffer.len() as u32;
    let written = GetRawInputData(
        lparam as *mut core::ffi::c_void,
        RID_INPUT,
        buffer.as_mut_ptr() as *mut core::ffi::c_void,
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    if written == u32::MAX || written == 0 {
        return;
    }
    let raw = &*(buffer.as_ptr() as *const RAWINPUT);
    if raw.header.dwType != RIM_TYPEMOUSE {
        return;
    }
    let mouse = &raw.data.mouse;
    if mouse.usFlags as u32 & MOUSE_MOVE_ABSOLUTE as u32 != 0 {
        advance_hand_absolute(mouse.lLastX, mouse.lLastY);
    } else if mouse.lLastX != 0 || mouse.lLastY != 0 {
        advance_hand_raw(mouse.lLastX, mouse.lLastY);
    } else {
        return;
    }
    let hand = hand_position().unwrap_or(POINT { x: 0, y: 0 });
    if TRACE_ENABLED.load(Ordering::Relaxed) {
        push_trace(TraceEntry {
            at: now_ms(),
            message: WM_INPUT,
            point: POINT { x: mouse.lLastX, y: mouse.lLastY },
            expected: POINT { x: 0, y: 0 },
            target: pointer_target(),
            hand,
        });
    }
    crate::chat::pointer_moved(hand);
}

fn register_raw_mouse(target: HWND) -> Result<(), String> {
    let device = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: target,
    };
    let ok = unsafe { RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) };
    if ok == 0 {
        return Err(format!("RegisterRawInputDevices failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn unregister_raw_mouse() {
    let device = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: ptr::null_mut(),
    };
    unsafe { RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) };
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
    // the combination so the polling hotkey thread still sees the press. The
    // hook also detects the completed combination itself — a low-level hook
    // reports side-specific modifier codes (`VK_LCONTROL`) and a swallowed key
    // never reaches `GetAsyncKeyState`, so polling alone could miss the press.
    if crate::hotkey::matches(info.vkCode as i32) {
        let key = crate::hotkey::normalize(info.vkCode as i32);
        let complete = {
            let mut held = lock(down_keys());
            if wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN {
                held.insert(key);
            } else {
                held.remove(&key);
            }
            crate::hotkey::satisfied(&held)
        };
        if complete && !HOTKEY_FIRED.swap(true, Ordering::SeqCst) {
            // `release_all()` injects input, so never run it inside the hook
            // callback: end the takeover from a short-lived thread instead.
            thread::spawn(|| crate::takeover::emergency_stop("hotkey"));
        }
        return CallNextHookEx(ptr::null_mut(), code, wparam, lparam);
    }
    // The mask chat panel may own the keyboard while the human types a message.
    // Only forward keys when the panel really holds the foreground; otherwise
    // a physical key could reach the app the agent is operating.
    if crate::chat::keyboard_should_pass() {
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
    if ACTIVE.load(Ordering::SeqCst) {
        if (info.flags & LLMHF_INJECTED) != 0 {
            // The agent's own moves define where the OS cursor has to stay.
            if wparam as u32 == WM_MOUSEMOVE {
                set_pointer_target(info.pt);
            }
            return CallNextHookEx(ptr::null_mut(), code, wparam, lparam);
        }
        // The physical `pt` is relative to the pinned cursor, so the hand
        // position comes from raw input instead (see `WM_INPUT`).
        let hand = hand_position().unwrap_or(info.pt);
        if TRACE_ENABLED.load(Ordering::Relaxed) {
            push_trace(TraceEntry {
                at: now_ms(),
                message: wparam as u32,
                point: info.pt,
                expected: info.pt,
                target: pointer_target(),
                hand,
            });
        }
        // The human's physical pointer is rendered and hit-tested by the mask
        // chat panel; the OS cursor stays wherever the agent put it.
        crate::chat::on_physical_mouse(wparam as u32, hand);
        // Windows moves the system cursor for the physical move before this
        // hook sees it; queue a pull-back to the agent's position.
        pin_pointer();
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
    chat: bool,
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
                // Deliberately *not* WS_EX_TRANSPARENT: the mask must be
                // hit-testable so pointer-based physical input is swallowed
                // instead of reaching the app. Input commands flip it to
                // click-through while they inject (see `set_click_through`).
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
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
            // Raw mouse deltas drive the simulated pointer: the low-level hook
            // only sees positions derived from the pinned cursor. Not fatal if
            // it fails (the mask itself is what keeps the desktop safe).
            let _ = register_raw_mouse(tint);
            let mut banner = ptr::null_mut();
            if config.banner {
                match create_banner(&config) {
                    Ok((handle, banner_excluded)) => {
                        banner = handle;
                        capture_excluded = capture_excluded && banner_excluded;
                    }
                    Err(error) => {
                        // The tint and hooks are the safety-critical part; a
                        // missing banner must not leave the desktop usable.
                        let _ = error;
                    }
                }
            }
            // The chat panel is optional as well: if it cannot be created the
            // takeover still blocks all physical input like before.
            let mut chat = false;
            if let Some(chat_config) = config.chat.as_ref() {
                match crate::chat::create(chat_config) {
                    Ok(session) => {
                        chat = true;
                        capture_excluded = capture_excluded && session.capture_excluded;
                    }
                    Err(_) => {
                        chat = false;
                    }
                }
            }
            if let Err(error) = install_hooks(instance) {
                crate::chat::destroy();
                if !banner.is_null() {
                    DestroyWindow(banner);
                }
                DestroyWindow(tint);
                return Err(error);
            }
            Ok(Session {
                tint,
                banner,
                chat,
                capture_excluded,
            })
        }
    }

    fn destroy(mut self) {
        uninstall_hooks();
        unregister_raw_mouse();
        crate::chat::destroy();
        finish_composing();
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
    HOTKEY_FIRED.store(false, Ordering::SeqCst);
    HOOK_KEYS_BLOCKED.store(0, Ordering::SeqCst);
    HOOK_MOUSE_BLOCKED.store(0, Ordering::SeqCst);
    // Until the agent injects its first move, the cursor is pinned where it
    // already was so the human cannot drag it around under the mask.
    clear_pointer_target();
    if let Some(cursor) = current_cursor() {
        set_pointer_target(cursor);
        reset_pointer_state(cursor);
    }

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
                chat: session.chat,
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
    if reason == "hotkey" || reason == "error" || reason == "debug" || reason == "cancelled" {
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
        if message.message == WM_TAKEOVER_PIN {
            apply_pointer_pin();
            continue;
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
        clear_pointer_target();
        finish_composing();
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
        chat: params.get("chat").and_then(crate::chat::ChatConfig::from_json),
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
        "chat": info.chat,
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
    finish_composing();
    let _ = crate::input::release_all();
    request_end(reason);
}

/// The human clicked "cancel takeover" in the mask chat panel. Fail closed like
/// an emergency stop (abort in-flight input and refuse further injected input),
/// but do not press the PAUSED latch: the human may approve a new takeover
/// without pressing the hotkey first.
pub fn cancel_from_ui() {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    CANCELLED.store(true, Ordering::SeqCst);
    finish_composing();
    let _ = crate::input::release_all();
    request_end("cancelled");
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
    crate::chat::hide_for_capture();
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
    crate::chat::finish_capture(hidden);
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
    crate::chat::set_capture_affinity(false);
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
    if !crate::chat::set_capture_affinity(true) {
        excluded = false;
    }
    CAPTURE_EXCLUDED.store(excluded, Ordering::SeqCst);
}
