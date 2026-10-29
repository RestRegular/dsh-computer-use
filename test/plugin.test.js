import test from 'node:test';
import assert from 'node:assert/strict';

import { apply, Config } from '../index.js';

test('apply wires tools, guidance, approval, takeover lifecycle and provider', { skip: process.platform !== 'win32' }, () => {
  const tools = [];
  const listeners = new Map();
  const sections = [];
  let providerRegistered = 0;
  const logger = { info() {}, warn() {}, debug() {}, error() {} };
  const ctx = {
    logger,
    effect(fn) {
      return fn();
    },
    get() {
      return undefined;
    },
    on(event, handler) {
      listeners.set(event, handler);
      return () => listeners.delete(event);
    },
    inject(services, callback) {
      if (services.includes('systemPrompt')) {
        callback({
          systemPrompt: {
            getSectionOrder() {
              return 7;
            },
            section(section) {
              sections.push(section);
            },
          },
        });
      }
      if (services.includes('computerUse')) {
        callback({
          effect(fn) {
            return fn();
          },
          computerUse: {
            register(name) {
              providerRegistered += 1;
              assert.equal(name, 'windows-native');
              return () => {};
            },
          },
          logger,
        });
      }
    },
    tools: {
      register(definition) {
        tools.push(definition);
        return () => {};
      },
    },
  };

  assert.doesNotThrow(() => apply(ctx, {}));
  assert.deepEqual(
    tools.map((tool) => tool.name),
    ['computer_screenshot', 'computer_action', 'computer_windows'],
  );
  assert.equal(sections.length, 1);
  assert.equal(sections[0].order, 7);
  assert.equal(providerRegistered, 1);
  for (const event of ['tools/pre-execute', 'tools/result', 'agent/turn-stopping', 'agent/disposed']) {
    assert.equal(typeof listeners.get(event), 'function', `missing listener for ${event}`);
  }
  assert.ok(Config.dict.consent);
  assert.ok(Config.dict.takeover);
});
