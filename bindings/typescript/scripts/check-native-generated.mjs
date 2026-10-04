#!/usr/bin/env node
// Verify that the committed NAPI-RS outputs (native/index.d.ts and
// native/index.js) are non-empty and match what `napi build` generates
// from the current Rust sources.
//
// Usage (from bindings/typescript/native):
//   node ../scripts/check-native-generated.mjs           # check after a build
//   node ../scripts/check-native-generated.mjs --build   # build, then check
//
// In CI run it after the native build step. Locally, any cargo invocation
// must go through the Kache launcher, e.g.
//   with-kache node ../scripts/check-native-generated.mjs --build

import { execFileSync, spawnSync } from 'node:child_process';
import { readFileSync, statSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const nativeDir = join(dirname(fileURLToPath(import.meta.url)), '..', 'native');
const generated = ['index.d.ts', 'index.js'];

function fail(msg) {
  console.error(`check-native-generated: ${msg}`);
  process.exit(1);
}

if (process.argv.includes('--build')) {
  const res = spawnSync('npx', ['napi', 'build', '--platform', '--release'], {
    cwd: nativeDir,
    stdio: 'inherit',
    shell: process.platform === 'win32',
  });
  if (res.status !== 0) fail(`napi build failed (exit ${res.status})`);
}

for (const file of generated) {
  const path = join(nativeDir, file);
  let size = 0;
  try {
    size = statSync(path).size;
  } catch {
    fail(`${file} is missing; run the native build first`);
  }
  if (size === 0) fail(`${file} is empty; napi build did not emit type definitions`);
}

const dts = readFileSync(join(nativeDir, 'index.d.ts'), 'utf8');
for (const symbol of ['export declare class Node', 'export declare function version']) {
  if (!dts.includes(symbol)) fail(`index.d.ts is missing \`${symbol}\``);
}

const repoRoot = execFileSync('git', ['rev-parse', '--show-toplevel'], {
  cwd: nativeDir,
  encoding: 'utf8',
}).trim();
const paths = generated.map((f) => relative(repoRoot, join(nativeDir, f)));

const tracked = spawnSync('git', ['ls-files', '--error-unmatch', '--', ...paths], {
  cwd: repoRoot,
  stdio: 'ignore',
});
if (tracked.status !== 0) fail(`${paths.join(', ')} must be committed`);

const diff = spawnSync('git', ['diff', '--exit-code', '--stat', '--', ...paths], {
  cwd: repoRoot,
  stdio: 'inherit',
});
if (diff.status !== 0) {
  fail(
    'generated NAPI bindings are out of date. Rebuild in bindings/typescript/native ' +
      '(`bun run build`) and commit index.d.ts and index.js.',
  );
}

console.log('check-native-generated: index.d.ts and index.js are current');
