/**
 * End-to-end input test: open Notepad, clear the document, type, select all,
 * copy, verify the clipboard, then close without saving.
 *
 * Run it only on a disposable desktop.
 * Usage: node scripts/notepad-test.mjs [path-to-helper.exe]
 */
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import readline from 'node:readline';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const exe = process.argv[2] ?? join(process.env.LOCALAPPDATA ?? root, 'dsh-computer-use', 'helper-test.exe');
if (!existsSync(exe)) {
  console.error(`helper not found: ${exe}`);
  process.exit(2);
}

const helper = spawn(exe, [], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
const rl = readline.createInterface({ input: helper.stdout });
let nextId = 0;
const pending = new Map();
rl.on('line', (line) => {
  const message = JSON.parse(line);
  const entry = pending.get(message.id);
  if (!entry) return;
  pending.delete(message.id);
  if (message.ok) entry.resolve(message.result);
  else entry.reject(Object.assign(new Error(message.error?.message ?? 'helper error'), { code: message.error?.code }));
});
helper.on('exit', (code) => {
  for (const entry of pending.values()) entry.reject(new Error(`helper exited with ${code}`));
  pending.clear();
});
const call = (method, params = {}) => new Promise((resolve, reject) => {
  const id = ++nextId;
  pending.set(id, { resolve, reject });
  helper.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
});

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const text = `dsh computer use smoke ${Date.now()}`;

let notepad;
let previousClipboard = '';
let ok = false;
try {
  previousClipboard = (await call('clipboard.get')).text;
  notepad = spawn('notepad.exe', [], { stdio: 'ignore' });
  await sleep(1600);
  const list = await call('windows.list');
  const byPid = list.windows.find((window) => window.pid === notepad.pid);
  const candidates = list.windows.filter((window) => /notepad/i.test(window.process) || /notepad|记事本/i.test(window.title));
  const target = byPid ?? candidates.find((window) => window.foreground) ?? candidates[0];
  if (!target) throw new Error('Notepad window not found');
  console.log('focus:', target.id, target.title, target.process);
  await call('window.focus', { window_id: target.id });
  await sleep(300);
  const x = target.bounds.x + Math.floor(target.bounds.width / 2);
  const y = target.bounds.y + Math.floor(target.bounds.height / 2);
  await call('mouse.click', { x, y, button: 'left' });
  await sleep(200);
  await call('key.press', { keys: ['ctrl', 'a'] });
  await call('key.press', { keys: ['delete'] });
  await sleep(150);
  await call('type', { text, interval_ms: 0 });
  await sleep(300);
  await call('key.press', { keys: ['ctrl', 'a'] });
  await call('key.press', { keys: ['ctrl', 'c'] });
  await sleep(300);
  const copied = await call('clipboard.get');
  console.log('clipboard:', JSON.stringify(copied.text));
  if (copied.text.trim() !== text) throw new Error(`clipboard mismatch: ${JSON.stringify(copied.text)}`);
  ok = true;
  await call('key.press', { keys: ['alt', 'f4'] });
  await sleep(500);
  try {
    await call('key.press', { keys: ['n'] });
  } catch {}
  await sleep(300);
  console.log('OK');
} catch (error) {
  console.error('FAILED:', error.code ?? '', error.message);
  process.exitCode = 1;
} finally {
  try {
    if (previousClipboard) await call('clipboard.set', { text: previousClipboard });
  } catch {}
  try {
    await call('shutdown');
  } catch {}
  helper.stdin.end();
  setTimeout(() => process.exit(process.exitCode ?? (ok ? 0 : 1)), 400);
}
