/**
 * Consent and takeover bookkeeping for one shared Windows desktop.
 *
 * The approval gate decides whether the agent may drive this computer, and this
 * controller turns the first approved input into a native takeover: the helper
 * puts a semi-transparent mask over the screen and discards physical mouse and
 * keyboard input until the turn ends, the agent releases control, or the human
 * presses the emergency hotkey.
 */

import { ComputerUseError } from './errors.js';

/** computer_action actions that change the computer and need user approval. */
export const INPUT_ACTIONS = new Set([
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

/** The subset that physically drives the pointer/keyboard: those take over. */
export const CONTROL_ACTIONS = new Set([
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
]);

const DEFAULT_TITLE = {
  zh: 'AI 正在操作这台电脑',
  en: 'AI is operating this computer',
};

function preferredLocale() {
  try {
    return /^zh/iu.test(Intl.DateTimeFormat().resolvedOptions().locale ?? '') ? 'zh' : 'en';
  } catch {
    return 'en';
  }
}

/** `ctrl+alt+shift+f12` -> `Ctrl+Alt+Shift+F12`, the way the banner shows it. */
export function formatHotkey(hotkey) {
  const parts = String(hotkey ?? '')
    .split('+')
    .map((part) => part.trim())
    .filter(Boolean);
  return parts
    .map((part) => (part.length === 1 ? part.toUpperCase() : part.charAt(0).toUpperCase() + part.slice(1)))
    .join('+');
}

function defaultTitle() {
  return DEFAULT_TITLE[preferredLocale()] ?? DEFAULT_TITLE.en;
}

function defaultHint(hotkey) {
  return preferredLocale() === 'zh'
    ? `按 ${hotkey} 立即终止接管`
    : `Press ${hotkey} to take back control`;
}

export class ComputerControl {
  #ctx;
  #config;
  #helper;
  #approved = new Set();
  #active = null;
  #starting = null;
  #idleTimer = null;
  #listeners = [];

  constructor(ctx, helper, config) {
    this.#ctx = ctx;
    this.#helper = helper;
    this.#config = config;
  }

  get enabled() {
    return this.#config.takeover.enabled;
  }

  get hotkey() {
    return formatHotkey(this.#config.safety.hotkey);
  }

  isActive() {
    return this.#active !== null;
  }

  activeAgent() {
    return this.#active?.agentId ?? null;
  }

  isApproved(agentId) {
    return agentId !== undefined && this.#approved.has(String(agentId));
  }

  approve(agentId) {
    if (agentId === undefined) return;
    this.#approved.add(String(agentId));
  }

  revoke(agentId) {
    if (agentId === undefined) return;
    this.#approved.delete(String(agentId));
  }

  log(level, message) {
    const logger = this.#ctx?.logger;
    const sink = logger?.[level];
    if (typeof sink === 'function') sink.call(logger, `[dsh-computer-use] ${message}`);
  }

  /** Listen for helper-initiated ends (emergency hotkey, crash). */
  attach() {
    this.#listeners.push(
      this.#helper.on('takeover.ended', (data) => this.#endedExternally(data)),
    );
    this.#listeners.push(
      this.#helper.on('helper-exited', (data) => this.#endedExternally({ reason: 'helper-exited', ...data })),
    );
  }

  /**
   * End the takeover for one external reason. Input is paused by the helper on
   * an emergency stop; approval is revoked either way so continuing needs a new
   * human decision.
   */
  #endedExternally(data) {
    const active = this.#active;
    if (active === null) return;
    this.#active = null;
    this.#clearIdle();
    this.#helper.releaseHold();
    this.#approved.delete(active.agentId);
    this.log('warn', `takeover ended externally (${data?.reason ?? 'unknown'}); approval for ${active.agentId} revoked`);
  }

  /**
   * Make sure the mask and the input hooks are up before a control action.
   * Approval is required by the tools/pre-execute gate; this only starts the
   * native side after the turn has been approved.
   */
  async ensure(agentId, exec) {
    if (!this.enabled || agentId === undefined) return;
    const id = String(agentId);
    if (this.#active?.agentId === id) {
      this.#touchIdle();
      return;
    }
    if (this.#active !== null) {
      throw new ComputerUseError(
        'TAKEOVER_BUSY',
        `another agent (${this.#active.agentId}) is controlling this computer; wait until it releases control`,
      );
    }
    if (this.#starting !== null) {
      await this.#starting.catch(() => {});
      if (this.#active?.agentId === id) {
        this.#touchIdle();
        return;
      }
      if (this.#active !== null) {
        throw new ComputerUseError(
          'TAKEOVER_BUSY',
          `another agent (${this.#active.agentId}) is controlling this computer; wait until it releases control`,
        );
      }
    }
    this.#starting = this.#begin(id, exec);
    try {
      await this.#starting;
    } finally {
      this.#starting = null;
    }
  }

  async #begin(agentId, exec) {
    const hotkey = this.hotkey;
    const title = String(this.#config.takeover.title ?? '').trim() || defaultTitle();
    const hint = String(this.#config.takeover.hint ?? '').trim() || defaultHint(hotkey);
    const result = await this.#helper.request(
      'takeover.begin',
      {
        opacity: this.#config.takeover.opacity,
        title,
        hint,
        banner: this.#config.takeover.banner !== false,
      },
      { signal: exec?.signal, timeoutMs: 8000 },
    );
    if (!result?.active) {
      throw new ComputerUseError('TAKEOVER_FAILED', 'the native helper did not report an active takeover');
    }
    this.#active = {
      agentId,
      since: Number.isFinite(result.since) ? result.since : Date.now(),
      title,
    };
    this.#helper.hold();
    this.#touchIdle();
    this.log(
      'info',
      `takeover active for ${agentId}: physical mouse/keyboard is discarded, emergency stop is ${hotkey}`,
    );
  }

  #touchIdle() {
    this.#clearIdle();
    const ms = this.#config.takeover.idleEndMs;
    if (!Number.isFinite(ms) || ms <= 0) return;
    this.#idleTimer = setTimeout(() => {
      this.#idleTimer = null;
      void this.end('idle');
    }, ms);
    this.#idleTimer.unref?.();
  }

  #clearIdle() {
    if (this.#idleTimer !== null) {
      clearTimeout(this.#idleTimer);
      this.#idleTimer = null;
    }
  }

  async end(reason = 'host') {
    const active = this.#active;
    if (active === null) return false;
    this.#active = null;
    this.#clearIdle();
    this.#helper.releaseHold();
    try {
      await this.#helper.request('takeover.end', { reason }, { timeoutMs: 6000 });
    } catch (error) {
      this.log('warn', `could not stop the takeover cleanly: ${error instanceof Error ? error.message : String(error)}`);
    }
    this.log('info', `takeover ended (${reason})`);
    return true;
  }

  /**
   * End the turn for one agent: the turn approval expires and its takeover is
   * released. Other agents' takeovers are untouched.
   */
  async endTurn(agentId) {
    const id = String(agentId);
    if (this.#config.consent.scope === 'turn') this.#approved.delete(id);
    if (this.#active?.agentId === id) await this.end('turn-ended');
  }

  /** Forget everything about an agent that was disposed. */
  forget(agentId) {
    const id = String(agentId);
    this.#approved.delete(id);
    if (this.#active?.agentId === id) void this.end('agent-disposed');
  }

  /**
   * Return control to the human from the model-facing `release` action. The
   * approval is discarded so a later action asks the human again.
   */
  async release(agentId) {
    if (agentId === undefined) return false;
    const id = String(agentId);
    this.#approved.delete(id);
    if (this.#active?.agentId !== id) return false;
    return this.end('released');
  }

  dispose() {
    for (const off of this.#listeners.splice(0)) {
      try {
        off();
      } catch {}
    }
    this.#clearIdle();
    this.#approved.clear();
    const active = this.#active;
    this.#active = null;
    if (active !== null) {
      this.#helper.releaseHold();
      void this.#helper
        .request('takeover.end', { reason: 'disposed' }, { timeoutMs: 2000 })
        .catch(() => {});
    }
  }
}

/**
 * Expire per-turn approval and release control when an agent's turn closes or
 * the agent goes away. `agent/turn-stopping` is serial, so the mask is gone
 * before the turn actually ends.
 */
export function registerControlLifecycle(ctx, control) {
  ctx.on('agent/turn-stopping', async (payload) => {
    const agent = payload?.agent;
    if (agent?.id === undefined) return;
    await control.endTurn(String(agent.id));
  });
  ctx.on('agent/disposed', (payload) => {
    const agent = payload?.agent;
    if (agent?.id === undefined) return;
    control.forget(String(agent.id));
  });
}
