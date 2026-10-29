//! Global emergency-stop hotkey.
//!
//! The hotkey is polled with `GetAsyncKeyState` on a dedicated thread, so it
//! keeps working while the helper is idle or blocked in a long input action.
//! Pressing it toggles PAUSED, releases every button/modifier the helper may
//! have left down, and refuses further input until the human presses it again.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

const DEFAULT_LABEL: &str = "ctrl+alt+shift+f12";

static PAUSED: AtomicBool = AtomicBool::new(false);
static KEYS: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();
static LABEL: OnceLock<Mutex<String>> = OnceLock::new();

fn keys() -> &'static Mutex<Vec<i32>> {
    KEYS.get_or_init(|| Mutex::new(vec![0x11, 0x12, 0x10, 0x7B]))
}

fn label() -> &'static Mutex<String> {
    LABEL.get_or_init(|| Mutex::new(DEFAULT_LABEL.to_string()))
}

pub fn is_paused() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

pub fn set_paused(value: bool) {
    PAUSED.store(value, Ordering::SeqCst);
}

pub fn current_label() -> String {
    label().lock().unwrap_or_else(|error| error.into_inner()).clone()
}

/// Virtual-key codes of the polling combination. The takeover keyboard hook
/// forwards exactly these keys, so the emergency hotkey is never discarded.
pub fn codes() -> Vec<i32> {
    keys().lock().unwrap_or_else(|error| error.into_inner()).clone()
}

/// Map the side-specific modifier codes a low-level keyboard hook reports
/// (`VK_LCONTROL`/`VK_RCONTROL`, `VK_LSHIFT`/..., `VK_LMENU`/...) onto the
/// generic codes the configuration uses (`ctrl`, `shift`, `alt`). A hook that
/// only compared raw codes used to swallow `Ctrl`/`Alt`/`Shift`, which also
/// kept `GetAsyncKeyState` from ever seeing the combination.
pub fn normalize(code: i32) -> i32 {
    match code {
        0xA0 | 0xA1 => 0x10,
        0xA2 | 0xA3 => 0x11,
        0xA4 | 0xA5 => 0x12,
        other => other,
    }
}

/// True when `code` (raw or side-specific) belongs to the polling combination.
pub fn matches(code: i32) -> bool {
    let code = normalize(code);
    codes().iter().any(|configured| *configured == code)
}

/// True when every key of the combination is held down in `pressed`.
pub fn satisfied(pressed: &HashSet<i32>) -> bool {
    let held: HashSet<i32> = pressed.iter().copied().map(normalize).collect();
    let codes = codes();
    !codes.is_empty() && codes.iter().all(|code| held.contains(code))
}

fn pressed(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

/// Poll one configured key. Side-specific states are checked as well because a
/// forwarded `VK_LCONTROL` is not always mirrored on the generic `VK_CONTROL`.
fn is_down(code: i32) -> bool {
    let code = normalize(code);
    if pressed(code) {
        return true;
    }
    match code {
        0x10 => pressed(0xA0) || pressed(0xA1),
        0x11 => pressed(0xA2) || pressed(0xA3),
        0x12 => pressed(0xA4) || pressed(0xA5),
        _ => false,
    }
}

/// Replace the polling combination. Names are the same names accepted by
/// `input::vk_for_name` (ctrl, alt, shift, win, f1..f24, letters, digits).
pub fn set_from_names(names: &[String]) -> Result<(), String> {
    if names.is_empty() {
        return Err("hotkey must contain at least one key".to_string());
    }
    let mut codes = Vec::with_capacity(names.len());
    for name in names {
        let code = crate::input::vk_for_name(name).ok_or_else(|| format!("unknown hotkey key: {name}"))?;
        codes.push(code as i32);
    }
    *keys().lock().unwrap_or_else(|error| error.into_inner()) = codes;
    *label().lock().unwrap_or_else(|error| error.into_inner()) = names.join("+").to_ascii_lowercase();
    Ok(())
}

pub fn start() {
    thread::spawn(|| {
        let mut was_down = false;
        loop {
            let codes = keys().lock().unwrap_or_else(|error| error.into_inner()).clone();
            let down = !codes.is_empty() && codes.iter().all(|code| is_down(*code));
            if down && !was_down {
                if crate::takeover::is_active() {
                    // While the agent holds the desktop, the emergency hotkey
                    // ends the takeover and fails closed (PAUSED stays on).
                    crate::takeover::emergency_stop("hotkey");
                } else {
                    let next = !is_paused();
                    set_paused(next);
                    if next {
                        let _ = crate::input::release_all();
                    }
                }
            }
            was_down = down;
            crate::util::sleep_ms(30);
        }
    });
}
