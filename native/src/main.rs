//! DSH computer-use native helper.
//!
//! One JSON object per line on stdin, one JSON response per line on stdout.
//! stdout is reserved for protocol frames; diagnostics never go there.

mod capture;
mod clipboard;
mod hotkey;
mod input;
mod util;
mod window;

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

#[derive(Debug)]
pub struct HelperError {
    pub code: &'static str,
    pub message: String,
}

impl HelperError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

pub type HResult<T> = Result<T, HelperError>;

fn string_param<'a>(params: &'a Value, name: &str) -> HResult<&'a str> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| HelperError::new("BAD_ARGS", format!("missing string parameter: {name}")))
}

fn int_param(params: &Value, name: &str) -> HResult<i32> {
    params
        .get(name)
        .and_then(Value::as_i64)
        .map(|value| value as i32)
        .ok_or_else(|| HelperError::new("BAD_ARGS", format!("missing integer parameter: {name}")))
}

fn optional_int(params: &Value, name: &str) -> Option<i32> {
    params.get(name).and_then(Value::as_i64).map(|value| value as i32)
}

fn optional_point(params: &Value) -> HResult<Option<(i32, i32)>> {
    let x = params.get("x").and_then(Value::as_i64);
    let y = params.get("y").and_then(Value::as_i64);
    match (x, y) {
        (None, None) => Ok(None),
        (Some(x), Some(y)) => Ok(Some((x as i32, y as i32))),
        _ => Err(HelperError::new("BAD_ARGS", "x and y must be supplied together")),
    }
}

fn keys_param(params: &Value) -> HResult<Vec<String>> {
    let keys = params
        .get("keys")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    if keys.is_empty() {
        return Err(HelperError::new("BAD_ARGS", "keys must contain at least one key name"));
    }
    Ok(keys)
}

fn hello() -> HResult<Value> {
    Ok(json!({
        "name": "dsh-computer-use-helper",
        "protocol": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "arch": std::env::consts::ARCH,
        "inputSize": std::mem::size_of::<windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT>(),
        "elevated": util::is_elevated(),
        "dpiAware": true,
        "secureDesktop": util::is_secure_desktop(),
        "paused": hotkey::is_paused(),
        "hotkey": hotkey::current_label(),
        "at": util::now_ms(),
        "monitors": capture::monitors()?,
    }))
}

fn status() -> HResult<Value> {
    let foreground = match window::foreground()? { Some(info) => serde_json::to_value(info).unwrap_or(Value::Null), None => Value::Null };
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "paused": hotkey::is_paused(),
        "hotkey": hotkey::current_label(),
        "elevated": util::is_elevated(),
        "secureDesktop": util::is_secure_desktop(),
        "cursor": capture::cursor(),
        "foreground": foreground,
        "monitors": capture::monitors()?,
        "at": util::now_ms(),
    }))
}

fn dispatch(method: &str, params: &Value) -> HResult<Value> {
    match method {
        "hello" => hello(),
        "status" => status(),
        "monitors" => capture::monitors(),
        "screenshot" => capture::screenshot(params),
        "windows.list" => window::list(),
        "window.focus" => {
            let id = string_param(params, "window_id")?;
            let handle = window::parse(id)?;
            Ok(json!({ "window": window::focus(handle)? }))
        }
        "window.state" => {
            let id = string_param(params, "window_id")?;
            let state = string_param(params, "state")?;
            let handle = window::parse(id)?;
            Ok(json!({ "window": window::set_state(handle, state)? }))
        }
        "mouse.move" => {
            let x = int_param(params, "x")?;
            let y = int_param(params, "y")?;
            input::move_to(x, y)?;
            Ok(json!({ "x": x, "y": y }))
        }
        "mouse.move_rel" => {
            let dx = int_param(params, "dx")?;
            let dy = int_param(params, "dy")?;
            let before = capture::cursor();
            input::move_relative(dx, dy)?;
            util::sleep_ms(80);
            let after = capture::cursor();
            Ok(json!({ "dx": dx, "dy": dy, "before": before, "after": after }))
        }
        "debug.set_cursor" => {
            let x = int_param(params, "x")?;
            let y = int_param(params, "y")?;
            input::set_cursor(x, y)?;
            util::sleep_ms(80);
            Ok(json!({ "cursor": capture::cursor() }))
        }
        "debug.desktop" => Ok(input::desktop_names()),
        "debug.env" => Ok(input::env_info()),
        "debug.token" => Ok(input::token_info()),
        "debug.set_cursor_thread" => {
            let x = int_param(params, "x")?;
            let y = int_param(params, "y")?;
            input::set_cursor_on_thread(x, y)?;
            util::sleep_ms(120);
            Ok(json!({ "cursor": capture::cursor() }))
        }
        "debug.raw_move" => {
            let dx = int_param(params, "dx")?;
            let dy = int_param(params, "dy")?;
            let before = capture::cursor();
            input::raw_move_relative(dx, dy)?;
            util::sleep_ms(120);
            let after = capture::cursor();
            Ok(json!({ "dx": dx, "dy": dy, "before": before, "after": after }))
        }
        "mouse.click" => {
            let button = input::parse_button(params.get("button").and_then(Value::as_str))?;
            let clicks = params.get("clicks").and_then(Value::as_u64).unwrap_or(1) as u32;
            let hold_ms = params.get("hold_ms").and_then(Value::as_u64).unwrap_or(0);
            let point = optional_point(params)?;
            input::click(button, clicks, hold_ms, point)?;
            Ok(json!({ "clicks": clicks, "holdMs": hold_ms, "point": point }))
        }
        "mouse.drag" => {
            let button = input::parse_button(params.get("button").and_then(Value::as_str))?;
            let from = (int_param(params, "from_x")?, int_param(params, "from_y")?);
            let to = (int_param(params, "to_x")?, int_param(params, "to_y")?);
            let duration_ms = params.get("duration_ms").and_then(Value::as_u64).unwrap_or(400);
            let steps = params.get("steps").and_then(Value::as_u64).unwrap_or(24) as u32;
            input::drag(button, from, to, duration_ms, steps)?;
            Ok(json!({ "from": from, "to": to, "durationMs": duration_ms, "steps": steps }))
        }
        "mouse.scroll" => {
            let delta_x = optional_int(params, "delta_x").unwrap_or(0);
            let delta_y = optional_int(params, "delta_y").unwrap_or(0);
            if delta_x == 0 && delta_y == 0 {
                return Err(HelperError::new("BAD_ARGS", "delta_x or delta_y must be non-zero"));
            }
            let point = optional_point(params)?;
            input::scroll(delta_x, delta_y, point)?;
            Ok(json!({ "deltaX": delta_x, "deltaY": delta_y }))
        }
        "key.press" => {
            let keys = keys_param(params)?;
            input::press(&keys)?;
            Ok(json!({ "keys": keys }))
        }
        "type" => {
            let text = string_param(params, "text")?;
            let interval_ms = params.get("interval_ms").and_then(Value::as_u64).unwrap_or(0);
            input::type_text(text, interval_ms)?;
            Ok(json!({ "characters": text.chars().count(), "intervalMs": interval_ms }))
        }
        "clipboard.get" => Ok(json!({ "text": clipboard::get()? })),
        "clipboard.set" => {
            let text = string_param(params, "text")?;
            clipboard::set(text)?;
            Ok(json!({ "characters": text.chars().count() }))
        }
        "input.release_all" => {
            input::release_all()?;
            Ok(json!({ "released": true }))
        }
        "configure" => {
            if let Some(names) = params.get("keys").and_then(Value::as_array).map(|values| values.iter().filter_map(Value::as_str).map(ToString::to_string).collect::<Vec<String>>()) {
                hotkey::set_from_names(&names).map_err(|message| HelperError::new("BAD_ARGS", message))?;
            }
            if let Some(combos) = params.get("blocked_combos").and_then(Value::as_array).map(|values| values.iter().filter_map(Value::as_str).map(ToString::to_string).collect::<Vec<String>>()) {
                input::set_blocked(combos);
            }
            Ok(json!({ "hotkey": hotkey::current_label() }))
        }
        other => Err(HelperError::new("UNKNOWN_METHOD", format!("unknown method: {other}"))),
    }
}

fn write_frame(out: &mut impl Write, frame: &Value) {
    let _ = writeln!(out, "{frame}");
    let _ = out.flush();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|arg| arg == "--no-dpi") {
        unsafe {
            let ok = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let _ = ok;
        }
    }
    if !args.iter().any(|arg| arg == "--no-hotkey") {
        hotkey::start();
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_frame(
                    &mut stdout,
                    &json!({ "id": Value::Null, "ok": false, "error": { "code": "BAD_JSON", "message": error.to_string() } }),
                );
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        if method == "shutdown" {
            write_frame(&mut stdout, &json!({ "id": id, "ok": true, "result": { "bye": true } }));
            break;
        }
        let response = match dispatch(method, &params) {
            Ok(result) => json!({ "id": id, "ok": true, "result": result }),
            Err(error) => json!({
                "id": id,
                "ok": false,
                "error": { "code": error.code, "message": error.message },
            }),
        };
        write_frame(&mut stdout, &response);
    }
    let _ = input::release_all();
}
