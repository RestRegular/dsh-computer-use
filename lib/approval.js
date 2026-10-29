/** Optional per-session approval gate for input actions. */

const INPUT_ACTIONS = new Set([
  'move',
  'click',
  'double_click',
  'right_click',
  'middle_click',
  'drag',
  'scroll',
  'key',
  'hotkey',
  'type',
  'clipboard_set',
  'release_all',
]);

export function registerApproval(ctx, config) {
  if (!config.input.requireApproval) return;
  const approved = new Set();
  ctx.on('tools/pre-execute', async (exec, next) => {
    if (exec.name !== 'computer_action') return next();
    if (!INPUT_ACTIONS.has(exec.arguments?.action)) return next();
    const id = exec.agent?.id === undefined ? '' : String(exec.agent.id);
    if (id !== '' && approved.has(id)) return next();
    return {
      kind: 'ask',
      reason: 'The agent wants to control the mouse and keyboard of this computer.',
      displayReason: {
        en: 'Allow the agent to control the mouse and keyboard?',
        zh: '允许 Agent 控制这台电脑的鼠标和键盘吗？',
      },
    };
  });
  ctx.on('tools/result', (exec, result) => {
    if (exec.name !== 'computer_action' || result.isError) return;
    const id = exec.agent?.id === undefined ? '' : String(exec.agent.id);
    if (id !== '') approved.add(id);
  });
  ctx.on('agent/disposed', ({ agent }) => {
    if (agent?.id !== undefined) approved.delete(String(agent.id));
  });
}
