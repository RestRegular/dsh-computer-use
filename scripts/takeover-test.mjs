/**
 * End-to-end test of the consent takeover: mask overlay, capture exclusion,
 * injected input passing through the low-level hooks, host-initiated release,
 * and the emergency-stop path (which must pause the helper and fail closed).
 *
 * Usage: node scripts/takeover-test.mjs [path-to-helper.exe]
 *
 * The test does not press the real hotkey; it calls `debug.emergency`, which
 * runs the exact code path used by the polling hotkey thread. Run it on a
 * disposable desktop.
 */
import { spawn } from 'node:child_process';
import { existsSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import readline from 'node:readline';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const exe = process.argv[2] ?? join(root, 'native', 'target', 'release', 'dsh-computer-use-helper.exe');
if (!existsSync(exe)) {
  console.error(`helper not found: ${exe}\nRun: npm run build:native`);
  process.exit(2);
}

const child = spawn(exe, [], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
const rl = readline.createInterface({ input: child.stdout });
let nextId = 0;
const pending = new Map();
const events = [];

rl.on('line', (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    console.error('non-JSON stdout:', line);
    return;
  }
  if (typeof message.event === 'string') {
    events.push({ name: message.event, data: message.data, at: Date.now() });
    console.log('event:', message.event, JSON.stringify(message.data));
    return;
  }
  const entry = pending.get(message.id);
  if (!entry) {
    console.error('unexpected frame:', message);
    return;
  }
  pending.delete(message.id);
  if (message.ok) entry.resolve(message.result);
  else entry.reject(Object.assign(new Error(message.error?.message ?? 'helper error'), { code: message.error?.code }));
});
child.stderr.on('data', (chunk) => process.stderr.write(`[helper] ${chunk}`));
child.on('exit', (code) => {
  for (const entry of pending.values()) entry.reject(Object.assign(new Error(`helper exited with ${code}`), { code: 'EXITED' }));
  pending.clear();
});

function call(method, params = {}, timeoutMs = 15000) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(Object.assign(new Error(`${method} timed out`), { code: 'TIMEOUT' }));
    }, timeoutMs);
    pending.set(id, {
      resolve: (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      reject: (error) => {
        clearTimeout(timer);
        reject(error);
      },
    });
    child.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
  });
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function expectError(promise, code) {
  try {
    await promise;
  } catch (error) {
    if (error.code !== code) throw new Error(`expected ${code}, got ${error.code ?? error.message}`);
    return error;
  }
  throw new Error(`expected ${code}, but the call succeeded`);
}

let failed = false;
try {
  const hello = await call('hello');
  console.log('hello:', JSON.stringify({ version: hello.version, paused: hello.paused, takeover: hello.takeover }));

  await call('configure', { require_takeover: true });
  await expectError(call('mouse.move', { x: 10, y: 10 }), 'TAKEOVER_REQUIRED');
  console.log('input without takeover: TAKEOVER_REQUIRED (as expected)');

  const started = await call('takeover.begin', {
    opacity: 0.32,
    title: 'AI 正在操作这台电脑',
    hint: '按 Ctrl+Alt+Shift+F12 立即终止接管',
    banner: true,
  });
  console.log('takeover.begin:', JSON.stringify(started));
  if (!started.active || !started.overlay) throw new Error('the takeover overlay did not start');
  if (!started.captureExcluded) console.warn('WARNING: capture exclusion is unavailable on this machine; screenshots will hide the mask briefly');

  const status = await call('takeover.status');
  if (!status.active) throw new Error('takeover.status does not report an active takeover');

  // Injected input must keep flowing through the physical-input hooks.
  const before = await call('status');
  await call('mouse.move', { x: before.cursor.x + 4, y: before.cursor.y + 4 });
  await sleep(120);
  const after = await call('status');
  if (after.cursor.x === before.cursor.x && after.cursor.y === before.cursor.y) {
    throw new Error('injected mouse input was blocked by the takeover hooks');
  }
  await call('mouse.move', { x: before.cursor.x, y: before.cursor.y });
  console.log('injected input passes through the mask:', `${before.cursor.x},${before.cursor.y}`, '->', `${after.cursor.x},${after.cursor.y}`);

  // A physical mouse move still moves the system cursor before the hook sees
  // it; the takeover must pull it back to where the agent put it.
  await call('mouse.move', { x: 700, y: 500 });
  await sleep(80);
  const pointer = (await call('takeover.status')).pointer;
  if (!pointer) throw new Error('the takeover did not record the agent pointer target');
  await call('debug.cursor_force', { x: 1200, y: 800 });
  await sleep(80);
  const dragged = await call('status');
  await call('debug.pin_post');
  await sleep(120);
  const pinned = await call('status');
  if (pinned.cursor.x !== pointer.x || pinned.cursor.y !== pointer.y) {
    throw new Error(
      `the mask did not pull the cursor back: target ${pointer.x},${pointer.y}, dragged ${dragged.cursor.x},${dragged.cursor.y}, now ${pinned.cursor.x},${pinned.cursor.y}`,
    );
  }
  console.log('physical drag is undone:', `${dragged.cursor.x},${dragged.cursor.y}`, '->', `${pinned.cursor.x},${pinned.cursor.y}`);

  await sleep(500);
  const normal = await call('screenshot', { target: 'screen', format: 'png', max_edge: 1600 });
  writeFileSync(join(root, 'native', 'mask-hidden.png'), Buffer.from(normal.data, 'base64'));
  console.log('screenshot without mask:', `${normal.width}x${normal.height}`);

  const preview = await call('debug.mask_capture', { target: 'screen', format: 'png', max_edge: 1600 });
  writeFileSync(join(root, 'native', 'mask-preview.png'), Buffer.from(preview.data, 'base64'));
  console.log('screenshot with mask:', `${preview.width}x${preview.height}`);

  const ended = await call('takeover.end', { reason: 'test' });
  console.log('takeover.end:', JSON.stringify(ended));
  if (ended.active) throw new Error('the takeover did not stop');
  await expectError(call('mouse.move', { x: 10, y: 10 }), 'TAKEOVER_REQUIRED');

  const again = await call('takeover.begin', { opacity: 0.2 });
  if (!again.active) throw new Error('the second takeover did not start');
  console.log('second takeover:', JSON.stringify(again));

  await call('debug.emergency', { reason: 'hotkey' }, 5000);
  const deadline = Date.now() + 3000;
  while (!events.some((event) => event.name === 'takeover.ended') && Date.now() < deadline) await sleep(50);
  const hotkeyEvent = events.find((event) => event.name === 'takeover.ended');
  if (!hotkeyEvent || hotkeyEvent.data?.reason !== 'hotkey') {
    throw new Error(`expected a takeover.ended(hotkey) event, got ${JSON.stringify(events)}`);
  }
  console.log('emergency event:', JSON.stringify(hotkeyEvent.data));
  await sleep(200);
  const stopped = await call('status');
  if (stopped.takeover?.active) throw new Error('the takeover is still active after the emergency stop');
  if (!stopped.paused) throw new Error('the emergency stop did not pause input');
  await expectError(call('mouse.move', { x: 10, y: 10 }), 'TAKEOVER_ENDED');
  await expectError(call('takeover.begin', { opacity: 0.2 }), 'PAUSED');
  console.log('after emergency: paused + TAKEOVER_ENDED + begin refused (fail closed)');

  await call('shutdown');
  console.log('OK');
} catch (error) {
  failed = true;
  console.error('FAILED:', error.code ?? '', error.message);
  try {
    await call('shutdown');
  } catch {}
} finally {
  child.stdin.end();
  setTimeout(() => process.exit(failed ? 1 : 0), 500).unref();
}
