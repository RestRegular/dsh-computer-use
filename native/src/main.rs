//! DSH computer-use native helper.
//!
//! One JSON object per line on stdin, one JSON response per line on stdout.
//! stdout is reserved for protocol frames; diagnostics never go there.

mod capture;
mod chat;
mod clipboard;
mod hotkey;
mod input;
mod takeover;
mod util;
mod window;

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

/// stdout is shared with the takeover thread, which emits `takeover.ended`
/// events while the main thread may be writing a response. One lock per frame
/// keeps the line-delimited protocol intact.
static OUTPUT: OnceLock<Mutex<std::io::Stdout>> = OnceLock::new();

fn emit_event(name: &str, data: Value) {
    let Some(output) = OUTPUT.get() else { return };
    if let Ok(mut out) = output.lock() {
        write_frame(&mut *out, &json!({ "event": name, "data": data }));
    }
}

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
        "cursorMovable": input::cursor_movable(),
        "paused": hotkey::is_paused(),
        "hotkey": hotkey::current_label(),
        "takeover": takeover::status_value(),
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
        "takeover": takeover::status_value(),
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
        "screenshot" => {
            // SetWindowDisplayAffinity keeps the takeover mask out of captures
            // on Windows 10 2004+. Where that is unavailable, hide the overlay
            // for the duration of the capture instead.
            let hidden = takeover::hide_for_capture();
            let result = capture::screenshot(params);
            takeover::finish_capture(hidden);
            result
        }
        "takeover.begin" => takeover::begin(params),
        "takeover.end" => {
            let reason = params.get("reason").and_then(Value::as_str).unwrap_or("host");
            takeover::end(reason)
        }
        "takeover.status" => Ok(takeover::status_value()),
        "takeover.chat.ack" => {
            let ok = params.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let note = params.get("note").and_then(Value::as_str).unwrap_or("");
            chat::ack(ok, note);
            Ok(json!({ "displayed": true }))
        }
        "debug.chat" => {
            let action = params.get("action").and_then(Value::as_str).unwrap_or("");
            match action {
                "pointer" => {
                    chat::debug_pointer(
                        optional_int(params, "x").unwrap_or(0),
                        optional_int(params, "y").unwrap_or(0),
                    );
                }
                "click" => {
                    chat::debug_click(
                        optional_int(params, "x").unwrap_or(0),
                        optional_int(params, "y").unwrap_or(0),
                    );
                }
                "compose" => chat::debug(chat::DebugAction::Compose),
                "send" => chat::debug(chat::DebugAction::Send),
                "text" => {
                    let text = params.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                    chat::debug(chat::DebugAction::Text(text));
                }
                "keys" => {
                    let keys = params.get("keys").and_then(Value::as_str).unwrap_or("").to_string();
                    chat::debug(chat::DebugAction::Keys(keys));
                }
                "press" => {
                    let keys: Vec<String> = params
                        .get("keys")
                        .and_then(Value::as_array)
                        .map(|values| values.iter().filter_map(Value::as_str).map(ToString::to_string).collect())
                        .unwrap_or_default();
                    if keys.is_empty() {
                        return Err(HelperError::new("BAD_ARGS", "debug.chat press requires a non-empty keys array"));
                    }
                    chat::debug(chat::DebugAction::Press(keys));
                }
                other => return Err(HelperError::new("BAD_ARGS", format!("unknown debug.chat action: {other}"))),
            }
            Ok(json!({ "queued": true, "status": takeover::status_value() }))
        }
        "debug.emergency" => {
            let reason = params.get("reason").and_then(Value::as_str).unwrap_or("debug");
            takeover::emergency_stop(reason);
            Ok(takeover::status_value())
        }
        "debug.mask_capture" => {
            takeover::disable_capture_exclusion();
            let result = capture::screenshot(params);
            takeover::restore_capture_exclusion();
            result
        }
        "windows.list" => window::list(),
        "window.focus" => {
            let _guard = takeover::input_guard()?;
            let id = string_param(params, "window_id")?;
            let handle = window::parse(id)?;
            Ok(json!({ "window": window::focus(handle)? }))
        }
        "window.state" => {
            let _guard = takeover::input_guard()?;
            let id = string_param(params, "window_id")?;
            let state = string_param(params, "state")?;
            let handle = window::parse(id)?;
            Ok(json!({ "window": window::set_state(handle, state)? }))
        }
        "mouse.move" => {
            let _guard = takeover::input_guard()?;
            let x = int_param(params, "x")?;
            let y = int_param(params, "y")?;
            input::move_to(x, y)?;
            Ok(json!({ "x": x, "y": y }))
        }
        "mouse.move_rel" => {
            let _guard = takeover::input_guard()?;
            let dx = int_param(params, "dx")?;
            let dy = int_param(params, "dy")?;
            let before = capture::cursor();
            input::move_relative(dx, dy)?;
            util::sleep_ms(80);
            let after = capture::cursor();
            Ok(json!({ "dx": dx, "dy": dy, "before": before, "after": after }))
        }
        "debug.set_cursor" => {
            let _guard = takeover::input_guard()?;
            let x = int_param(params, "x")?;
            let y = int_param(params, "y")?;
            input::set_cursor(x, y)?;
            util::sleep_ms(80);
            Ok(json!({ "cursor": capture::cursor() }))
        }
        "debug.desktop" => Ok(input::desktop_names()),
        "debug.env" => Ok(input::env_info()),
        "debug.token" => Ok(input::token_info()),
        "debug.pin_pointer" => {
            takeover::debug_pin();
            Ok(json!({ "cursor": capture::cursor(), "takeover": takeover::status_value() }))
        }
        "debug.pin_post" => {
            takeover::debug_post_pin();
            util::sleep_ms(50);
            Ok(json!({ "cursor": capture::cursor(), "takeover": takeover::status_value() }))
        }
        "debug.cursor_force" => {
            // Move the cursor the way the human's hand would: without updating
            // the takeover's pointer target, so `debug.pin_pointer` can prove
            // the pull-back works.
            let x = int_param(params, "x")?;
            let y = int_param(params, "y")?;
            input::set_cursor_raw(x, y)?;
            util::sleep_ms(30);
            Ok(json!({ "cursor": capture::cursor() }))
        }
        "debug.set_cursor_thread" => {
            let _guard = takeover::input_guard()?;
            let x = int_param(params, "x")?;
            let y = int_param(params, "y")?;
            input::set_cursor_on_thread(x, y)?;
            util::sleep_ms(120);
            Ok(json!({ "cursor": capture::cursor() }))
        }
        "debug.raw_move" => {
            let _guard = takeover::input_guard()?;
            let dx = int_param(params, "dx")?;
            let dy = int_param(params, "dy")?;
            let before = capture::cursor();
            input::raw_move_relative(dx, dy)?;
            util::sleep_ms(120);
            let after = capture::cursor();
            Ok(json!({ "dx": dx, "dy": dy, "before": before, "after": after }))
        }
        "mouse.click" => {
            let _guard = takeover::input_guard()?;
            let button = input::parse_button(params.get("button").and_then(Value::as_str))?;
            let clicks = params.get("clicks").and_then(Value::as_u64).unwrap_or(1) as u32;
            let hold_ms = params.get("hold_ms").and_then(Value::as_u64).unwrap_or(0);
            let point = optional_point(params)?;
            input::click(button, clicks, hold_ms, point)?;
            Ok(json!({ "clicks": clicks, "holdMs": hold_ms, "point": point }))
        }
        "mouse.drag" => {
            let _guard = takeover::input_guard()?;
            let button = input::parse_button(params.get("button").and_then(Value::as_str))?;
            let from = (int_param(params, "from_x")?, int_param(params, "from_y")?);
            let to = (int_param(params, "to_x")?, int_param(params, "to_y")?);
            let duration_ms = params.get("duration_ms").and_then(Value::as_u64).unwrap_or(400);
            let steps = params.get("steps").and_then(Value::as_u64).unwrap_or(24) as u32;
            input::drag(button, from, to, duration_ms, steps)?;
            Ok(json!({ "from": from, "to": to, "durationMs": duration_ms, "steps": steps }))
        }
        "mouse.scroll" => {
            let _guard = takeover::input_guard()?;
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
            let _guard = takeover::input_guard()?;
            let keys = keys_param(params)?;
            input::press(&keys)?;
            Ok(json!({ "keys": keys }))
        }
        "type" => {
            let _guard = takeover::input_guard()?;
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
            if let Some(required) = params.get("require_takeover").and_then(Value::as_bool) {
                takeover::set_input_required(required);
            }
            Ok(json!({ "hotkey": hotkey::current_label(), "takeoverRequired": takeover::input_required() }))
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
    OUTPUT.set(Mutex::new(std::io::stdout())).ok();
    takeover::set_event_sink(emit_event);
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
    let output = OUTPUT.get();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                if let Some(output) = output {
                    if let Ok(mut out) = output.lock() {
                        write_frame(
                            &mut *out,
                            &json!({ "id": Value::Null, "ok": false, "error": { "code": "BAD_JSON", "message": error.to_string() } }),
                        );
                    }
                }
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        if method == "shutdown" {
            if let Some(output) = output {
                if let Ok(mut out) = output.lock() {
                    write_frame(&mut *out, &json!({ "id": id, "ok": true, "result": { "bye": true } }));
                }
            }
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
        if let Some(output) = output {
            if let Ok(mut out) = output.lock() {
                write_frame(&mut *out, &response);
            }
        }
    }
    takeover::shutdown();
    let _ = input::release_all();
}
