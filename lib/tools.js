/** Model-facing tools: screenshot, input action and window management. */

import { captureScreenshot, renderScreenshot } from './screenshot.js';
import { resolvePoint, resolveTo } from './coordinates.js';
import { asComputerUseError, ComputerUseError } from './errors.js';

const SCREENSHOT_OUTPUT = {
  schema: { type: 'object', additionalProperties: true },
  render: renderScreenshot,
};

const JSON_OUTPUT = {
  schema: { type: 'object', additionalProperties: true },
  render(_args, value) {
    return [{ type: 'text', text: JSON.stringify(value) }];
  },
};

const ACTIONS = [
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
  'wait',
  'clipboard_get',
  'clipboard_set',
  'release_all',
];

const SYNONYMS = new Map([
  ['control', 'ctrl'],
  ['meta', 'win'],
  ['super', 'win'],
  ['cmd', 'win'],
  ['return', 'enter'],
  ['escape', 'esc'],
  ['del', 'delete'],
  ['ins', 'insert'],
  ['pgup', 'pageup'],
  ['pgdn', 'pagedown'],
  ['arrowup', 'up'],
  ['arrowdown', 'down'],
  ['arrowleft', 'left'],
  ['arrowright', 'right'],
]);

function canonicalKey(name) {
  const lower = String(name).trim().toLowerCase();
  return SYNONYMS.get(lower) ?? lower;
}

function canonicalCombo(keys) {
  return keys.map(canonicalKey).sort().join('+');
}

function blockedCombos(config) {
  return new Set(
    (config.input.blockedCombos ?? []).map((combo) =>
      canonicalCombo(String(combo).split('+').map((part) => part.trim()).filter(Boolean)),
    ),
  );
}

function assertComboAllowed(config, keys) {
  if (!Array.isArray(keys) || keys.length === 0) return;
  const combo = canonicalCombo(keys);
  if (blockedCombos(config).has(combo)) {
    throw new ComputerUseError('BLOCKED_COMBO', `the key combination ${combo} is blocked by the computer-use safety policy`);
  }
}

function processName(value) {
  return typeof value === 'string' ? value.trim().toLowerCase() : '';
}

function assertProcessAllowed(config, process) {
  const name = processName(process);
  if (!name) return;
  if (config.safety.blockedProcesses.includes(name)) {
    throw new ComputerUseError('BLOCKED_PROCESS', `the process ${name} is blocked by the computer-use safety policy`);
  }
  if (config.safety.allowedProcesses.length > 0 && !config.safety.allowedProcesses.includes(name)) {
    throw new ComputerUseError('BLOCKED_PROCESS', `the process ${name} is not in safety.allowedProcesses`);
  }
}

function needsProcessPolicy(config) {
  return config.safety.allowedProcesses.length > 0 || config.safety.blockedProcesses.length > 0;
}

async function focusWindow(helper, config, windowId, exec) {
  const result = await helper.request('window.focus', { window_id: windowId }, { signal: exec.signal, timeoutMs: 10000 });
  assertProcessAllowed(config, result?.window?.process);
  return result.window ?? null;
}

async function assertForegroundAllowed(helper, config, exec) {
  if (!needsProcessPolicy(config)) return;
  const status = await helper.request('status', {}, { signal: exec.signal, timeoutMs: 5000 });
  assertProcessAllowed(config, status?.foreground?.process);
}

function pointParams(point) {
  return point ? { x: point.x, y: point.y } : {};
}

function wait(ms, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(new ComputerUseError('ABORTED', 'the call was cancelled'));
      return;
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      reject(new ComputerUseError('ABORTED', 'the call was cancelled'));
    };
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

/**
 * Register the three computer-use tools on the plugin context.
 * @param {import('@deepseek-ai/cordis').Context} ctx
 * @param {import('./helper-client.js').HelperClient} helper
 * @param {ReturnType<import('./config.js').normalizeConfig>} config
 */
export function registerTools(ctx, helper, config) {
  const definitions = [
    {
      name: 'computer_screenshot',
      description:
        'Capture the Windows desktop, one monitor, a window or a region and return an image plus coordinate metadata. Call this before coordinate actions and after each action to verify the result. Coordinates in computer_action are in THIS returned image\'s pixel space (which may be downscaled); use coordinate_space:"screen" only for absolute desktop pixels. The metadata includes the captured rectangle, image dimensions, cursor position, foreground window (for window captures) and capture method.',
      parameters: {
        type: 'object',
        properties: {
          target: {
            type: 'string',
            enum: ['screen', 'monitor', 'window', 'region'],
            description: 'What to capture (default screen = the whole virtual desktop).',
          },
          monitor: { type: 'integer', minimum: 0, description: 'Zero-based monitor index for target=monitor (or screen).' },
          window_id: { type: 'string', description: 'Window id from computer_windows for target=window.' },
          region: {
            type: 'object',
            properties: {
              x: { type: 'integer' },
              y: { type: 'integer' },
              width: { type: 'integer', minimum: 1 },
              height: { type: 'integer', minimum: 1 },
            },
            required: ['x', 'y', 'width', 'height'],
            additionalProperties: false,
            description: 'Desktop rectangle in physical screen pixels for target=region.',
          },
          include_cursor: { type: 'boolean', description: 'Draw the mouse cursor (default from config).' },
          format: { type: 'string', enum: ['png', 'jpeg'], description: 'Encoding (default from config).' },
          max_edge: { type: 'integer', minimum: 0, maximum: 4096, description: 'Longest image edge; 0 keeps the captured size.' },
        },
        additionalProperties: false,
      },
      output: SCREENSHOT_OUTPUT,
      timeoutMs: 45000,
      isConcurrencySafe: () => false,
      presentCall(args) {
        return { card: 'generic', title: `Screenshot ${args?.target ?? 'screen'}`, kind: 'read' };
      },
      async execute(args, exec) {
        try {
          return await captureScreenshot(ctx, helper, config, args ?? {}, exec);
        } catch (error) {
          throw asComputerUseError(error);
        }
      },
    },
    {
      name: 'computer_action',
      description:
        'Control the real Windows mouse and keyboard. Coordinates are in the last screenshot\'s image pixels unless coordinate_space is "screen"; call computer_screenshot first. Pass window_id to focus a window before acting. Actions: move, click, double_click, right_click, middle_click, drag, scroll, key, hotkey, type, wait, clipboard_get, clipboard_set, release_all. Input is real and cannot be undone: take a screenshot after each action to verify. Use type for literal text and hotkey for shortcuts; if a field needs focus, click it first. The emergency hotkey (default Ctrl+Alt+Shift+F12) pauses input.',
      parameters: {
        type: 'object',
        properties: {
          action: { type: 'string', enum: ACTIONS, description: 'The operation to perform.' },
          x: { type: 'number', description: 'Image-space x for move/click/scroll, or the drag start.' },
          y: { type: 'number', description: 'Image-space y for move/click/scroll, or the drag start.' },
          to_x: { type: 'number', description: 'Drag end x in image space.' },
          to_y: { type: 'number', description: 'Drag end y in image space.' },
          button: { type: 'string', enum: ['left', 'right', 'middle'], description: 'Mouse button (default left).' },
          clicks: { type: 'integer', minimum: 1, maximum: 3, description: 'Click count (default 1; double_click implies 2).' },
          hold_ms: { type: 'integer', minimum: 0, maximum: 30000, description: 'Hold the button for this many ms.' },
          duration_ms: { type: 'integer', minimum: 0, maximum: 10000, description: 'Drag duration in ms (default 400).' },
          keys: {
            type: 'array',
            items: { type: 'string' },
            description: 'Key names for key/hotkey, e.g. ["ctrl","shift","t"] or ["enter"].',
          },
          text: { type: 'string', description: 'Literal text for type or clipboard_set.' },
          delta_x: { type: 'integer', minimum: -50, maximum: 50, description: 'Horizontal scroll notches; positive = right.' },
          delta_y: { type: 'integer', minimum: -50, maximum: 50, description: 'Vertical scroll notches; positive = up.' },
          ms: { type: 'integer', minimum: 0, maximum: 30000, description: 'Milliseconds for wait.' },
          window_id: { type: 'string', description: 'Optional window to focus before this action.' },
          coordinate_space: { type: 'string', enum: ['image', 'screen'], description: 'Coordinate space (default image).' },
          interval_ms: { type: 'integer', minimum: 0, maximum: 1000, description: 'Per-character delay for type.' },
        },
        required: ['action'],
        additionalProperties: false,
      },
      output: JSON_OUTPUT,
      timeoutMs: 45000,
      isConcurrencySafe: () => false,
      presentCall(args) {
        return { card: 'generic', title: `Computer ${args?.action ?? 'action'}`, kind: 'execute' };
      },
      async execute(args, exec) {
        try {
          const action = String(args?.action ?? '');
          if (!ACTIONS.includes(action)) throw new ComputerUseError('BAD_ARGS', `unknown action: ${action}`);
          let focused = null;
          if (typeof args?.window_id === 'string' && args.window_id.length > 0) {
            focused = await focusWindow(helper, config, args.window_id, exec);
          }
          const signal = exec.signal;
          const agentId = exec.agent?.id;

          if (action === 'move') {
            const point = resolvePoint(agentId, args);
            if (!point) throw new ComputerUseError('BAD_ARGS', 'move requires x and y');
            await helper.request('mouse.move', pointParams(point), { signal, timeoutMs: 10000 });
            return { ok: true, action, x: point.x, y: point.y, window: focused };
          }

          if (action === 'click' || action === 'double_click' || action === 'right_click' || action === 'middle_click') {
            await assertForegroundAllowed(helper, config, exec);
            const point = resolvePoint(agentId, args);
            const button = action === 'right_click' ? 'right' : action === 'middle_click' ? 'middle' : args?.button ?? 'left';
            const clicks = action === 'double_click' ? 2 : Math.min(3, Math.max(1, args?.clicks ?? 1));
            const holdMs = Math.min(config.input.maxHoldMs, Math.max(0, args?.hold_ms ?? 0));
            const result = await helper.request(
              'mouse.click',
              { ...pointParams(point), button, clicks, hold_ms: holdMs },
              { signal, timeoutMs: 15000 + holdMs },
            );
            return { ok: true, action, ...result, window: focused };
          }

          if (action === 'drag') {
            await assertForegroundAllowed(helper, config, exec);
            const from = resolvePoint(agentId, args);
            if (!from) throw new ComputerUseError('BAD_ARGS', 'drag requires x and y as the start point');
            const to = resolveTo(agentId, args);
            const durationMs = Math.max(0, Math.min(10000, args?.duration_ms ?? 400));
            const result = await helper.request(
              'mouse.drag',
              {
                from_x: from.x,
                from_y: from.y,
                to_x: to.x,
                to_y: to.y,
                button: args?.button ?? 'left',
                duration_ms: durationMs,
                steps: config.input.dragSteps,
              },
              { signal, timeoutMs: 15000 + durationMs },
            );
            return { ok: true, action, ...result, window: focused };
          }

          if (action === 'scroll') {
            await assertForegroundAllowed(helper, config, exec);
            const point = resolvePoint(agentId, args);
            const deltaX = Math.max(-50, Math.min(50, args?.delta_x ?? 0));
            const deltaY = Math.max(-50, Math.min(50, args?.delta_y ?? 0));
            if (deltaX === 0 && deltaY === 0) throw new ComputerUseError('BAD_ARGS', 'scroll requires delta_x or delta_y');
            const result = await helper.request(
              'mouse.scroll',
              { ...pointParams(point), delta_x: deltaX, delta_y: deltaY },
              { signal, timeoutMs: 10000 },
            );
            return { ok: true, action, ...result, window: focused };
          }

          if (action === 'key' || action === 'hotkey') {
            await assertForegroundAllowed(helper, config, exec);
            const keys = Array.isArray(args?.keys) ? args.keys.map(String) : [];
            if (keys.length === 0) throw new ComputerUseError('BAD_ARGS', `${action} requires a non-empty keys array`);
            assertComboAllowed(config, keys);
            const result = await helper.request('key.press', { keys }, { signal, timeoutMs: 10000 });
            return { ok: true, action, ...result, window: focused };
          }

          if (action === 'type') {
            await assertForegroundAllowed(helper, config, exec);
            const text = args?.text;
            if (typeof text !== 'string' || text.length === 0) throw new ComputerUseError('BAD_ARGS', 'type requires non-empty text');
            if (text.length > config.input.maxTextLength) {
              throw new ComputerUseError('BAD_ARGS', `text exceeds input.maxTextLength (${config.input.maxTextLength})`);
            }
            const intervalMs = Math.max(0, Math.min(1000, args?.interval_ms ?? config.input.typeIntervalMs));
            const result = await helper.request('type', { text, interval_ms: intervalMs }, { signal, timeoutMs: 15000 + text.length * intervalMs });
            return { ok: true, action, ...result, window: focused };
          }

          if (action === 'wait') {
            const ms = Math.max(0, Math.min(30000, args?.ms ?? 500));
            await wait(ms, signal);
            return { ok: true, action, ms };
          }

          if (action === 'clipboard_get') {
            const result = await helper.request('clipboard.get', {}, { signal, timeoutMs: 5000 });
            return { ok: true, action, text: result.text };
          }

          if (action === 'clipboard_set') {
            if (typeof args?.text !== 'string') throw new ComputerUseError('BAD_ARGS', 'clipboard_set requires text');
            const result = await helper.request('clipboard.set', { text: args.text }, { signal, timeoutMs: 5000 });
            return { ok: true, action, ...result };
          }

          if (action === 'release_all') {
            const result = await helper.request('input.release_all', {}, { signal, timeoutMs: 5000 });
            return { ok: true, action, ...result };
          }

          throw new ComputerUseError('BAD_ARGS', `unhandled action: ${action}`);
        } catch (error) {
          throw asComputerUseError(error);
        }
      },
    },
    {
      name: 'computer_windows',
      description:
        'List top-level Windows windows with id, title, class, process, pid, bounds, state and whether one is foreground; or focus/change a window. Pass the returned id as window_id to computer_action or computer_screenshot. Focus brings a window forward but keyboard input follows the application\'s focused control: click the target field before typing.',
      parameters: {
        type: 'object',
        properties: {
          action: { type: 'string', enum: ['list', 'focus', 'state'], description: 'Operation to perform.' },
          window_id: { type: 'string', description: 'Window id for focus/state.' },
          state: { type: 'string', enum: ['minimize', 'maximize', 'restore'], description: 'Window state for action=state.' },
        },
        required: ['action'],
        additionalProperties: false,
      },
      output: JSON_OUTPUT,
      timeoutMs: 20000,
      isConcurrencySafe: () => false,
      presentCall(args) {
        return { card: 'generic', title: `Windows ${args?.action ?? 'list'}`, kind: 'read' };
      },
      async execute(args, exec) {
        try {
          const action = String(args?.action ?? '');
          if (action === 'list') {
            const result = await helper.request('windows.list', {}, { signal: exec.signal, timeoutMs: 10000 });
            return { ok: true, action, ...result };
          }
          if (action === 'focus') {
            if (typeof args?.window_id !== 'string') throw new ComputerUseError('BAD_ARGS', 'focus requires window_id');
            const window = await focusWindow(helper, config, args.window_id, exec);
            return { ok: true, action, window: window ?? null };
          }
          if (action === 'state') {
            if (typeof args?.window_id !== 'string') throw new ComputerUseError('BAD_ARGS', 'state requires window_id');
            if (!['minimize', 'maximize', 'restore'].includes(args?.state)) {
              throw new ComputerUseError('BAD_ARGS', 'state must be minimize, maximize or restore');
            }
            const result = await helper.request(
              'window.state',
              { window_id: args.window_id, state: args.state },
              { signal: exec.signal, timeoutMs: 10000 },
            );
            assertProcessAllowed(config, result?.window?.process);
            return { ok: true, action, ...result };
          }
          throw new ComputerUseError('BAD_ARGS', `unknown windows action: ${action}`);
        } catch (error) {
          throw asComputerUseError(error);
        }
      },
    },
  ];
  for (const definition of definitions) ctx.effect(() => ctx.tools.register(definition));
}
