//! Global emergency-stop hotkey.
//!
//! The hotkey is polled with `GetAsyncKeyState` on a dedicated thread, so it
//! keeps working while the helper is idle or blocked in a long input action.
//! Pressing it toggles PAUSED, releases every button/modifier the helper may
//! have left down, and refuses further input until the human presses it again.

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
            let down = !codes.is_empty()
                && codes.iter().all(|code| unsafe {
                    (GetAsyncKeyState(*code) as u16 & 0x8000) != 0
                });
            if down && !was_down {
                let next = !is_paused();
                set_paused(next);
                if next {
                    let _ = crate::input::release_all();
                }
            }
            was_down = down;
            crate::util::sleep_ms(30);
        }
    });
}
