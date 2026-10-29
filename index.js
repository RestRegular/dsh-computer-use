/**
 * DSH Computer Use: Windows screenshots, mouse, keyboard, windows and clipboard.
 *
 * The Host plugin owns three tools and a small JSONL client. The actual Win32
 * work happens in a separate native helper so a capture or drag cannot block
 * the harness event loop and a helper crash cannot take down the Host.
 */

import { Config, normalizeConfig } from './lib/config.js';
import { HelperClient } from './lib/helper-client.js';
import { registerTools } from './lib/tools.js';
import { registerGuidance } from './lib/prompt.js';
import { registerApproval } from './lib/approval.js';

export const name = 'dsh-computer-use';
export const inject = ['tools'];
export { Config };

/**
 * @param {import('@deepseek-ai/cordis').Context} ctx
 * @param {Record<string, unknown>} rawConfig
 */
export function apply(ctx, rawConfig) {
  const config = normalizeConfig(rawConfig ?? {});
  if (process.platform !== 'win32') {
    ctx.logger?.warn?.('[dsh-computer-use] Windows is required; tools are not registered');
    return;
  }
  if (!config.enabled) {
    ctx.logger?.info?.('[dsh-computer-use] disabled by configuration');
    return;
  }

  const helper = new HelperClient(ctx, config);
  ctx.effect(() => () => helper.stop());

  registerTools(ctx, helper, config);
  registerGuidance(ctx, config);
  registerApproval(ctx, config);

  // Register as the optional shared computer-use provider when the official
  // registry happens to be mounted. A second provider is a profile conflict;
  // keep this plugin's own tools available instead of failing activation.
  ctx.inject(['computerUse'], (scope) => {
    scope.effect(() => {
      try {
        return scope.computerUse.register('windows-native');
      } catch (error) {
        scope.logger?.warn?.(
          `[dsh-computer-use] another computer-use provider is already registered: ${error instanceof Error ? error.message : String(error)}`,
        );
        return () => {};
      }
    });
  });

  ctx.logger?.info?.('[dsh-computer-use] Windows native computer-use tools are ready');
}
