import test from 'node:test';
import assert from 'node:assert/strict';

import { normalizeConfig } from '../lib/config.js';
import { ComputerControl } from '../lib/control.js';
import { registerApproval } from '../lib/approval.js';

function harness(rawConfig = {}, services = {}) {
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
    get(name) {
      return services[name];
    },
    on(event, handler) {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    },
  };
  registerApproval(ctx, config, control);
  return { config, control, listeners };
}

const execOf = (action, id = 'agent-a', session = undefined) => ({
  name: 'computer_action',
  arguments: { action },
  agent: { id, session },
});

const next = async () => ({ kind: 'allow' });

/** Minimal stand-in for the harness approval service. */
const approvalService = (policy) => ({
  effectivePolicy() {
    return policy;
  },
});

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

test('full access (approval policy "never") runs input actions without a prompt', async () => {
  const session = { id: 'session-a' };
  const { listeners } = harness({}, { approval: approvalService('never') });
  const gate = listeners.get('tools/pre-execute');
  assert.equal((await gate(execOf('type', 'agent-a', session), next)).kind, 'allow');
  assert.equal((await gate(execOf('click', 'agent-a', session), next)).kind, 'allow');
});

test('an "ask" session still gets the approval prompt', async () => {
  const { listeners } = harness({}, { approval: approvalService('ask') });
  const gate = listeners.get('tools/pre-execute');
  const session = { id: 'session-a' };
  assert.equal((await gate(execOf('type', 'agent-a', session), next)).kind, 'ask');
});

test('unknown approval state still asks (fail closed)', async () => {
  // No approval service and no session: the gate cannot prove full access.
  const { listeners } = harness();
  const gate = listeners.get('tools/pre-execute');
  assert.equal((await gate(execOf('type'), next)).kind, 'ask');
  // A session whose policy cannot be read also asks.
  const broken = harness({}, { approval: { effectivePolicy: () => undefined } });
  const gateBroken = broken.listeners.get('tools/pre-execute');
  assert.equal((await gateBroken(execOf('type', 'agent-a', { id: 'session-a' }), next)).kind, 'ask');
});

test('full access is also read from the permissions projection', async () => {
  const session = { id: 'session-a' };
  const { listeners } = harness({}, {
    sessionProjections: { stateOf: () => ({ preset: 'danger-full-access', sandbox: 'danger-full-access', approval: 'never' }) },
  });
  const gate = listeners.get('tools/pre-execute');
  assert.equal((await gate(execOf('type', 'agent-a', session), next)).kind, 'allow');
});
