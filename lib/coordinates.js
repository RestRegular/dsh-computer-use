/** Per-session mapping from the model-visible screenshot pixels to the desktop. */

import { ComputerUseError } from './errors.js';

const mappings = new Map();

export function remember(agentId, mapping) {
  if (!agentId) return;
  mappings.set(String(agentId), mapping);
}

export function forget(agentId) {
  mappings.delete(String(agentId));
}

export function mappingOf(agentId) {
  return mappings.get(String(agentId));
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

/**
 * Convert a screenshot-pixel point into virtual-desktop pixels.
 *
 * The model sees a possibly downscaled image, so coordinate actions use the
 * image's own pixel space unless the caller explicitly asks for screen pixels.
 */
export function resolvePoint(agentId, args) {
  const x = args?.x;
  const y = args?.y;
  if (x === undefined && y === undefined) return undefined;
  if (!Number.isFinite(x) || !Number.isFinite(y)) {
    throw new ComputerUseError('BAD_ARGS', 'x and y must be numbers supplied together');
  }
  const space = args.coordinate_space ?? 'image';
  if (space === 'screen') return { x: Math.round(x), y: Math.round(y), space: 'screen' };
  if (space !== 'image') throw new ComputerUseError('BAD_ARGS', 'coordinate_space must be "image" or "screen"');
  const mapping = mappingOf(agentId);
  if (!mapping) {
    throw new ComputerUseError(
      'SCREENSHOT_REQUIRED',
      'no screenshot is recorded for this session; call computer_screenshot first or set coordinate_space:"screen"',
    );
  }
  if (mapping.image.width <= 0 || mapping.image.height <= 0 || mapping.capture.width <= 0 || mapping.capture.height <= 0) {
    throw new ComputerUseError('SCREENSHOT_REQUIRED', 'the last screenshot has no usable coordinate mapping; take a new screenshot');
  }
  const imageX = clamp(x, 0, mapping.image.width);
  const imageY = clamp(y, 0, mapping.image.height);
  return {
    x: Math.round(mapping.capture.x + (imageX * mapping.capture.width) / mapping.image.width),
    y: Math.round(mapping.capture.y + (imageY * mapping.capture.height) / mapping.image.height),
    space: 'image',
  };
}

export function resolveTo(agentId, args) {
  const x = args?.to_x;
  const y = args?.to_y;
  if (!Number.isFinite(x) || !Number.isFinite(y)) {
    throw new ComputerUseError('BAD_ARGS', 'drag needs numeric to_x and to_y');
  }
  return resolvePoint(agentId, { ...args, x, y });
}

export function describe(agentId) {
  return mappingOf(agentId);
}
