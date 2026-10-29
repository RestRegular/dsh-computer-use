# dsh-computer-use

[中文](README.zh.md) | English

Let DeepSeek Harness agents see and operate a real Windows desktop: screenshots, mouse, keyboard, windows and clipboard. A small bundled Rust helper does the native work; no browser or external automation runtime is required.

## Install

```bash
dsh plugin --profile web add @restregular/dsh-computer-use
```

On DSH Desktop you can also install `@restregular/dsh-computer-use` from **Plugins → Plugin Manager** and enable it. On first use the plugin copies the native helper to `%LOCALAPPDATA%\dsh-computer-use\` and runs it from there, so a plugin installed under a low-integrity workspace can still inject input.

Requirements: Windows 10/11 x64, DSH 0.2.1 or newer.

## Tools

| Tool | Purpose |
|---|---|
| `computer_screenshot` | Capture the whole virtual desktop, one monitor, a window or a region; returns an image and coordinate metadata |
| `computer_action` | Mouse move/click/double-click/right-click/drag/scroll, keyboard keys/combos/text, wait, clipboard get/set |
| `computer_windows` | List top-level windows (id, title, process, bounds, state, foreground), focus one, or minimize/maximize/restore |

### Coordinates

- The screenshot may be downscaled; `computer_action` coordinates default to **that image's pixel space**.
- The result reports the captured desktop rectangle (`capture`) and the model-visible image size (`image`); the plugin maps image pixels back to the desktop.
- Image-space coordinates without a screenshot fail with `SCREENSHOT_REQUIRED`; pass `coordinate_space: "screen"` for absolute desktop pixels.
- All sessions share one desktop; the plugin does not coordinate between them.

### Recommended loop

1. `computer_windows` to find the target window;
2. `computer_screenshot` to observe (optionally with `window_id` for a window-only capture);
3. one `computer_action` (focus the window, click the field, then `type`);
4. take a fresh screenshot to verify. A dispatched click does not prove the outcome.

## Safety

- Input is real and **cannot be undone**; the plugin can interrupt, release keys and re-observe.
- Emergency hotkey, default `Ctrl+Alt+Shift+F12`: pauses the helper, releases every button/modifier, and refuses further input until pressed again. The model cannot resume on its own.
- `Win+L`, `Ctrl+Alt+Del` and `Win+U` are blocked by default (`input.blockedCombos`).
- Optional process allow/deny lists: `safety.allowedProcesses` / `safety.blockedProcesses`.
- Lock screens and UAC secure desktops report `SECURE_DESKTOP`; a non-elevated helper cannot inject into elevated windows (UIPI).
- Screenshots enter the DSH session attachment store and may contain sensitive desktop content; base64 is never written to the session log.
- `input.requireApproval: true` asks once per session before the first input action (an approval policy that denies prompts will refuse it).

## Configuration

Override the plugin row in `cordis.patch.yml`; every field has a default:

```yaml
- id: dsh-computer-use
  config:
    screenshot:
      format: jpeg        # png | jpeg
      jpegQuality: 85
      maxEdge: 1200       # 0 keeps the captured size
      includeCursor: true
      captureMethod: auto # auto | gdi | printwindow
    input:
      typeIntervalMs: 0
      dragSteps: 24
      maxHoldMs: 5000
      blockedCombos: ["win+l", "ctrl+alt+del", "win+u"]
      requireApproval: false
      maxTextLength: 20000
    safety:
      hotkey: "ctrl+alt+shift+f12"
      allowedProcesses: []
      blockedProcesses: []
    timeoutMs: 15000
    idleStopMs: 300000
    debug:
      logHelper: false
```

| Field | Default | Meaning |
|---|---|---|
| `enabled` | true | Register the tools |
| `helperPath` | empty | Explicit helper exe; otherwise the packaged helper is staged under LOCALAPPDATA |
| `timeoutMs` | 15000 | One helper request timeout |
| `idleStopMs` | 300000 | Stop an idle helper; 0 keeps it resident |
| `screenshot.maxEdge` | 1600 | Longest edge before encoding; smaller saves image tokens |
| `input.typeIntervalMs` | 0 | Delay between typed characters |
| `input.dragSteps` | 24 | Drag interpolation steps |
| `input.maxHoldMs` | 5000 | Maximum explicit button hold |
| `safety.hotkey` | ctrl+alt+shift+f12 | Emergency stop |
| `safety.allowedProcesses` | [] | If non-empty, only these process names accept input |
| `safety.blockedProcesses` | [] | Always refuse input to these process names |

## Development

```bash
npm run build:native        # cargo build --release, stage the exe under native/bin/win32-x64
npm test                    # Node unit tests
node scripts/smoke.mjs      # helper-level screenshot/window/mouse/clipboard test
node scripts/notepad-test.mjs "$LOCALAPPDATA/dsh-computer-use/helper-test.exe"
```

The Rust helper lives in `native/` and speaks one JSON object per line over stdio. It handles one request at a time; the Host kills it on timeout/cancel and sends `input.release_all` after a restart.

## Known limitations

- Windows x64 only; ARM64 Windows runs the x64 build under emulation (not separately validated).
- No UIA accessibility tree yet (planned for v0.2); actions use screenshots and coordinates.
- GDI can capture black frames for some GPU/DRM windows; window capture tries `PrintWindow` and falls back to the screen rectangle. Windows Graphics Capture is a follow-up.
- Unsigned native executables may trigger SmartScreen or antivirus prompts on some machines.
- Multiple sessions or independent DSH processes share one desktop and are not coordinated.
- Some games and protected apps use Raw Input and may ignore `SendInput`.

## License

MIT. Rust dependency licenses remain with their crates; the published package contains only the compiled helper.
