/** Plugin configuration: defaults, validation and a standard-schema descriptor. */

const field = (type, description, extra = {}) => ({
  type,
  meta: { volatile: true, description },
  ...extra,
});

const nested = (description, dict) => ({
  type: 'object',
  meta: { volatile: true, description },
  dict,
});

const FIELDS = {
  enabled: field('boolean', 'Register the computer-use tools. Disable to keep the plugin installed but inactive.'),
  helperPath: field('string', 'Optional explicit path to dsh-computer-use-helper.exe; by default the packaged helper is copied to %LOCALAPPDATA%.'),
  timeoutMs: field('integer', 'Timeout for one native helper request, in milliseconds.', { min: 1000, max: 120000 }),
  idleStopMs: field('integer', 'Stop the native helper after this much idle time, in milliseconds (0 keeps it running).', { min: 0, max: 86400000 }),
  screenshot: nested('Screenshot defaults.', {
    format: field('string', 'Image format returned to the model.', { enum: ['png', 'jpeg'] }),
    jpegQuality: field('integer', 'JPEG quality when format is jpeg.', { min: 40, max: 100 }),
    maxEdge: field('integer', 'Longest image edge before encoding; 0 keeps the captured size.', { min: 0, max: 4096 }),
    includeCursor: field('boolean', 'Draw the mouse cursor into screenshots.'),
    captureMethod: field('string', 'Window capture strategy.', { enum: ['auto', 'gdi', 'printwindow'] }),
  }),
  input: nested('Input defaults and safety policy.', {
    typeIntervalMs: field('integer', 'Delay between typed characters, in milliseconds.', { min: 0, max: 1000 }),
    dragSteps: field('integer', 'Interpolation steps for a drag gesture.', { min: 1, max: 240 }),
    maxHoldMs: field('integer', 'Maximum explicit mouse-button hold, in milliseconds.', { min: 0, max: 30000 }),
    blockedCombos: field('array', 'Key combinations refused by the host, for example ["win+l"].', { items: { type: 'string' } }),
    requireApproval: field('boolean', 'Ask the user before the first input action in each session.'),
    maxTextLength: field('integer', 'Maximum characters accepted by one type action.', { min: 1, max: 200000 }),
  }),
  safety: nested('Emergency stop and process policy.', {
    hotkey: field('string', 'Emergency stop hotkey in the helper (default ctrl+alt+shift+f12).'),
    allowedProcesses: field('array', 'If non-empty, input is only sent to these process names.', { items: { type: 'string' } }),
    blockedProcesses: field('array', 'Input is never sent to these process names.', { items: { type: 'string' } }),
  }),
  debug: nested('Diagnostics.', {
    logHelper: field('boolean', 'Forward native helper stderr to the DSH log.'),
  }),
};

const DEFAULTS = {
  enabled: true,
  helperPath: '',
  timeoutMs: 15000,
  idleStopMs: 300000,
  screenshot: {
    format: 'png',
    jpegQuality: 85,
    maxEdge: 1600,
    includeCursor: true,
    captureMethod: 'auto',
  },
  input: {
    typeIntervalMs: 0,
    dragSteps: 24,
    maxHoldMs: 5000,
    blockedCombos: ['win+l', 'ctrl+alt+del', 'win+u'],
    requireApproval: false,
    maxTextLength: 20000,
  },
  safety: {
    hotkey: 'ctrl+alt+shift+f12',
    allowedProcesses: [],
    blockedProcesses: [],
  },
  debug: {
    logHelper: false,
  },
};

const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

function merge(base, override) {
  if (!isObject(override)) return structuredClone(base);
  const result = structuredClone(base);
  for (const [key, value] of Object.entries(override)) {
    if (isObject(value) && isObject(result[key])) result[key] = merge(result[key], value);
    else if (value !== undefined) result[key] = value;
  }
  return result;
}

function stringArray(value, name, fallback) {
  if (value === undefined) return [...fallback];
  if (!Array.isArray(value) || value.some((entry) => typeof entry !== 'string')) {
    throw new Error(`${name} must be an array of strings`);
  }
  return value.map((entry) => entry.trim().toLowerCase()).filter(Boolean);
}

function oneOf(value, name, allowed) {
  if (!allowed.includes(value)) throw new Error(`${name} must be one of: ${allowed.join(', ')}`);
  return value;
}

function integer(value, name, min, max) {
  if (!Number.isInteger(value) || value < min || value > max) {
    throw new Error(`${name} must be an integer between ${min} and ${max}`);
  }
  return value;
}

export function normalizeConfig(raw) {
  const config = merge(DEFAULTS, raw);
  if (typeof config.enabled !== 'boolean') throw new Error('enabled must be a boolean');
  if (typeof config.helperPath !== 'string') throw new Error('helperPath must be a string');
  config.timeoutMs = integer(config.timeoutMs, 'timeoutMs', 1000, 120000);
  config.idleStopMs = integer(config.idleStopMs, 'idleStopMs', 0, 86400000);
  config.screenshot.format = oneOf(config.screenshot.format, 'screenshot.format', ['png', 'jpeg']);
  config.screenshot.jpegQuality = integer(config.screenshot.jpegQuality, 'screenshot.jpegQuality', 40, 100);
  config.screenshot.maxEdge = integer(config.screenshot.maxEdge, 'screenshot.maxEdge', 0, 4096);
  config.screenshot.includeCursor = Boolean(config.screenshot.includeCursor);
  config.screenshot.captureMethod = oneOf(config.screenshot.captureMethod, 'screenshot.captureMethod', ['auto', 'gdi', 'printwindow']);
  config.input.typeIntervalMs = integer(config.input.typeIntervalMs, 'input.typeIntervalMs', 0, 1000);
  config.input.dragSteps = integer(config.input.dragSteps, 'input.dragSteps', 1, 240);
  config.input.maxHoldMs = integer(config.input.maxHoldMs, 'input.maxHoldMs', 0, 30000);
  config.input.blockedCombos = stringArray(config.input.blockedCombos, 'input.blockedCombos', DEFAULTS.input.blockedCombos);
  config.input.requireApproval = Boolean(config.input.requireApproval);
  config.input.maxTextLength = integer(config.input.maxTextLength, 'input.maxTextLength', 1, 200000);
  config.safety.hotkey = String(config.safety.hotkey).trim().toLowerCase();
  if (config.safety.hotkey.length === 0) throw new Error('safety.hotkey must not be empty');
  config.safety.allowedProcesses = stringArray(config.safety.allowedProcesses, 'safety.allowedProcesses', []);
  config.safety.blockedProcesses = stringArray(config.safety.blockedProcesses, 'safety.blockedProcesses', []);
  config.debug.logHelper = Boolean(config.debug.logHelper);
  return config;
}

function descriptor(node) {
  return {
    ...node,
    toJSON() {
      return JSON.parse(JSON.stringify({ ...this, toJSON: undefined }));
    },
  };
}

export const Config = Object.assign(descriptor({ type: 'object', dict: FIELDS }), {
  '~standard': {
    version: 1,
    vendor: 'dsh-computer-use',
    validate(value) {
      try {
        return { value: normalizeConfig(value) };
      } catch (error) {
        return { issues: [{ message: error instanceof Error ? error.message : String(error) }] };
      }
    },
  },
});
