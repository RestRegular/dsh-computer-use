/** Build the Rust helper and stage the executable next to the plugin. */

import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const native = join(root, 'native');
const source = join(native, 'target', 'release', 'dsh-computer-use-helper.exe');
const destination = join(native, 'bin', 'win32-x64', 'dsh-computer-use-helper.exe');

if (process.platform !== 'win32') {
  console.error('the native helper is Windows-only');
  process.exit(2);
}

const result = spawnSync('cargo', ['build', '--release'], { cwd: native, stdio: 'inherit' });
if (result.status !== 0) {
  console.error('cargo build failed');
  process.exit(result.status ?? 1);
}
mkdirSync(dirname(destination), { recursive: true });
copyFileSync(source, destination);
const size = statSync(destination).size;
console.log(`staged ${destination} (${(size / 1024).toFixed(1)} KiB)`);
