/** Capture a screenshot through the helper and return it to the model. */

import { mkdirSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { remember } from './coordinates.js';
import { ComputerUseError } from './errors.js';

const SCREENSHOT_DIRECTORY = join(tmpdir(), 'dsh-computer-use');
let cleaned = false;

function cleanOldScreenshots() {
  if (cleaned) return;
  cleaned = true;
  try {
    const cutoff = Date.now() - 60 * 60 * 1000;
    for (const entry of readdirSync(SCREENSHOT_DIRECTORY)) {
      if (!/^screenshot-.*\.(png|jpe?g)$/u.test(entry)) continue;
      const path = join(SCREENSHOT_DIRECTORY, entry);
      try {
        if (statSync(path).mtimeMs < cutoff) rmSync(path, { force: true });
      } catch {}
    }
  } catch {}
}

function extensionOf(mediaType) {
  return mediaType === 'image/jpeg' ? 'jpg' : 'png';
}

function routeOf(agent) {
  try {
    const routed = agent?.session?.requestHeader?.()?.config;
    return {
      provider: routed?.provider ?? agent?.options?.provider,
      model: routed?.model ?? agent?.options?.model,
    };
  } catch {
    return {};
  }
}

function fallbackFile(data, mediaType) {
  cleanOldScreenshots();
  mkdirSync(SCREENSHOT_DIRECTORY, { recursive: true });
  const path = join(
    SCREENSHOT_DIRECTORY,
    `screenshot-${Date.now()}-${Math.random().toString(36).slice(2, 8)}.${extensionOf(mediaType)}`,
  );
  writeFileSync(path, data);
  return path;
}

/**
 * Capture, persist through the attachment service when possible, and record the
 * coordinate mapping for this agent. The returned value never contains base64.
 */
export async function captureScreenshot(ctx, helper, config, args, exec) {
  const target = args?.target ?? 'screen';
  if (!['screen', 'monitor', 'window', 'region'].includes(target)) {
    throw new ComputerUseError('BAD_ARGS', `target must be screen, monitor, window or region; got ${target}`);
  }
  const format = args?.format ?? config.screenshot.format;
  if (!['png', 'jpeg'].includes(format)) throw new ComputerUseError('BAD_ARGS', 'format must be png or jpeg');
  const maxEdge = args?.max_edge ?? config.screenshot.maxEdge;
  if (!Number.isInteger(maxEdge) || maxEdge < 0 || maxEdge > 4096) {
    throw new ComputerUseError('BAD_ARGS', 'max_edge must be an integer between 0 and 4096');
  }
  const params = {
    target,
    include_cursor: args?.include_cursor ?? config.screenshot.includeCursor,
    format,
    max_edge: maxEdge,
    jpeg_quality: config.screenshot.jpegQuality,
    capture_method: config.screenshot.captureMethod,
  };
  if (args?.monitor !== undefined) {
    if (!Number.isInteger(args.monitor) || args.monitor < 0) throw new ComputerUseError('BAD_ARGS', 'monitor must be a non-negative integer');
    params.monitor = args.monitor;
  }
  if (target === 'window') {
    if (typeof args?.window_id !== 'string' || args.window_id.length === 0) {
      throw new ComputerUseError('BAD_ARGS', 'target=window requires window_id');
    }
    params.window_id = args.window_id;
  }
  if (target === 'region') {
    const region = args?.region;
    if (
      !region ||
      ![region.x, region.y, region.width, region.height].every(Number.isInteger) ||
      region.width <= 0 ||
      region.height <= 0
    ) {
      throw new ComputerUseError('BAD_ARGS', 'target=region requires integer x, y, width and height');
    }
    params.region = { x: region.x, y: region.y, width: region.width, height: region.height };
  }

  const result = await helper.request('screenshot', params, {
    signal: exec.signal,
    timeoutMs: 30000,
  });
  if (typeof result?.data !== 'string' || result.data.length === 0) {
    throw new ComputerUseError('CAPTURE_FAILED', 'the helper returned no image data');
  }
  const data = Buffer.from(result.data, 'base64');
  const mediaType = result.mediaType === 'image/jpeg' ? 'image/jpeg' : 'image/png';
  const capture = result.capture;
  const image = { width: result.width, height: result.height };
  if (!capture || !Number.isFinite(image.width) || !Number.isFinite(image.height)) {
    throw new ComputerUseError('CAPTURE_FAILED', 'the helper returned incomplete screenshot metadata');
  }

  let attachment;
  let mode = 'file';
  let path;
  try {
    const attachments = ctx.get('attachments');
    const llm = ctx.get('llm');
    const { provider, model } = routeOf(exec.agent);
    if (attachments && llm && provider && model) {
      const info = await llm.resolveModelInfo(provider, model, exec.signal);
      if (Array.isArray(info?.inputModalities) && info.inputModalities.includes('image')) {
        const stored = await attachments.saveImage({
          data,
          mediaType,
          name: `computer-screenshot-${Date.now()}.${extensionOf(mediaType)}`,
        });
        attachment = {
          attachmentId: stored.attachmentId,
          mediaType: stored.mediaType,
          bytes: stored.bytes,
          width: stored.width,
          height: stored.height,
          ...(stored.name ? { name: stored.name } : {}),
          ...(stored.originalDimensions
            ? { originalDimensions: { width: stored.originalDimensions.width, height: stored.originalDimensions.height } }
            : {}),
        };
        try {
          const requestTarget = llm.imageRequestTarget(provider, model, attachment);
          const variant = await attachments.readImageRequest(attachment, requestTarget, exec.signal);
          if (Number.isFinite(variant?.width) && Number.isFinite(variant?.height) && variant.width > 0 && variant.height > 0) {
            image.width = variant.width;
            image.height = variant.height;
          }
        } catch {}
        mode = 'image';
      }
    }
  } catch (error) {
    if (exec.signal?.aborted) throw error;
    attachment = undefined;
    mode = 'file';
  }
  if (mode !== 'image') {
    path = fallbackFile(data, mediaType);
  }

  const agentId = exec.agent?.id;
  remember(agentId, {
    capture,
    image: { ...image },
    target,
    windowId: args?.window_id,
    mode,
  });

  const cursorScreen = result.cursor ?? null;
  const cursor = cursorScreen && capture.width > 0 && capture.height > 0
    ? {
        x: Math.round(((cursorScreen.x - capture.x) * image.width) / capture.width),
        y: Math.round(((cursorScreen.y - capture.y) * image.height) / capture.height),
        visible: cursorScreen.visible,
        space: 'image',
      }
    : null;
  const value = {
    mode,
    target,
    capture,
    image: { ...image },
    coordinateSpace: 'image',
    captureMethod: result.captureMethod,
    cursor,
    cursorScreen,
    window: result.window ?? null,
    bytes: data.length,
    at: result.at,
  };
  if (mode === 'image') value.attachment = attachment;
  else value.path = path;
  return value;
}

/** Model-facing content: metadata plus the image itself (or a file fallback). */
export function renderScreenshot(_args, value) {
  const metadata = { ...value };
  delete metadata.attachment;
  const text = [
    'Screenshot captured. Coordinates in computer_action default to this image pixel space.',
    JSON.stringify(metadata),
  ].join('\n');
  if (value.attachment) {
    return [
      { type: 'text', text },
      { type: 'image', attachment: value.attachment },
    ];
  }
  return [
    {
      type: 'text',
      text: `${text}\nThe screenshot could not be attached to the model request. Open it with read_image: ${value.path}`,
    },
  ];
}
