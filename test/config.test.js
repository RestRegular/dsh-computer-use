import test from 'node:test';
import assert from 'node:assert/strict';

import { Config, normalizeConfig } from '../lib/config.js';

test('defaults are complete and stable', () => {
  const config = normalizeConfig({});
  assert.equal(config.enabled, true);
  assert.equal(config.screenshot.format, 'png');
  assert.equal(config.screenshot.maxEdge, 1600);
  assert.deepEqual(config.input.blockedCombos, ['win+l', 'ctrl+alt+del', 'win+u']);
  assert.deepEqual(config.safety.allowedProcesses, []);
  assert.equal(config.input.requireApproval, false);
  // Consent and takeover are on by default: the agent asks before input and
  // the screen is masked while it operates.
  assert.equal(config.consent.requireApproval, true);
  assert.equal(config.consent.scope, 'turn');
  assert.equal(config.takeover.enabled, true);
  assert.equal(config.takeover.opacity, 0.18);
  assert.equal(config.takeover.banner, true);
  assert.equal(config.takeover.chat, true);
  assert.equal(config.takeover.title, '');
  assert.equal(config.takeover.idleEndMs, 0);
});

test('the legacy input.requireApproval still enables consent', () => {
  const config = normalizeConfig({ input: { requireApproval: true } });
  assert.equal(config.consent.requireApproval, true);
  assert.equal(config.input.requireApproval, true);
  const optedOut = normalizeConfig({ consent: { requireApproval: false } });
  assert.equal(optedOut.consent.requireApproval, false);
});

test('takeover and consent overrides are validated', () => {
  const config = normalizeConfig({
    consent: { scope: 'session' },
    takeover: { opacity: 0.4, title: 'Busy', hint: 'Stop with F12', idleEndMs: 60000, banner: false, chat: false },
  });
  assert.equal(config.consent.scope, 'session');
  assert.equal(config.takeover.opacity, 0.4);
  assert.equal(config.takeover.title, 'Busy');
  assert.equal(config.takeover.hint, 'Stop with F12');
  assert.equal(config.takeover.idleEndMs, 60000);
  assert.equal(config.takeover.banner, false);
  assert.equal(config.takeover.chat, false);
  assert.throws(() => normalizeConfig({ takeover: { opacity: 0 } }), /takeover.opacity/);
  assert.throws(() => normalizeConfig({ takeover: { opacity: 1 } }), /takeover.opacity/);
  assert.throws(() => normalizeConfig({ consent: { scope: 'week' } }), /consent.scope/);
  assert.throws(() => normalizeConfig({ takeover: { idleEndMs: -1 } }), /idleEndMs/);
});

test('partial nested overrides keep unrelated defaults', () => {
  const config = normalizeConfig({ screenshot: { format: 'jpeg' }, input: { maxHoldMs: 1000 } });
  assert.equal(config.screenshot.format, 'jpeg');
  assert.equal(config.screenshot.jpegQuality, 85);
  assert.equal(config.input.maxHoldMs, 1000);
  assert.equal(config.input.dragSteps, 24);
});

test('invalid values reject', () => {
  assert.throws(() => normalizeConfig({ screenshot: { format: 'gif' } }), /screenshot.format/);
  assert.throws(() => normalizeConfig({ input: { dragSteps: 0 } }), /dragSteps/);
  assert.throws(() => normalizeConfig({ safety: { hotkey: '' } }), /hotkey/);
  assert.throws(() => normalizeConfig({ input: { blockedCombos: [1] } }), /array of strings/);
});

test('standard schema exposes the normalizer', () => {
  const result = Config['~standard'].validate({ enabled: false });
  assert.equal(result.value.enabled, false);
  assert.equal(result.value.timeoutMs, 15000);
  const failure = Config['~standard'].validate({ screenshot: { format: 'nope' } });
  assert.ok(Array.isArray(failure.issues));
  assert.ok(JSON.parse(JSON.stringify(Config)).dict.enabled);
});

test('every Config node has meta so settings/describe cannot throw', () => {
  const visit = (node) => {
    assert.equal(typeof node.meta, 'object', `missing meta on ${node.type}`);
    for (const child of Object.values(node.dict ?? {})) visit(child);
    if (node.inner) visit(node.inner);
    if (node.items) visit(node.items);
  };
  visit(Config);
  assert.equal(Config.meta.volatile, undefined);
  assert.equal(Config.dict.screenshot.meta.volatile, true);
  const volatileForm = (schema) => {
    if (schema.meta.volatile) return true;
    if (schema.type === 'object') {
      return Object.values(schema.dict ?? {}).some((child) => volatileForm(child));
    }
    return false;
  };
  assert.doesNotThrow(() => volatileForm(Config));
  assert.equal(volatileForm(Config), true);
});
