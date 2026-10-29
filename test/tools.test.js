import test from 'node:test';
import assert from 'node:assert/strict';

import { normalizeConfig } from '../lib/config.js';
import { registerTools } from '../lib/tools.js';

function harness(config = normalizeConfig({})) {
  const definitions = [];
  const ctx = {
    logger: { info() {}, warn() {}, debug() {} },
    get() {
      return undefined;
    },
    effect(fn) {
      fn();
    },
    tools: {
      register(definition) {
        definitions.push(definition);
        return () => {};
      },
    },
  };
  const requests = [];
  const helper = {
    async request(method, params = {}) {
      requests.push({ method, params });
      return { ok: true };
    },
  };
  const control = {
    ensured: [],
    released: [],
    async ensure(agentId) {
      this.ensured.push(String(agentId));
    },
    async release(agentId) {
      this.released.push(String(agentId));
      return true;
    },
    isActive() {
      return true;
    },
  };
  registerTools(ctx, helper, config, control);
  const action = definitions.find((definition) => definition.name === 'computer_action');
  return { action, requests, control, definitions };
}

const exec = () => ({
  agent: { id: 'agent-a' },
  signal: new AbortController().signal,
});

test('registers the three tools', () => {
  const { definitions } = harness();
  assert.deepEqual(
    definitions.map((definition) => definition.name),
    ['computer_screenshot', 'computer_action', 'computer_windows'],
  );
});

test('control actions start the takeover before injecting input', async () => {
  const { action, requests, control } = harness();
  await action.execute({ action: 'click', x: 10, y: 20, coordinate_space: 'screen' }, exec());
  assert.deepEqual(control.ensured, ['agent-a']);
  assert.deepEqual(
    requests.map((request) => request.method),
    ['mouse.click'],
  );

  await action.execute({ action: 'type', text: 'hello' }, exec());
  assert.deepEqual(control.ensured, ['agent-a', 'agent-a']);
  assert.equal(requests.at(-1).method, 'type');
});

test('wait, clipboard and release do not start the mask', async () => {
  const { action, requests, control } = harness();
  await action.execute({ action: 'wait', ms: 1 }, exec());
  await action.execute({ action: 'clipboard_set', text: 'x' }, exec());
  await action.execute({ action: 'release_all' }, exec());
  assert.deepEqual(control.ensured, []);
  assert.equal(control.released.length, 0);
  assert.deepEqual(
    requests.map((request) => request.method),
    ['clipboard.set', 'input.release_all'],
  );
});

test('release reports the takeover state and hands control back', async () => {
  const { action, requests, control } = harness();
  const result = await action.execute({ action: 'release' }, exec());
  assert.deepEqual(control.released, ['agent-a']);
  assert.equal(control.ensured.length, 0);
  assert.deepEqual(result, { ok: true, action: 'release', released: true, takeover: { active: true } });
  assert.deepEqual(requests, []);
});
