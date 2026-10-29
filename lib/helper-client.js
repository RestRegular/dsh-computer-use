/** Spawns and talks to the native helper over line-delimited JSON. */

import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import readline from 'node:readline';

import { ComputerUseError } from './errors.js';

const PACKAGED_HELPER = fileURLToPath(new URL('../native/bin/win32-x64/dsh-computer-use-helper.exe', import.meta.url));
const PROTOCOL_VERSION = 1;

export class HelperClient {
  #ctx;
  #config;
  #child = null;
  #starting = null;
  #ready = false;
  #queue = Promise.resolve();
  #pending = new Map();
  #nextId = 0;
  #stderr = [];
  #disposed = false;
  #idleTimer = null;
  #info = null;
  #exe = null;

  constructor(ctx, config) {
    this.#ctx = ctx;
    this.#config = config;
  }

  get info() {
    return this.#info;
  }

  get running() {
    return this.#ready && this.#child !== null;
  }

  log(level, message) {
    const logger = this.#ctx?.logger;
    const sink = logger?.[level];
    if (typeof sink === 'function') sink.call(logger, `[dsh-computer-use] ${message}`);
  }

  #resolveExecutable() {
    if (process.platform !== 'win32') {
      throw new ComputerUseError('UNSUPPORTED_PLATFORM', 'the native helper only supports Windows');
    }
    if (this.#exe !== null) return this.#exe;
    const configured = this.#config.helperPath?.trim();
    if (configured) {
      if (!existsSync(configured)) {
        throw new ComputerUseError('HELPER_MISSING', `configured helperPath does not exist: ${configured}`);
      }
      this.#exe = configured;
      return configured;
    }
    if (!existsSync(PACKAGED_HELPER)) {
      throw new ComputerUseError('HELPER_MISSING', `the packaged helper is missing: ${PACKAGED_HELPER}`);
    }
    // A plugin installed from a sandboxed workspace can inherit a low-integrity
    // label. Copy the helper to LOCALAPPDATA so it runs as a normal desktop
    // process that is allowed to inject input and move the cursor.
    const bytes = readFileSync(PACKAGED_HELPER);
    const hash = createHash('sha256').update(bytes).digest('hex').slice(0, 16);
    const directory = join(process.env.LOCALAPPDATA ?? tmpdir(), 'dsh-computer-use');
    const target = join(directory, `helper-${hash}.exe`);
    try {
      if (!existsSync(target) || statSync(target).size !== bytes.length) {
        mkdirSync(directory, { recursive: true });
        const temporary = `${target}.${process.pid}.tmp`;
        writeFileSync(temporary, bytes, { mode: 0o755 });
        renameSync(temporary, target);
      }
      for (const entry of readdirSync(directory)) {
        if (entry.startsWith('helper-') && entry.endsWith('.exe') && join(directory, entry) !== target) {
          try {
            rmSync(join(directory, entry), { force: true });
          } catch {}
        }
      }
      this.#exe = target;
      return target;
    } catch (error) {
      this.log('warn', `could not stage the helper under LOCALAPPDATA (${error.message}); using the packaged path`);
      this.#exe = PACKAGED_HELPER;
      return PACKAGED_HELPER;
    }
  }

  async ensureStarted() {
    if (this.#disposed) throw new ComputerUseError('DISPOSED', 'the computer-use plugin has been disposed');
    if (this.#ready && this.#child !== null) return;
    if (this.#starting !== null) return this.#starting;
    this.#starting = this.#start().finally(() => {
      this.#starting = null;
    });
    return this.#starting;
  }

  async #start() {
    const executable = this.#resolveExecutable();
    const child = spawn(executable, [], {
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    });
    this.#child = child;
    this.#ready = false;
    this.#stderr = [];
    const output = readline.createInterface({ input: child.stdout });
    output.on('line', (line) => this.#onLine(line));
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => {
      for (const line of String(chunk).split(/\r?\n/u)) {
        const text = line.trim();
        if (text.length === 0) continue;
        this.#stderr.push(text);
        if (this.#stderr.length > 30) this.#stderr.shift();
        if (this.#config.debug.logHelper) this.log('debug', text);
      }
    });
    child.on('exit', (code, signal) => {
      const detail = this.#stderr.length > 0 ? `: ${this.#stderr.slice(-3).join(' | ')}` : '';
      const error = new ComputerUseError(
        'HELPER_EXITED',
        `the native helper exited (code ${code ?? 'null'}${signal ? `, signal ${signal}` : ''})${detail}`,
      );
      this.#child = null;
      this.#ready = false;
      this.#info = null;
      for (const entry of this.#pending.values()) entry.reject(error);
      this.#pending.clear();
      if (!this.#disposed) this.log('warn', error.message);
    });
    child.on('error', (error) => {
      this.log('error', `helper spawn failed: ${error.message}`);
    });
    try {
      const hello = await this.#write('hello', {}, Math.min(5000, this.#config.timeoutMs));
      if (hello?.protocol !== PROTOCOL_VERSION) {
        throw new ComputerUseError(
          'HELPER_PROTOCOL',
          `the helper speaks protocol ${hello?.protocol ?? 'unknown'}; this plugin expects ${PROTOCOL_VERSION}`,
        );
      }
      this.#info = hello;
      this.#ready = true;
      // A previous helper may have been killed while a button was down.
      await this.#write('input.release_all', {}, 1000).catch(() => {});
      const hotkey = this.#config.safety.hotkey;
      const names = typeof hotkey === 'string' ? hotkey.split('+').map((part) => part.trim()).filter(Boolean) : [];
      await this.#write(
        'configure',
        { keys: names, blocked_combos: this.#config.input.blockedCombos },
        1000,
      ).catch(() => {});
      return;
    } catch (error) {
      this.#ready = false;
      try {
        child.kill();
      } catch {}
      this.#child = null;
      throw error instanceof ComputerUseError
        ? error
        : new ComputerUseError('HELPER_START_FAILED', error instanceof Error ? error.message : String(error), { cause: error });
    }
  }

  #onLine(line) {
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      this.log('debug', `non-JSON helper output: ${line.slice(0, 200)}`);
      return;
    }
    const entry = this.#pending.get(message.id);
    if (!entry) {
      this.log('debug', `unmatched helper frame: ${line.slice(0, 200)}`);
      return;
    }
    this.#pending.delete(message.id);
    clearTimeout(entry.timer);
    entry.cleanup();
    if (message.ok) entry.resolve(message.result);
    else {
      entry.reject(
        new ComputerUseError(
          typeof message.error?.code === 'string' ? message.error.code : 'HELPER_ERROR',
          typeof message.error?.message === 'string' ? message.error.message : 'the native helper returned an error',
        ),
      );
    }
  }

  #write(method, params, timeoutMs) {
    const child = this.#child;
    if (child === null || child.stdin.destroyed) {
      return Promise.reject(new ComputerUseError('HELPER_NOT_RUNNING', 'the native helper is not running'));
    }
    return new Promise((resolve, reject) => {
      const id = ++this.#nextId;
      const timer = setTimeout(() => {
        if (!this.#pending.has(id)) return;
        this.#pending.delete(id);
        entry.cleanup();
        this.#kill();
        reject(new ComputerUseError('HELPER_TIMEOUT', `the native helper did not answer ${method} within ${timeoutMs}ms`));
      }, timeoutMs);
      const entry = {
        resolve,
        reject,
        timer,
        cleanup: () => {},
      };
      this.#pending.set(id, entry);
      try {
        child.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
      } catch (error) {
        this.#pending.delete(id);
        clearTimeout(timer);
        reject(new ComputerUseError('HELPER_WRITE_FAILED', error instanceof Error ? error.message : String(error)));
      }
    });
  }

  #kill() {
    const child = this.#child;
    this.#child = null;
    this.#ready = false;
    this.#info = null;
    if (child !== null) {
      try {
        child.kill();
      } catch {}
      try {
        child.stdin.destroy();
      } catch {}
    }
  }

  async request(method, params = {}, options = {}) {
    if (this.#disposed) throw new ComputerUseError('DISPOSED', 'the computer-use plugin has been disposed');
    const run = this.#queue.then(async () => {
      await this.ensureStarted();
      const signal = options.signal;
      if (signal?.aborted) throw new ComputerUseError('ABORTED', 'the call was cancelled');
      const timeoutMs = Math.max(1000, options.timeoutMs ?? this.#config.timeoutMs);
      const onAbort = () => {
        this.#kill();
      };
      signal?.addEventListener('abort', onAbort, { once: true });
      try {
        const result = await this.#write(method, params, timeoutMs);
        this.#touchIdle();
        return result;
      } catch (error) {
        if (signal?.aborted) throw new ComputerUseError('ABORTED', 'the call was cancelled', { cause: error });
        throw error;
      } finally {
        signal?.removeEventListener('abort', onAbort);
      }
    });
    this.#queue = run.catch(() => {});
    return run;
  }

  #touchIdle() {
    if (this.#idleTimer !== null) clearTimeout(this.#idleTimer);
    const idle = this.#config.idleStopMs;
    if (!Number.isFinite(idle) || idle <= 0) return;
    this.#idleTimer = setTimeout(() => {
      this.#idleTimer = null;
      void this.stop({ restartable: true });
    }, idle);
    this.#idleTimer.unref?.();
  }

  async releaseAll() {
    try {
      await this.request('input.release_all', {}, { timeoutMs: 2000 });
    } catch {}
  }

  async status() {
    return this.request('status', {}, { timeoutMs: 5000 });
  }

  async stop({ restartable = false } = {}) {
    if (this.#idleTimer !== null) {
      clearTimeout(this.#idleTimer);
      this.#idleTimer = null;
    }
    if (!restartable) this.#disposed = true;
    const child = this.#child;
    if (child !== null) {
      try {
        await this.#write('shutdown', {}, 750);
      } catch {}
      try {
        child.kill();
      } catch {}
    }
    this.#child = null;
    this.#ready = false;
    this.#info = null;
    for (const entry of this.#pending.values()) {
      clearTimeout(entry.timer);
      entry.reject(new ComputerUseError('HELPER_STOPPED', 'the native helper was stopped'));
    }
    this.#pending.clear();
    if (restartable) this.#disposed = false;
  }
}

