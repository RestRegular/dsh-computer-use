/** Approval gate: ask the human before the agent drives the mouse/keyboard. */

import { formatHotkey, INPUT_ACTIONS } from './control.js';

/** Look a service up without letting a missing/broken one break the gate. */
function service(ctx, name) {
  try {
    return ctx.get?.(name);
  } catch {
    return undefined;
  }
}

/**
 * The session's effective approval policy, or `undefined` when this deployment
 * does not expose one. The gate only needs to know whether a prompt can still
 * reach the human at all.
 */
function approvalPolicyOf(ctx, exec) {
  const session = exec.agent?.session;
  if (session === undefined) return undefined;
  const approval = service(ctx, 'approval');
  try {
    if (typeof approval?.effectivePolicy === 'function') return approval.effectivePolicy(session);
  } catch {}
  // Older hosts without the approval service still project the last
  // `approval/policy` event; read it directly as a fallback.
  try {
    return service(ctx, 'sessionProjections')?.stateOf?.(session, 'permissions')?.approval ?? undefined;
  } catch {
    return undefined;
  }
}

/**
 * Register the per-turn approval gate. `tools/pre-execute` returning `ask`
 * opens the harness approval prompt; the harness rejects it unless the session
 * approval policy is "ask".
 */
export function registerApproval(ctx, config, control) {
  if (!config.consent.requireApproval) return;
  const hotkey = formatHotkey(config.safety.hotkey);
  const takeover = config.takeover.enabled;

  ctx.on('tools/pre-execute', async (exec, next) => {
    if (exec.name !== 'computer_action') return next();
    const action = exec.arguments?.action;
    if (!INPUT_ACTIONS.has(action)) return next();
    // Full access (the "danger-full-access" permission preset) pins the
    // session approval policy to "never". The harness then answers every ask
    // with "rejected" without ever reaching the human, so gating input there
    // would only make the tools fail. The user's full-access decision already
    // covers computer use: run the action without a prompt. The takeover mask,
    // its Cancel takeover button and the emergency hotkey still apply.
    if (approvalPolicyOf(ctx, exec) === 'never') return next();
    const agentId = exec.agent?.id;
    if (agentId !== undefined && control.isApproved(agentId)) return next();
    return {
      kind: 'ask',
      reason: takeover
        ? `The agent wants to control this computer's mouse and keyboard. Approving lets it operate under a takeover: while it drives them a mask covers the screen, your input is handled by the mask's chat panel (message the session or cancel there), and the agent's injected input is paused while you type. Emergency stop: ${hotkey}.`
        : `The agent wants to control this computer's mouse and keyboard. Emergency stop: ${hotkey}.`,
      displayReason: takeover
        ? {
            en: `Allow the agent to control this computer? While it drives the mouse and keyboard, a mask covers the screen and your input goes to its chat panel (message the session or cancel the takeover there). Press ${hotkey} to take back control.`,
            zh: `允许 Agent 接管这台电脑吗？它操作鼠标键盘期间屏幕会被蒙版覆盖，你的输入会进入蒙版的输入框（可以在里面给会话发消息或取消接管）；按 ${hotkey} 可立即终止。`,
          }
        : {
            en: `Allow the agent to control this computer's mouse and keyboard? Press ${hotkey} to stop it.`,
            zh: `允许 Agent 控制这台电脑的鼠标和键盘吗？按 ${hotkey} 可以终止。`,
          },
    };
  });

  // A completed input action is the human's approval for the rest of the turn
  // (or the session, depending on consent.scope). Failed or denied actions do
  // not grant anything, so the next attempt asks again.
  ctx.on('tools/result', (exec, result) => {
    if (exec.name !== 'computer_action' || result.isError) return;
    if (!INPUT_ACTIONS.has(exec.arguments?.action)) return;
    const agentId = exec.agent?.id;
    if (agentId === undefined) return;
    control.approve(agentId);
  });
}
