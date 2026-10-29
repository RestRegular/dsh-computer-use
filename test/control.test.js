import test from 'node:test';
import assert from 'node:assert/strict';

import { normalizeConfig } from '../lib/config.js';
import { ComputerControl, formatHotkey } from '../lib/control.js';

class FakeHelper {
  calls = [];
  handlers = new Map();
  holds = 0;

  async request(method, params = {}) {
    this.calls.push({ method, params });
    if (method === 'takeover.begin') return { active: true, since: 1700000000000, hotkey: 'ctrl+alt+shift+f12' };
    if (method === 'takeover.end') return { active: false, reason: params.reason };
    return {};
  }

  on(event, handler) {
    let handlers = this.handlers.get(event);
    if (handlers === undefined) {
      handlers = new Set();
      this.handlers.set(event, handlers);
    }
    handlers.add(handler);
    return () => handlers.delete(handler);
  }

  hold() {
    this.holds += 1;
  }

  releaseHold() {
    this.holds -= 1;
  }

  emit(event, data) {
    for (const handler of [...(this.handlers.get(event) ?? [])]) handler(data);
  }
}

const controller = (config = normalizeConfig({}), ctx = null) => {
  const helper = new FakeHelper();
  const control = new ComputerControl(ctx, helper, config);
  control.attach();
  return { helper, control };
};

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function context({ busyEnter = 'queue', agent } = {}) {
  return {
    logger: { info() {}, warn() {}, debug() {} },
    get(name) {
      if (name === 'agents') {
        return { get: (id) => (String(id) === 'agent-a' ? agent : undefined) };
      }
      if (name === 'settings') {
        return { describe: () => [{ ns: 'ui-conversation', value: { busyEnter } }] };
      }
      return undefined;
    },
  };
}

function fakeAgent(status = 'running') {
  return {
    status,
    steers: [],
    followups: [],
    steer(message) {
      this.steers.push(message);
    },
    followup(message) {
      this.followups.push(message);
    },
  };
}

test('formatHotkey renders the banner form', () => {
  assert.equal(formatHotkey('ctrl+alt+shift+f12'), 'Ctrl+Alt+Shift+F12');
  assert.equal(formatHotkey('win+x'), 'Win+X');
  assert.equal(formatHotkey(''), '');
});

test('the first control action starts one takeover and reuses it', async () => {
  const { helper, control } = controller();
  await control.ensure('agent-a');
  await control.ensure('agent-a');
  assert.equal(helper.calls.filter((call) => call.method === 'takeover.begin').length, 1);
  assert.equal(helper.holds, 1);
  assert.equal(control.isActive(), true);
  assert.equal(control.activeAgent(), 'agent-a');
  const begin = helper.calls.find((call) => call.method === 'takeover.begin');
  assert.equal(begin.params.opacity, 0.18);
  assert.ok(begin.params.title.length > 0);
  assert.ok(begin.params.hint.length > 0);
  assert.equal(begin.params.banner, true);
});

test('another agent cannot take over a busy desktop', async () => {
  const { control } = controller();
  await control.ensure('agent-a');
  const error = await control.ensure('agent-b').then(
    () => null,
    (reason) => reason,
  );
  assert.equal(error?.code, 'TAKEOVER_BUSY');
  assert.equal(control.activeAgent(), 'agent-a');
});

test('turn scope expires approval and releases control at turn end', async () => {
  const { helper, control } = controller();
  control.approve('agent-a');
  await control.ensure('agent-a');
  await control.endTurn('agent-a');
  assert.equal(control.isApproved('agent-a'), false);
  assert.equal(control.isActive(), false);
  assert.equal(helper.holds, 0);
  assert.ok(helper.calls.some((call) => call.method === 'takeover.end' && call.params.reason === 'turn-ended'));
});

test('session scope keeps approval across turns but still releases control', async () => {
  const { control } = controller(normalizeConfig({ consent: { scope: 'session' } }));
  control.approve('agent-a');
  await control.ensure('agent-a');
  await control.endTurn('agent-a');
  assert.equal(control.isApproved('agent-a'), true);
  assert.equal(control.isActive(), false);
});

test('an emergency end from the helper revokes approval and drops the hold', async () => {
  const { helper, control } = controller();
  control.approve('agent-a');
  await control.ensure('agent-a');
  helper.emit('takeover.ended', { reason: 'hotkey' });
  assert.equal(control.isActive(), false);
  assert.equal(control.isApproved('agent-a'), false);
  assert.equal(helper.holds, 0);
});

test('a helper crash clears the active takeover', async () => {
  const { helper, control } = controller();
  await control.ensure('agent-a');
  helper.emit('helper-exited', { code: 1 });
  assert.equal(control.isActive(), false);
  assert.equal(helper.holds, 0);
});

test('release hands control back and forgets the approval', async () => {
  const { helper, control } = controller();
  control.approve('agent-a');
  await control.ensure('agent-a');
  assert.equal(await control.release('agent-a'), true);
  assert.equal(control.isActive(), false);
  assert.equal(control.isApproved('agent-a'), false);
  assert.ok(helper.calls.some((call) => call.method === 'takeover.end' && call.params.reason === 'released'));
});

test('disabling takeover leaves input ungated by the mask', async () => {
  const { helper, control } = controller(normalizeConfig({ takeover: { enabled: false } }));
  await control.ensure('agent-a');
  assert.equal(control.isActive(), false);
  assert.equal(helper.calls.length, 0);
});

test('forget stops a takeover owned by a disposed agent', async () => {
  const { control } = controller();
  await control.ensure('agent-a');
  control.forget('agent-a');
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(control.isActive(), false);
});

test('takeover.begin describes the chat panel for the mask', async () => {
  const { helper, control } = controller();
  await control.ensure('agent-a');
  const begin = helper.calls.find((call) => call.method === 'takeover.begin');
  assert.equal(begin.params.chat.title.length > 0, true);
  assert.equal(begin.params.chat.send.length > 0, true);
  assert.equal(begin.params.chat.cancel.length > 0, true);
  assert.equal(begin.params.chat.maxLength, 20000);
  assert.ok(begin.params.hint.includes('输入框'));

  const disabled = controller(normalizeConfig({ takeover: { chat: false } }));
  await disabled.control.ensure('agent-a');
  const plain = disabled.helper.calls.find((call) => call.method === 'takeover.begin');
  assert.equal(plain.params.chat, null);
});

test('composing from the mask panel waits and then releases input', async () => {
  const { helper, control } = controller();
  helper.emit('takeover.composing', { active: true });
  assert.equal(control.isComposing(), true);
  const waiting = control.waitForCompose(undefined, 2000);
  helper.emit('takeover.composing', { active: false });
  assert.equal(await waiting, true);
  assert.equal(control.isComposing(), false);
  assert.equal(await control.waitForCompose(undefined, 5), true);
});

test('waitForCompose times out while the human keeps typing', async () => {
  const { helper, control } = controller();
  helper.emit('takeover.composing', { active: true });
  assert.equal(await control.waitForCompose(undefined, 5), false);
  helper.emit('takeover.composing', { active: false });
});

test('a mask message steers a running session when busyEnter is steer', async () => {
  const agent = fakeAgent('running');
  const { helper, control } = controller(normalizeConfig({}), context({ busyEnter: 'steer', agent }));
  await control.ensure('agent-a', { agent });
  helper.emit('takeover.message', { text: '  look at the dialog  ' });
  await tick();
  assert.equal(agent.steers.length, 1);
  assert.equal(agent.steers[0].content[0].text, 'look at the dialog');
  assert.equal(agent.steers[0].source.kind, 'user');
  assert.equal(typeof agent.steers[0].id, 'string');
  assert.equal(agent.followups.length, 0);
  const ack = helper.calls.find((call) => call.method === 'takeover.chat.ack');
  assert.equal(ack.params.ok, true);
});

test('a mask message queues when the setting says queue or the session is idle', async () => {
  const running = fakeAgent('running');
  const queued = controller(normalizeConfig({}), context({ busyEnter: 'queue', agent: running }));
  await queued.control.ensure('agent-a', { agent: running });
  queued.helper.emit('takeover.message', { text: 'queue this' });
  await tick();
  assert.equal(running.followups.length, 1);
  assert.equal(running.steers.length, 0);

  const idle = fakeAgent('idle');
  const idleCase = controller(normalizeConfig({}), context({ busyEnter: 'steer', agent: idle }));
  await idleCase.control.ensure('agent-a', { agent: idle });
  idleCase.helper.emit('takeover.message', { text: 'next turn' });
  await tick();
  assert.equal(idle.followups.length, 1);
  assert.equal(idle.steers.length, 0);
});

test('a mask message without a live session reports a failed ack', async () => {
  const { helper, control } = controller(normalizeConfig({}), context({ agent: undefined }));
  await control.ensure('agent-a', { agent: undefined });
  helper.emit('takeover.message', { text: 'hello' });
  await tick();
  const ack = helper.calls.find((call) => call.method === 'takeover.chat.ack');
  assert.equal(ack.params.ok, false);
});

test('cancelling from the mask ends the takeover and revokes approval', async () => {
  const { helper, control } = controller();
  control.approve('agent-a');
  await control.ensure('agent-a');
  helper.emit('takeover.ended', { reason: 'cancelled' });
  assert.equal(control.isActive(), false);
  assert.equal(control.isApproved('agent-a'), false);
  assert.equal(helper.holds, 0);
  assert.equal(control.isComposing(), false);
});
