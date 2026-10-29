//! Mouse, keyboard and Unicode text injection through `SendInput`.

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, VK_DELETE, VK_DOWN, VK_END,
    VK_HOME, VK_INSERT, VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_UP, VkKeyScanW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use std::sync::{Mutex, OnceLock};

use crate::util::{last_error, sleep_ms};
use crate::{HelperError, HResult};

const WHEEL_DELTA: i32 = 120;
const VK_CONTROL: u16 = 0x11;
const VK_SHIFT: u16 = 0x10;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;

pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    fn down(&self) -> MOUSE_EVENT_FLAGS {
        match self {
            Self::Left => MOUSEEVENTF_LEFTDOWN,
            Self::Right => MOUSEEVENTF_RIGHTDOWN,
            Self::Middle => MOUSEEVENTF_MIDDLEDOWN,
        }
    }

    fn up(&self) -> MOUSE_EVENT_FLAGS {
        match self {
            Self::Left => MOUSEEVENTF_LEFTUP,
            Self::Right => MOUSEEVENTF_RIGHTUP,
            Self::Middle => MOUSEEVENTF_MIDDLEUP,
        }
    }
}

pub fn parse_button(value: Option<&str>) -> HResult<Button> {
    match value.unwrap_or("left").to_ascii_lowercase().as_str() {
        "left" | "l" => Ok(Button::Left),
        "right" | "r" => Ok(Button::Right),
        "middle" | "m" => Ok(Button::Middle),
        other => Err(HelperError::new("BAD_ARGS", format!("unknown mouse button: {other}"))),
    }
}

fn screen_bounds() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    }
}

fn absolute(point: (i32, i32)) -> (i32, i32) {
    let (left, top, width, height) = screen_bounds();
    let x = ((point.0 - left) as i64 * 65535 / (width as i64 - 1).max(1)) as i32;
    let y = ((point.1 - top) as i64 * 65535 / (height as i64 - 1).max(1)) as i32;
    (x.clamp(0, 65535), y.clamp(0, 65535))
}

fn mouse_event(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_event(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(events: &[INPUT]) -> HResult<()> {
    if events.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(events.len() as u32, events.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        return Err(HelperError::new("INPUT_FAILED", format!("SendInput accepted {sent}/{} events: {}", events.len(), last_error())));
    }
    Ok(())
}

fn check_ready() -> HResult<()> {
    if crate::takeover::is_cancelled() {
        return Err(HelperError::new(
            "TAKEOVER_ENDED",
            "the human ended the takeover with the emergency hotkey; stop operating and wait for a new approval",
        ));
    }
    if crate::hotkey::is_paused() {
        return Err(HelperError::new(
            "PAUSED",
            "computer use is paused by the emergency hotkey; ask the human to press it again",
        ));
    }
    if crate::util::is_secure_desktop() {
        return Err(HelperError::new(
            "SECURE_DESKTOP",
            "the current desktop is locked or showing a secure prompt",
        ));
    }
    if crate::takeover::input_required() && !crate::takeover::is_active() {
        return Err(HelperError::new(
            "TAKEOVER_REQUIRED",
            "mouse and keyboard input requires an active consent takeover; the host must show the mask first",
        ));
    }
    Ok(())
}

pub fn move_to(x: i32, y: i32) -> HResult<()> {
    check_ready()?;
    let (dx, dy) = absolute((x, y));
    send(&[mouse_event(dx, dy, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)])
}

pub fn move_relative(dx: i32, dy: i32) -> HResult<()> {
    check_ready()?;
    send(&[mouse_event(dx, dy, 0, MOUSEEVENTF_MOVE)])
}

/// Mouse button press helper that releases the button on every early return.
struct ButtonGuard {
    button: Button,
    armed: bool,
}

impl ButtonGuard {
    fn press(button: Button) -> HResult<Self> {
        send(&[mouse_event(0, 0, 0, button.down())])?;
        Ok(Self { button, armed: true })
    }

    fn release(&mut self) -> HResult<()> {
        if self.armed {
            self.armed = false;
            send(&[mouse_event(0, 0, 0, self.button.up())])?;
        }
        Ok(())
    }
}

impl Drop for ButtonGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = send(&[mouse_event(0, 0, 0, self.button.up())]);
            self.armed = false;
        }
    }
}

pub fn click(button: Button, clicks: u32, hold_ms: u64, point: Option<(i32, i32)>) -> HResult<()> {
    check_ready()?;
    if let Some((x, y)) = point {
        move_to(x, y)?;
    }
    for index in 0..clicks.clamp(1, 3) {
        check_ready()?;
        send(&[mouse_event(0, 0, 0, button.down())])?;
        if hold_ms > 0 {
            sleep_ms(hold_ms);
        }
        send(&[mouse_event(0, 0, 0, button.up())])?;
        if index + 1 < clicks {
            sleep_ms(40);
        }
    }
    Ok(())
}

pub fn drag(
    button: Button,
    from: (i32, i32),
    to: (i32, i32),
    duration_ms: u64,
    steps: u32,
) -> HResult<()> {
    check_ready()?;
    move_to(from.0, from.1)?;
    let mut guard = ButtonGuard::press(button)?;
    let steps = steps.clamp(1, 240);
    let step_delay = if duration_ms == 0 { 0 } else { duration_ms / steps as u64 };
    for step in 1..=steps {
        check_ready()?;
        let ratio = step as f64 / steps as f64;
        let x = from.0 as f64 + (to.0 - from.0) as f64 * ratio;
        let y = from.1 as f64 + (to.1 - from.1) as f64 * ratio;
        move_to(x.round() as i32, y.round() as i32)?;
        if step_delay > 0 {
            sleep_ms(step_delay);
        }
    }
    guard.release()
}

pub fn scroll(dx: i32, dy: i32, point: Option<(i32, i32)>) -> HResult<()> {
    check_ready()?;
    if let Some((x, y)) = point {
        move_to(x, y)?;
    }
    if dy != 0 {
        let mut events = Vec::new();
        for _ in 0..dy.unsigned_abs() {
            events.push(mouse_event(0, 0, WHEEL_DELTA as u32, MOUSEEVENTF_WHEEL));
        }
        send(&events)?;
    }
    if dx != 0 {
        let mut events = Vec::new();
        for _ in 0..dx.unsigned_abs() {
            events.push(mouse_event(0, 0, WHEEL_DELTA as u32, MOUSEEVENTF_HWHEEL));
        }
        send(&events)?;
    }
    Ok(())
}

fn extended(vk: u16) -> bool {
    matches!(
        vk,
        VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_INSERT | VK_DELETE | VK_RWIN | 0x5D | 0x6F
    )
}

pub fn vk_for_name(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "ctrl" | "control" => VK_CONTROL,
        "shift" => VK_SHIFT,
        "alt" | "menu" => VK_MENU,
        "win" | "meta" | "super" | "cmd" => VK_LWIN,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "space" | "spacebar" => 0x20,
        "backspace" | "back" => 0x08,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" | "prior" => VK_PRIOR,
        "pagedown" | "pgdn" | "next" => VK_NEXT,
        "up" | "arrowup" => VK_UP,
        "down" | "arrowdown" => VK_DOWN,
        "left" | "arrowleft" => VK_LEFT,
        "right" | "arrowright" => VK_RIGHT,
        "capslock" => 0x14,
        "numlock" => 0x90,
        "scrolllock" => 0x91,
        "printscreen" | "prtsc" => 0x2C,
        "pause" => 0x13,
        "apps" | "menu-key" => 0x5D,
        "plus" => 0xBB,
        "minus" => 0xBD,
        "comma" => 0xBC,
        "period" => 0xBE,
        "slash" => 0xBF,
        "semicolon" => 0xBA,
        "quote" => 0xDE,
        "backslash" => 0xDC,
        "bracketleft" | "[" => 0xDB,
        "bracketright" | "]" => 0xDD,
        "`" | "backtick" | "grave" => 0xC0,
        _ => {
            if let Some(rest) = lower.strip_prefix('f') {
                if let Ok(number) = rest.parse::<u16>() {
                    if (1..=24).contains(&number) {
                        return Some(0x70 + number - 1);
                    }
                }
            }
            let mut chars = lower.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if first.is_ascii_alphabetic() {
                return Some(first.to_ascii_uppercase() as u16);
            }
            if first.is_ascii_digit() {
                return Some(first as u16);
            }
            let scan = unsafe { VkKeyScanW(first as u16) };
            if scan == -1 {
                return None;
            }
            return Some((scan as u16) & 0xFF);
        }
    };
    Some(named)
}

fn canonical_name(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "control" => "ctrl".to_string(),
        "meta" | "super" | "cmd" => "win".to_string(),
        "return" => "enter".to_string(),
        "escape" => "esc".to_string(),
        "del" => "delete".to_string(),
        "ins" => "insert".to_string(),
        "pgup" => "pageup".to_string(),
        "pgdn" => "pagedown".to_string(),
        other => other.to_string(),
    }
}

fn canonical_combo(value: &str) -> String {
    let mut parts: Vec<String> = value
        .split('+')
        .map(canonical_name)
        .filter(|part| !part.is_empty())
        .collect();
    parts.sort();
    parts.join("+")
}

fn blocked_list() -> &'static Mutex<Vec<String>> {
    static BLOCKED: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    BLOCKED.get_or_init(|| Mutex::new(vec!["l+win".to_string(), "u+win".to_string(), "alt+ctrl+del".to_string()]))
}

pub fn set_blocked(combos: Vec<String>) {
    let mut list = blocked_list().lock().unwrap_or_else(|error| error.into_inner());
    list.clear();
    for combo in combos {
        let canonical = canonical_combo(&combo);
        if !canonical.is_empty() && !list.contains(&canonical) {
            list.push(canonical);
        }
    }
}

fn blocked(keys: &[u16]) -> bool {
    let mut normalized: Vec<String> = keys
        .iter()
        .map(|vk| match *vk {
            VK_CONTROL => "ctrl".to_string(),
            VK_SHIFT => "shift".to_string(),
            VK_MENU => "alt".to_string(),
            VK_LWIN | VK_RWIN => "win".to_string(),
            VK_DELETE => "delete".to_string(),
            other => char::from_u32(other as u32).map(|c| c.to_ascii_lowercase().to_string()).unwrap_or_else(|| format!("vk{other}")),
        })
        .collect();
    normalized.sort();
    let joined = normalized.join("+");
    blocked_list().lock().unwrap_or_else(|error| error.into_inner()).iter().any(|combo| combo == &joined)
}

pub fn press(keys: &[String]) -> HResult<()> {
    check_ready()?;
    if keys.is_empty() {
        return Err(HelperError::new("BAD_ARGS", "keys must contain at least one key"));
    }
    let mut codes = Vec::with_capacity(keys.len());
    for name in keys {
        let code = vk_for_name(name).ok_or_else(|| HelperError::new("BAD_ARGS", format!("unknown key: {name}")))?;
        codes.push(code);
    }
    if blocked(&codes) {
        return Err(HelperError::new("BLOCKED_COMBO", "this key combination is blocked by the computer-use safety policy"));
    }
    let mut events = Vec::new();
    for code in &codes {
        events.push(key_event(*code, 0, if extended(*code) { KEYEVENTF_EXTENDEDKEY } else { 0 }));
    }
    for code in codes.iter().rev() {
        events.push(key_event(
            *code,
            0,
            KEYEVENTF_KEYUP | if extended(*code) { KEYEVENTF_EXTENDEDKEY } else { 0 },
        ));
    }
    send(&events)
}

pub fn type_text(text: &str, interval_ms: u64) -> HResult<()> {
    check_ready()?;
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let interval = interval_ms.min(1000);
    for character in normalized.chars() {
        // A long `type` must stop as soon as the human cancels the takeover or
        // presses the emergency hotkey, not after the remaining characters.
        check_ready()?;
        if character == '\n' {
            press(&["enter".to_string()])?;
        } else if character == '\t' {
            press(&["tab".to_string()])?;
        } else {
            let mut units = [0u16; 2];
            for unit in character.encode_utf16(&mut units) {
                send(&[
                    key_event(0, *unit, KEYEVENTF_UNICODE),
                    key_event(0, *unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                ])?;
            }
        }
        if interval > 0 {
            sleep_ms(interval);
        }
    }
    Ok(())
}

pub fn release_all() -> HResult<()> {
    let events = vec![
        mouse_event(0, 0, 0, MOUSEEVENTF_LEFTUP),
        mouse_event(0, 0, 0, MOUSEEVENTF_RIGHTUP),
        mouse_event(0, 0, 0, MOUSEEVENTF_MIDDLEUP),
        key_event(VK_CONTROL, 0, KEYEVENTF_KEYUP),
        key_event(VK_SHIFT, 0, KEYEVENTF_KEYUP),
        key_event(VK_MENU, 0, KEYEVENTF_KEYUP),
        key_event(VK_LWIN, 0, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY),
        key_event(VK_RWIN, 0, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY),
    ];
    send(&events)
}


#[repr(C)]
struct RawMouseInput { dx: i32, dy: i32, mouse_data: u32, flags: u32, time: u32, extra: usize }

#[repr(C)]
struct RawInput { typ: u32, mi: RawMouseInput }

#[link(name = "user32")]
extern "system" {
    #[link_name = "SendInput"]
    fn RawSendInput(count: u32, inputs: *const RawInput, size: i32) -> u32;
}

pub fn raw_move_relative(dx: i32, dy: i32) -> HResult<()> {
    let input = RawInput { typ: 0, mi: RawMouseInput { dx, dy, mouse_data: 0, flags: 1, time: 0, extra: 0 } };
    let sent = unsafe { RawSendInput(1, &input, std::mem::size_of::<RawInput>() as i32) };
    if sent != 1 {
        return Err(HelperError::new("INPUT_FAILED", format!("raw SendInput returned {sent}")));
    }
    Ok(())
}


#[link(name = "user32")]
extern "system" {
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetProcessWindowStation() -> isize;
    fn GetThreadDesktop(thread_id: u32) -> isize;
    fn GetUserObjectInformationW(handle: isize, index: i32, info: *mut core::ffi::c_void, length: u32, needed: *mut u32) -> i32;
}

pub fn set_cursor(x: i32, y: i32) -> HResult<()> {
    if unsafe { SetCursorPos(x, y) } == 0 {
        return Err(HelperError::new("INPUT_FAILED", format!("SetCursorPos failed: {}", last_error())));
    }
    Ok(())
}

fn object_name(handle: isize) -> String {
    if handle == 0 { return String::new(); }
    let mut needed = 0u32;
    unsafe { GetUserObjectInformationW(handle, 2, std::ptr::null_mut(), 0, &mut needed); }
    if needed == 0 { return String::new(); }
    let mut buffer = vec![0u16; (needed as usize + 1) / 2];
    let ok = unsafe { GetUserObjectInformationW(handle, 2, buffer.as_mut_ptr() as *mut _, needed, &mut needed) };
    if ok == 0 { return String::new(); }
    crate::util::from_wide(&buffer)
}

pub fn desktop_names() -> serde_json::Value {
    let station = unsafe { GetProcessWindowStation() };
    let desktop = unsafe { GetThreadDesktop(windows_sys::Win32::System::Threading::GetCurrentThreadId()) };
    serde_json::json!({
        "windowStation": object_name(station),
        "desktop": object_name(desktop),
    })
}


#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn ProcessIdToSessionId(process_id: u32, session_id: *mut u32) -> i32;
}

#[link(name = "advapi32")]
extern "system" {
    fn GetUserNameW(buffer: *mut u16, length: *mut u32) -> i32;
}

pub fn env_info() -> serde_json::Value {
    let pid = unsafe { GetCurrentProcessId() };
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut session); }
    let mut buffer = [0u16; 256];
    let mut length = buffer.len() as u32;
    let user = if unsafe { GetUserNameW(buffer.as_mut_ptr(), &mut length) } != 0 {
        crate::util::from_wide(&buffer)
    } else {
        String::new()
    };
    serde_json::json!({
        "pid": pid,
        "threadId": unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() },
        "sessionId": session,
        "user": user,
        "desktop": desktop_names(),
    })
}

pub fn set_cursor_on_thread(x: i32, y: i32) -> HResult<()> {
    let handle = std::thread::spawn(move || set_cursor(x, y));
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(HelperError::new("INPUT_FAILED", "cursor thread panicked")),
    }
}


use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TokenIsAppContainer, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub fn token_info() -> serde_json::Value {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return serde_json::json!({ "error": "OpenProcessToken failed" });
        }
        let mut buffer = [0u8; 256];
        let mut length = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            buffer.len() as u32,
            &mut length,
        );
        let rid = if ok != 0 {
            let integrity = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            if !integrity.Label.Sid.is_null() {
                let count = *GetSidSubAuthorityCount(integrity.Label.Sid);
                if count > 0 {
                    *GetSidSubAuthority(integrity.Label.Sid, (count - 1) as u32) as u32
                } else { 0 }
            } else { 0 }
        } else { 0 };
        let mut app_container = 0u32;
        let mut returned = 0u32;
        GetTokenInformation(
            token,
            TokenIsAppContainer,
            &mut app_container as *mut _ as *mut core::ffi::c_void,
            4,
            &mut returned,
        );
        windows_sys::Win32::Foundation::CloseHandle(token);
        serde_json::json!({
            "integrityRid": rid,
            "integrity": match rid { 0x1000 => "low", 0x2000 => "medium", 0x3000 => "high", 0x4000 => "system", _ => "other" },
            "appContainer": app_container != 0,
            "elevated": crate::util::is_elevated(),
        })
    }
}
