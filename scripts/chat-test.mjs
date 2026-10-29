/**
 * End-to-end test of the interactive takeover chat panel: the mask's drawn
 * pointer, entering compose mode (the panel takes the keyboard), gating of
 * injected input while the human types, sending a message to the host, the
 * ack path and the cancel button.
 *
 * Usage: node scripts/chat-test.mjs [path-to-helper.exe]
 *
 * Run it on a disposable desktop: entering compose mode takes the foreground
 * briefly. The helper must be copied to %LOCALAPPDATA% first (a helper started
 * from the low-integrity workspace cannot inject input / focus windows).
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

async function waitFor(check, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await check()) return;
    await sleep(50);
  }
  throw new Error(`timed out waiting for ${label}`);
}

async function expectError(promise, code) {
  try {
    await promise;
  } catch (error) {
    if (error.code !== code) throw new Error(`expected ${code}, got ${error.code ?? error.message}`);
    return error;
  }
  throw new Error(`expected ${code}, but the call succeeded`);
}

const labels = {
  title: '给 Agent 发消息',
  hint: 'Enter 发送 · Shift+Enter 换行 · Esc 收起输入',
  send: '发送',
  cancel: '取消接管',
  idle: '点击输入框发消息 · 发送方式：排队',
  composing: '输入中 · Enter 发送',
  sending: '发送中…',
  sent: '已发送',
  failed: '发送失败',
  waiting: 'Agent 正在操作，请稍候…',
  empty: '消息不能为空',
  focusFailed: '无法聚焦输入框，请再点一次',
  maxLength: 2000,
};

let failed = false;
let inTakeover = false;
try {
  const hello = await call('hello');
  console.log('hello:', JSON.stringify({ version: hello.version, paused: hello.paused }));

  await call('configure', { require_takeover: true });
  const started = await call('takeover.begin', {
    opacity: 0.28,
    title: 'AI 正在操作这台电脑',
    hint: '按 Ctrl+Alt+Shift+F12 立即终止接管 · 点击下方输入框给 Agent 发消息',
    banner: true,
    chat: labels,
  });
  inTakeover = true;
  console.log('takeover.begin:', JSON.stringify(started));
  if (!started.active || !started.chat) throw new Error('the takeover chat panel did not start');

  await sleep(400);
  const status = await call('takeover.status');
  if (!status.chat?.visible) throw new Error('takeover.status does not report a visible chat panel');
  if (status.composing) throw new Error('the chat panel unexpectedly starts in compose mode');
  const beforeCompose = await call('status');
  const previousForeground = beforeCompose.foreground?.id;
  if (!previousForeground) throw new Error('status does not report the foreground window');

  const panel = status.chat.bounds;
  if (!panel || panel.width <= 0) throw new Error('takeover.status does not report the chat panel bounds');
  console.log('panel bounds:', JSON.stringify(panel));

  // A normal model screenshot must not contain the mask, the panel or the
  // simulated pointer (WDA_EXCLUDEFROMCAPTURE), unlike debug.mask_capture.
  const hidden = await call('screenshot', { target: 'screen', format: 'png', max_edge: 1600 });
  writeFileSync(join(root, 'native', 'chat-hidden.png'), Buffer.from(hidden.data, 'base64'));
  console.log('normal screenshot written:', `${hidden.width}x${hidden.height}`);

  // The mask screenshot must include the panel and the drawn pointer (this is
  // the debug capture mode that disables WDA_EXCLUDEFROMCAPTURE).
  await call('debug.chat', { action: 'pointer', x: panel.x + 40, y: panel.y + 30 });
  await sleep(150);
  const preview = await call('debug.mask_capture', { target: 'screen', format: 'png', max_edge: 1600 });
  writeFileSync(join(root, 'native', 'chat-preview.png'), Buffer.from(preview.data, 'base64'));
  console.log('chat preview written:', `${preview.width}x${preview.height}`);

  // Enter compose mode through the same path a physical click takes.
  await call('debug.chat', { action: 'compose' });
  await waitFor(
    async () => (await call('status')).foreground?.class === 'DshComputerUseTakeoverChat',
    5000,
    'compose mode foreground',
  );
  if (!(await call('takeover.status')).composing) throw new Error('compose mode is not active');
  console.log('compose mode is active (the panel owns the keyboard focus)');

  await expectError(call('mouse.move', { x: 10, y: 10 }), 'HUMAN_COMPOSING');
  console.log('injected input is gated with HUMAN_COMPOSING while typing');

  await call('debug.chat', { action: 'text', text: '来自蒙版的消息' });
  await sleep(150);
  const typed = await call('takeover.status');
  if (typed.chat?.textLength !== '来自蒙版的消息'.length) {
    throw new Error(`the message box did not accept the text (textLength=${typed.chat?.textLength})`);
  }
  console.log('message box text length:', typed.chat.textLength);

  // Click the Send button the way the human's drawn pointer would.
  const sendX = panel.x + panel.width - 18 - 92 / 2;
  const sendY = panel.y + 118 + 17;
  await call('debug.chat', { action: 'click', x: sendX, y: sendY });
  await waitFor(() => events.some((event) => event.name === 'takeover.message'), 5000, 'takeover.message');
  const sent = events.find((event) => event.name === 'takeover.message');
  if (sent.data?.text !== '来自蒙版的消息') throw new Error(`unexpected message event: ${JSON.stringify(sent.data)}`);
  console.log('takeover.message event:', JSON.stringify(sent.data));

  const afterSend = await call('takeover.status');
  if (afterSend.composing) throw new Error('the panel did not leave compose mode after sending');
  if (!afterSend.chat?.pendingSend) throw new Error('the panel does not show a pending send');
  await waitFor(async () => (await call('status')).foreground?.id === previousForeground, 3000, 'focus restore after send');
  console.log('focus returned to the previous window after send');

  await call('takeover.chat.ack', { ok: true, note: '' });
  await waitFor(async () => {
    const current = await call('takeover.status');
    return current.chat?.textLength === 0 && current.chat?.pendingSend === false;
  }, 5000, 'send ack');
  console.log('ack cleared the message box');

  // Enter in the message box must send without clicking the button.
  await call('debug.chat', { action: 'compose' });
  await waitFor(
    async () => (await call('status')).foreground?.class === 'DshComputerUseTakeoverChat',
    5000,
    'enter-compose foreground',
  );
  await call('debug.chat', { action: 'text', text: '第二条消息' });
  const beforeEnter = events.filter((event) => event.name === 'takeover.message').length;
  await call('debug.chat', { action: 'press', keys: ['enter'] });
  await waitFor(
    () => events.filter((event) => event.name === 'takeover.message').length > beforeEnter,
    5000,
    'takeover.message from Enter',
  );
  const enterMessage = events.filter((event) => event.name === 'takeover.message').at(-1);
  if (enterMessage.data?.text !== '第二条消息') throw new Error(`unexpected Enter message: ${JSON.stringify(enterMessage.data)}`);
  await call('takeover.chat.ack', { ok: true, note: '' });
  await waitFor(
    async () => {
      const current = await call('takeover.status');
      return current.chat?.textLength === 0 && current.chat?.pendingSend === false;
    },
    5000,
    'Enter ack',
  );
  console.log('Enter in the message box sent the message');

  // Click the Cancel takeover button (the same hit-testing path).
  const cancelX = panel.x + panel.width - 18 - 92 - 10 - 112 / 2;
  const cancelY = panel.y + 118 + 17;
  await call('debug.chat', { action: 'click', x: cancelX, y: cancelY });
  await waitFor(async () => !(await call('takeover.status')).active, 5000, 'cancel to end the takeover');
  inTakeover = false;
  const cancelled = events.find((event) => event.name === 'takeover.ended' && event.data?.reason === 'cancelled');
  if (!cancelled) throw new Error(`no takeover.ended(cancelled) event: ${JSON.stringify(events)}`);
  console.log('cancel button ended the takeover:', JSON.stringify(cancelled.data));
  await expectError(call('mouse.move', { x: 10, y: 10 }), 'TAKEOVER_ENDED');

  // A host-initiated end while the human is typing must not hang and must
  // restore the foreground window.
  const second = await call('takeover.begin', { opacity: 0.2, chat: labels });
  inTakeover = true;
  if (!second.active) throw new Error('the second takeover did not start');
  const nextForeground = (await call('status')).foreground?.id;
  await call('debug.chat', { action: 'compose' });
  await waitFor(
    async () => (await call('status')).foreground?.class === 'DshComputerUseTakeoverChat',
    5000,
    'second compose mode foreground',
  );
  const ended = await call('takeover.end', { reason: 'test-end' }, 8000);
  inTakeover = false;
  if (ended.active) throw new Error('takeover.end did not stop the second takeover');
  await waitFor(async () => {
    const current = await call('status');
    return !current.takeover?.active && current.takeover?.composing === false;
  }, 5000, 'takeover.end while composing');
  await waitFor(async () => (await call('status')).foreground?.id === nextForeground, 3000, 'focus restore after end');
  console.log('takeover.end while composing restored focus');

  await call('shutdown');
  console.log('OK');
} catch (error) {
  failed = true;
  console.error('FAILED:', error.code ?? '', error.message);
  try {
    if (inTakeover) await call('takeover.end', { reason: 'test-error' }, 5000);
    await call('shutdown');
  } catch {}
} finally {
  child.stdin.end();
  setTimeout(() => process.exit(failed ? 1 : 0), 500).unref();
}
