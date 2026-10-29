/**
 * Consent and takeover bookkeeping for one shared Windows desktop.
 *
 * The approval gate decides whether the agent may drive this computer, and this
 * controller turns the first approved input into a native takeover: the helper
 * puts a semi-transparent mask over the screen and routes the human's physical
 * input to the mask's chat panel until the turn ends, the agent releases
 * control, the human clicks Cancel takeover, or they press the emergency
 * hotkey. Messages typed into that panel are delivered to the requesting
 * session with the same busy-Enter behavior as the conversation composer.
 */

import { randomUUID } from 'node:crypto';

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

/** Labels for the interactive chat panel drawn inside the mask. */
const CHAT_LABELS = {
  zh: {
    title: '给 Agent 发消息',
    hint: 'Enter 发送 · Shift+Enter 换行 · Esc 收起输入',
    send: '发送',
    cancel: '取消接管',
    idle: (strategy) => `点击输入框发消息 · 发送方式：${strategy}`,
    composing: '输入中 · Enter 发送',
    sending: '发送中…',
    sent: '已发送',
    failed: '发送失败',
    waiting: 'Agent 正在操作，请稍候…',
    empty: '消息不能为空',
    focusFailed: '无法聚焦输入框，请再点一次',
    queue: '排队',
    steer: '插话',
  },
  en: {
    title: 'Message the agent',
    hint: 'Enter sends · Shift+Enter for a new line · Esc dismisses',
    send: 'Send',
    cancel: 'Cancel takeover',
    idle: (strategy) => `Click the box to type · delivery: ${strategy}`,
    composing: 'Typing · Enter sends',
    sending: 'Sending…',
    sent: 'Sent',
    failed: 'Send failed',
    waiting: 'The agent is working; one moment…',
    empty: 'The message is empty',
    focusFailed: 'Could not focus the input; click it again',
    queue: 'queue',
    steer: 'steer',
  },
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

function defaultHint(hotkey, chat) {
  if (preferredLocale() === 'zh') {
    return chat
      ? `按 ${hotkey} 立即终止接管 · 点击下方输入框给 Agent 发消息`
      : `按 ${hotkey} 立即终止接管`;
  }
  return chat
    ? `Press ${hotkey} to take back control · click the panel below to message the agent`
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
  #composing = false;
  #composeWaiters = [];

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

  /** True while the human is typing into the takeover mask's chat panel. */
  isComposing() {
    return this.#composing;
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
    this.#listeners.push(
      this.#helper.on('takeover.composing', (data) => this.#setComposing(Boolean(data?.active))),
    );
    this.#listeners.push(
      this.#helper.on('takeover.message', (data) => {
        void this.#deliverMessage(data);
      }),
    );
  }

  /**
   * Wait until the human stops typing in the mask chat panel. `computer_action`
   * uses this before driving the pointer/keyboard: while the panel owns the
   * foreground focus, injected input would land in the panel instead of the app.
   * @returns true when input may resume, false on timeout or abort.
   */
  waitForCompose(signal, timeoutMs = 30000) {
    if (!this.#composing) return Promise.resolve(true);
    if (signal?.aborted) return Promise.resolve(false);
    return new Promise((resolve) => {
      let timer = null;
      const finish = (value) => {
        if (timer !== null) clearTimeout(timer);
        signal?.removeEventListener?.('abort', onAbort);
        const index = this.#composeWaiters.indexOf(waiter);
        if (index >= 0) this.#composeWaiters.splice(index, 1);
        resolve(value);
      };
      const onAbort = () => finish(false);
      const waiter = { finish };
      this.#composeWaiters.push(waiter);
      timer = setTimeout(() => finish(false), Math.max(0, Number(timeoutMs) || 0));
      timer.unref?.();
      signal?.addEventListener?.('abort', onAbort, { once: true });
    });
  }

  #setComposing(value) {
    if (this.#composing === value) return;
    this.#composing = value;
    if (value) {
      // Typing is human activity: do not let the idle timer end the mask while
      // the human is in the middle of a message.
      this.#clearIdle();
      return;
    }
    for (const waiter of this.#composeWaiters.splice(0)) waiter.finish(true);
    if (this.#active !== null) this.#touchIdle();
  }

  #agents() {
    try {
      return this.#ctx?.get?.('agents');
    } catch {
      return undefined;
    }
  }

  /** Resolve the live agent that owns the active takeover. */
  #agent(agentId) {
    try {
      return this.#agents()?.get?.(String(agentId));
    } catch {
      return undefined;
    }
  }

  /** The busy-Enter preference from the session UI settings (default queue). */
  #busyEnter() {
    try {
      const settings = this.#ctx?.get?.('settings');
      if (typeof settings?.describe !== 'function') return 'queue';
      const descriptor = settings
        .describe({ redactSecrets: true })
        .find((row) => row?.ns === 'ui-conversation');
      return descriptor?.value?.busyEnter === 'steer' ? 'steer' : 'queue';
    } catch {
      return 'queue';
    }
  }

  /**
   * Deliver one message typed into the mask to the session that requested the
   * takeover, following the same busy-Enter behavior as the conversation
   * composer: Queue when idle or when the setting says queue, Steer while busy
   * when the setting says steer.
   */
  async #deliverMessage(data) {
    const text = String(data?.text ?? '').trim();
    const active = this.#active;
    if (active === null) {
      await this.#ack(false, 'the takeover has already ended');
      return;
    }
    if (text.length === 0) {
      await this.#ack(false, 'the message is empty');
      return;
    }
    if (text.length > this.#config.input.maxTextLength) {
      await this.#ack(false, `the message exceeds input.maxTextLength (${this.#config.input.maxTextLength})`);
      return;
    }
    const agent = active.agent ?? this.#agent(active.agentId);
    if (agent === undefined) {
      await this.#ack(false, 'the session that requested the takeover is not available');
      return;
    }
    const running = typeof agent.status === 'string' ? agent.status !== 'idle' : true;
    const mode = running ? this.#busyEnter() : 'queue';
    try {
      const message = Object.freeze({
        id: randomUUID(),
        role: 'user',
        content: Object.freeze([{ type: 'text', text }]),
        source: { kind: 'user' },
      });
      if (mode === 'steer') agent.steer(message);
      else agent.followup(message);
      this.log('info', `message from the takeover mask delivered to ${active.agentId} (${mode}, ${text.length} chars)`);
      await this.#ack(true, '');
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.log('warn', `could not deliver the message from the takeover mask: ${message}`);
      await this.#ack(false, message);
    }
  }

  #ack(ok, note) {
    return Promise.resolve(
      this.#helper.request(
        'takeover.chat.ack',
        { ok: Boolean(ok), note: String(note ?? '').slice(0, 300) },
        { timeoutMs: 3000 },
      ),
    ).catch(() => {});
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
    this.#composing = false;
    for (const waiter of this.#composeWaiters.splice(0)) waiter.finish(false);
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
    // While the human types in the mask panel it owns the foreground focus;
    // injected input would go into the panel, so wait until they finish.
    await this.waitForCompose(exec?.signal);
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

  /** Labels for the chat panel drawn inside the mask, or null when disabled. */
  #chatPayload() {
    if (this.#config.takeover.chat === false) return null;
    const labels = CHAT_LABELS[preferredLocale()] ?? CHAT_LABELS.en;
    const strategy = this.#busyEnter();
    return {
      title: labels.title,
      hint: labels.hint,
      send: labels.send,
      cancel: labels.cancel,
      idle: labels.idle(labels[strategy]),
      composing: labels.composing,
      sending: labels.sending,
      sent: labels.sent,
      failed: labels.failed,
      waiting: labels.waiting,
      empty: labels.empty,
      focusFailed: labels.focusFailed,
      maxLength: this.#config.input.maxTextLength,
    };
  }

  async #begin(agentId, exec) {
    const hotkey = this.hotkey;
    const title = String(this.#config.takeover.title ?? '').trim() || defaultTitle();
    const hint = String(this.#config.takeover.hint ?? '').trim() || defaultHint(hotkey, this.#config.takeover.chat !== false);
    const chat = this.#chatPayload();
    const result = await this.#helper.request(
      'takeover.begin',
      {
        opacity: this.#config.takeover.opacity,
        title,
        hint,
        banner: this.#config.takeover.banner !== false,
        chat,
      },
      { signal: exec?.signal, timeoutMs: 8000 },
    );
    if (!result?.active) {
      throw new ComputerUseError('TAKEOVER_FAILED', 'the native helper did not report an active takeover');
    }
    this.#active = {
      agentId,
      agent: exec?.agent,
      since: Number.isFinite(result.since) ? result.since : Date.now(),
      title,
    };
    this.#helper.hold();
    this.#touchIdle();
    this.log(
      'info',
      `takeover active for ${agentId}: ${result.chat ? 'the chat panel lets the human message this session or cancel' : 'physical mouse/keyboard is discarded'}, emergency stop is ${hotkey}`,
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
    this.#composing = false;
    for (const waiter of this.#composeWaiters.splice(0)) waiter.finish(false);
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
    this.#composing = false;
    for (const waiter of this.#composeWaiters.splice(0)) waiter.finish(false);
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
