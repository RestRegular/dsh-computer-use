import test from 'node:test';
import assert from 'node:assert/strict';

import { normalizeConfig } from '../lib/config.js';
import { ComputerControl } from '../lib/control.js';
import { registerApproval } from '../lib/approval.js';

function harness(rawConfig = {}) {
  const config = normalizeConfig(rawConfig);
  const helper = {
    calls: [],
    handlers: new Map(),
    holds: 0,
    async request(method, params = {}) {
      this.calls.push({ method, params });
      if (method === 'takeover.begin') return { active: true, since: 1 };
      return { active: false };
    },
    on(event, handler) {
      let handlers = this.handlers.get(event);
      if (handlers === undefined) {
        handlers = new Set();
        this.handlers.set(event, handlers);
      }
      handlers.add(handler);
      return () => handlers.delete(handler);
    },
    hold() {
      this.holds += 1;
    },
    releaseHold() {
      this.holds -= 1;
    },
  };
  const control = new ComputerControl(null, helper, config);
  const listeners = new Map();
  const ctx = {
    logger: { info() {}, warn() {}, debug() {} },
    on(event, handler) {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    },
  };
  registerApproval(ctx, config, control);
  return { config, control, listeners };
}

const execOf = (action, id = 'agent-a') => ({
  name: 'computer_action',
  arguments: { action },
  agent: { id },
});

const next = async () => ({ kind: 'allow' });

test('the first input action asks for approval with a localized reason', async () => {
  const { listeners } = harness();
  const gate = listeners.get('tools/pre-execute');
  assert.equal(typeof gate, 'function');
  const decision = await gate(execOf('type'), next);
  assert.equal(decision.kind, 'ask');
  assert.match(decision.reason, /takeover/i);
  assert.match(decision.displayReason.en, /Ctrl\+Alt\+Shift\+F12/);
  assert.match(decision.displayReason.zh, /Ctrl\+Alt\+Shift\+F12/);
});

test('read-only and non-input actions are not gated', async () => {
  const { listeners } = harness();
  const gate = listeners.get('tools/pre-execute');
  assert.equal((await gate(execOf('wait'), next)).kind, 'allow');
  assert.equal((await gate(execOf('clipboard_get'), next)).kind, 'allow');
  assert.equal((await gate(execOf('release'), next)).kind, 'allow');
  assert.equal((await gate({ name: 'computer_screenshot', arguments: {}, agent: { id: 'a' } }, next)).kind, 'allow');
});

test('a successful input action approves the rest of the turn', async () => {
  const { listeners } = harness();
  const gate = listeners.get('tools/pre-execute');
  const result = listeners.get('tools/result');
  assert.equal((await gate(execOf('click'), next)).kind, 'ask');
  result(execOf('click'), { isError: false });
  assert.equal((await gate(execOf('type'), next)).kind, 'allow');
  // A different agent still has to ask.
  assert.equal((await gate(execOf('type', 'agent-b'), next)).kind, 'ask');
});

test('a failed action does not grant approval', async () => {
  const { listeners } = harness();
  const gate = listeners.get('tools/pre-execute');
  const result = listeners.get('tools/result');
  result(execOf('click'), { isError: true });
  assert.equal((await gate(execOf('click'), next)).kind, 'ask');
});

test('consent.requireApproval:false disables the gate', () => {
  const { listeners } = harness({ consent: { requireApproval: false } });
  assert.equal(listeners.has('tools/pre-execute'), false);
});
