/**
 * Standalone smoke test for the native helper.
 *
 * Usage: node scripts/smoke.mjs [path-to-helper.exe]
 *
 * It exercises the protocol, a screenshot, window enumeration, a small mouse
 * round-trip, and the clipboard. The cursor is restored and the previous text
 * clipboard value is put back when it was readable.
 */
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
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

rl.on('line', (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    console.error('non-JSON stdout:', line);
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
  for (const entry of pending.values()) entry.reject(new Error(`helper exited with ${code}`));
  pending.clear();
});

function call(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, { resolve, reject });
    child.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
  });
}

try {
  const hello = await call('hello');
  console.log('hello:', JSON.stringify({
    version: hello.version,
    protocol: hello.protocol,
    elevated: hello.elevated,
    secureDesktop: hello.secureDesktop,
    paused: hello.paused,
    hotkey: hello.hotkey,
    monitors: hello.monitors?.monitors?.length,
  }));

  const shot = await call('screenshot', { target: 'screen', format: 'png', max_edge: 1600 });
  const output = join(root, 'native', 'smoke-screenshot.png');
  mkdirSync(dirname(output), { recursive: true });
  writeFileSync(output, Buffer.from(shot.data, 'base64'));
  console.log('screenshot:', `${shot.width}x${shot.height}`, `${shot.bytes} bytes`, shot.captureMethod, '->', output);

  const windows = await call('windows.list');
  console.log('windows:', windows.count, 'foreground:', windows.foreground, '|', windows.windows[0]?.title);

  const before = await call('status');
  const origin = before.cursor;
  await call('mouse.move', { x: origin.x + 3, y: origin.y + 3 });
  const moved = await call('status');
  await call('mouse.move', { x: origin.x, y: origin.y });
  console.log('mouse:', `${origin.x},${origin.y}`, '->', `${moved.cursor.x},${moved.cursor.y}`, '-> restored');

  let previousClipboard = '';
  try {
    previousClipboard = (await call('clipboard.get')).text;
  } catch {}
  await call('clipboard.set', { text: 'dsh-computer-use smoke test' });
  const clipboard = await call('clipboard.get');
  console.log('clipboard:', JSON.stringify(clipboard.text));
  if (previousClipboard) await call('clipboard.set', { text: previousClipboard });

  const status = await call('status');
  console.log('status:', JSON.stringify({
    paused: status.paused,
    secureDesktop: status.secureDesktop,
    foreground: status.foreground?.title,
  }));
  await call('shutdown');
  console.log('OK');
} catch (error) {
  console.error('FAILED:', error.code ?? '', error.message);
  process.exitCode = 1;
} finally {
  child.stdin.end();
  setTimeout(() => child.kill(), 500).unref();
}
