# dsh-computer-use

[中文](README.zh.md) | English

Let DeepSeek Harness agents see and operate a real Windows desktop: screenshots, mouse, keyboard, windows and clipboard — behind an explicit human approval and a screen takeover that keeps the human out until the agent is done. A small bundled Rust helper does the native work; no browser or external automation runtime is required.

## Install

```bash
dsh plugin --profile web add @restregular/dsh-computer-use
```

On DSH Desktop you can also install `@restregular/dsh-computer-use` from **Plugins → Plugin Manager** and enable it. On first use the plugin copies the native helper to `%LOCALAPPDATA%\\dsh-computer-use\\` and runs it from there, so a plugin installed under a low-integrity workspace can still inject input.

Requirements: Windows 10/11 x64, DSH 0.2.1 or newer.

## Tools

| Tool | Purpose |
|---|---|
| `computer_screenshot` | Capture the whole virtual desktop, one monitor, a window or a region; returns an image and coordinate metadata |
| `computer_action` | Mouse move/click/double-click/right-click/drag/scroll, keyboard keys/combos/text, wait, clipboard get/set, `release` control |
| `computer_windows` | List top-level windows (id, title, process, bounds, state, foreground), focus one, or minimize/maximize/restore |

### Coordinates

- The screenshot may be downscaled; `computer_action` coordinates default to **that image's pixel space**.
- The result reports the captured desktop rectangle (`capture`) and the model-visible image size (`image`); the plugin maps image pixels back to the desktop.
- Image-space coordinates without a screenshot fail with `SCREENSHOT_REQUIRED`; pass `coordinate_space: "screen"` for absolute desktop pixels.
- All sessions share one desktop; only one agent can hold the takeover at a time.

### Recommended loop

1. `computer_windows` to find the target window;
2. `computer_screenshot` to observe (optionally with `window_id` for a window-only capture);
3. one `computer_action` (focus the window, click the field, then `type`);
4. take a fresh screenshot to verify. A dispatched click does not prove the outcome.

## Consent and takeover

1. **Consent.** The first mouse/keyboard action of each turn goes through the harness approval prompt. This requires the session approval policy to be `ask`; with `never` the prompt is refused automatically and the agent cannot operate (fail closed). Set `consent.scope: session` to keep one approval for the whole session, or `consent.requireApproval: false` to remove the gate entirely.
2. **Takeover.** After approval, the helper takes over before the first injected input:
   - a semi-transparent mask covers the whole virtual desktop, with a banner showing the title and the emergency hotkey;
   - low-level `WH_KEYBOARD_LL` / `WH_MOUSE_LL` hooks discard every **physical** mouse and keyboard event, so the human cannot operate the machine while the agent works;
   - input injected by the agent with `SendInput` carries the `injected` flag and passes through, so the agent keeps working behind the mask;
   - the overlay uses `WDA_EXCLUDEFROMCAPTURE`, so it never pollutes model screenshots (on older Windows the mask is briefly hidden for the capture instead);
   - the mask is click-through and never takes keyboard focus.
3. **Ending.** Control returns to the human when the agent turn closes, when the agent calls `computer_action` with `action: "release"`, when the agent is disposed, on an emergency hotkey, or if the helper exits. `takeover.idleEndMs` can release the takeover after inactivity.
4. **Emergency stop.** While the takeover is active, pressing the emergency hotkey (default `Ctrl+Alt+Shift+F12`) immediately ends the takeover, restores human input, pauses the helper and revokes the turn approval. The in-flight action fails with `TAKEOVER_ENDED`; later input fails with `PAUSED`. The model cannot resume: the human presses the hotkey again to clear `PAUSED`, and the next input asks for approval again.
5. **Critical keys.** Every key of the emergency hotkey is always forwarded through the hooks instead of being discarded. `Ctrl+Alt+Del` is a secure attention sequence that cannot be intercepted, and the lock/UAC desktops stay reachable, so the human can always take the machine back.
6. **One at a time.** Only one agent can hold the takeover. Another agent's input fails with `TAKEOVER_BUSY` until control is released.

Approving from a phone is supported by the harness approval UI (for example through `dsh-pocket`); the mask itself is always drawn on the controlled Windows desktop.

## Safety

- Input is real and **cannot be undone**; the plugin can interrupt, release keys and re-observe.
- Approval and takeover are on by default. `input.requireApproval` is a deprecated alias for `consent.requireApproval`.
- The emergency hotkey (default `Ctrl+Alt+Shift+F12`) pauses the helper, releases every button/modifier and refuses further input until the human presses it again.
- While the takeover is active the mask discards physical mouse/keyboard input. Input injected by other software carries the same `injected` flag as the agent's own input, so the hooks cannot tell them apart.
- `Win+L`, `Ctrl+Alt+Del` and `Win+U` are blocked for the agent by default (`input.blockedCombos`).
- Optional process allow/deny lists: `safety.allowedProcesses` / `safety.blockedProcesses`.
- Lock screens and UAC secure desktops report `SECURE_DESKTOP`; a non-elevated helper cannot inject into elevated windows (UIPI).
- Screenshots enter the DSH session attachment store and may contain sensitive desktop content; base64 is never written to the session log.

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
      maxTextLength: 20000
    consent:
      requireApproval: true
      scope: turn         # turn | session
    takeover:
      enabled: true
      opacity: 0.18       # 0.05 .. 0.9
      banner: true
      title: ""           # empty = localized default
      hint: ""            # empty = localized default with the hotkey
      idleEndMs: 0        # 0 = release when the agent turn ends
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
| `idleStopMs` | 300000 | Stop an idle helper; 0 keeps it resident (a takeover holds the helper alive) |
| `screenshot.maxEdge` | 1600 | Longest edge before encoding; smaller saves image tokens |
| `input.typeIntervalMs` | 0 | Delay between typed characters |
| `input.dragSteps` | 24 | Drag interpolation steps |
| `input.maxHoldMs` | 5000 | Maximum explicit button hold |
| `consent.requireApproval` | true | Ask before the first input action; `input.requireApproval` still works as an alias |
| `consent.scope` | turn | `turn` asks again next turn, `session` keeps one approval until the agent is disposed |
| `takeover.enabled` | true | Mask the screen and discard physical input while the agent operates |
| `takeover.opacity` | 0.18 | Mask opacity, 0.05–0.9 |
| `takeover.banner` | true | Show the banner with the emergency hotkey |
| `takeover.title` / `hint` | empty | Banner text; empty uses a localized default |
| `takeover.idleEndMs` | 0 | Release after this much inactivity; 0 releases when the turn ends |
| `safety.hotkey` | ctrl+alt+shift+f12 | Emergency stop and takeover abort |
| `safety.allowedProcesses` | [] | If non-empty, only these process names accept input |
| `safety.blockedProcesses` | [] | Always refuse input to these process names |

## Development

```bash
npm run build:native        # cargo build --release, stage the exe under native/bin/win32-x64
npm test                    # Node unit tests
node scripts/smoke.mjs      # helper-level screenshot/window/mouse/clipboard test
node scripts/notepad-test.mjs "$LOCALAPPDATA/dsh-computer-use/helper-test.exe"
node scripts/takeover-test.mjs "$LOCALAPPDATA/dsh-computer-use/helper-test.exe"  # mask + hooks + emergency path
```

Run the native tests on a disposable desktop and stage the exe under `%LOCALAPPDATA%` first: a helper started from a low-integrity workspace can capture the screen but cannot inject input. The takeover test verifies the mask, capture exclusion, injected input passing through the hooks, host release and the emergency-stop path.

The Rust helper lives in `native/` and speaks one JSON object per line over stdio. It handles one request at a time; the Host kills it on timeout/cancel and sends `input.release_all` after a restart. The takeover overlay and the hooks run on their own thread with a message loop; if the helper dies Windows removes the hooks and the overlay automatically.

## Known limitations

- Windows x64 only; ARM64 Windows runs the x64 build under emulation (not separately validated).
- No UIA accessibility tree yet; actions use screenshots and coordinates.
- GDI can capture black frames for some GPU/DRM windows; window capture tries `PrintWindow` and falls back to the screen rectangle. Windows Graphics Capture is a follow-up.
- Unsigned native executables may trigger SmartScreen or antivirus prompts on some machines.
- Multiple sessions share one desktop; takeover is coordinated in-process but not between independent DSH processes.
- Some games and protected apps use Raw Input and may ignore `SendInput`.
- The low-level hooks only cover the interactive desktop; secure desktops (UAC, lock screen) are unaffected, which is also what keeps `Ctrl+Alt+Del` available to the human.

## License

MIT. Rust dependency licenses remain with their crates; the published package contains only the compiled helper.
