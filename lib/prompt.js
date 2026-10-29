/** System-prompt guidance for the computer-use tools. */

const GUIDANCE = `You can observe and operate a real Windows desktop through computer_screenshot, computer_action and computer_windows.

Work in small verified steps:
1. Call computer_screenshot first; it returns an image and the captured rectangle. Coordinates in computer_action are pixels of THAT image unless you pass coordinate_space:"screen".
2. Use computer_windows to find the target window, then pass window_id to focus it. Bringing a window forward does not necessarily focus its edit field: click the field before typing.
3. Perform one action, then take a fresh screenshot to verify the outcome. A click that was dispatched does not prove the requested result.
4. Prefer type for literal text and hotkey for shortcuts. Clipboard actions operate on the shared system clipboard.
5. Never operate the DSH window or the Computer Use controls unless the user explicitly asks. If the screen shows passwords, tokens or other sensitive data, stop and ask.
6. Input is real and cannot be undone. The user can pause everything with the emergency hotkey (default Ctrl+Alt+Shift+F12); resume by pressing it again.`;

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
