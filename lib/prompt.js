/** System-prompt guidance for the computer-use tools. */

const GUIDANCE = `You can observe and operate a real Windows desktop through computer_screenshot, computer_action and computer_windows.

Work in small verified steps:
1. Call computer_screenshot first; it returns an image and the captured rectangle. Coordinates in computer_action are pixels of THAT image unless you pass coordinate_space:"screen".
2. Use computer_windows to find the target window, then pass window_id to focus it. Bringing a window forward does not necessarily focus its edit field: click the field before typing.
3. Perform one action, then take a fresh screenshot to verify the outcome. A click that was dispatched does not prove the requested result.
4. Prefer type for literal text and hotkey for shortcuts. Clipboard actions operate on the shared system clipboard.
5. Never operate the DSH window or the Computer Use controls unless the user explicitly asks. If the screen shows passwords, tokens or other sensitive data, stop and ask.

Consent and takeover:
- The first mouse/keyboard action of a turn requires the human's approval. If approval is denied or the session approval policy is "never", the call returns an approval error: stop operating and tell the human, do not retry in a loop.
- Once approved, the plugin takes over the screen: a semi-transparent mask covers it and the human's physical input goes to a chat panel inside the mask, where they can message this session or cancel the takeover. The mask is excluded from screenshots, so keep operating normally. Never try to close, hide, capture around or otherwise bypass the mask.
- While the human types in the mask panel, input actions fail with HUMAN_COMPOSING; wait a moment and retry. If they cancel the takeover, the next input fails with TAKEOVER_ENDED (or asks for approval again).
- Only one agent can hold the takeover at a time. While another agent holds it, input actions fail with TAKEOVER_BUSY.
- When you finish with the desktop before the turn ends, call computer_action with action:"release" to hand control back immediately. The takeover also ends when the turn closes.
- The human can press the emergency hotkey (default Ctrl+Alt+Shift+F12) at any time; it ends the takeover and fails closed. Your next input then fails with TAKEOVER_ENDED or PAUSED. Stop, summarize what you did, and wait for new instructions. Only the human can clear PAUSED by pressing the hotkey again; never try to clear or work around it yourself.`;

export function registerGuidance(ctx) {
  ctx.inject(['systemPrompt'], (scope) => {
    let order;
    try {
      order = scope.systemPrompt.getSectionOrder('TOOL_COMPUTER_USE');
    } catch {}
    const section = {
      name: 'computer-use:windows-native',
      text: GUIDANCE,
    };
    if (order !== undefined) section.order = order;
    scope.systemPrompt.section(section);
  });
}
