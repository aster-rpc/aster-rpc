#!/usr/bin/env node
// Regenerate only the npm loader, using the sealed normal exports and the
// pinned generator. First prove baseline regeneration is byte-identical.
import { readFile, mkdtemp, rm, copyFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { writeJsBinding } from '../../bindings/typescript/node_modules/@napi-rs/cli/dist/index.js';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const [input, version, output] = process.argv.slice(2);
if (!input || !output || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version ?? '')) {
  throw Error('usage: napi-loader.mjs <verified index.js> <npm version> <output index.js>');
}
const original = await readFile(input, 'utf8');
const idents = [...original.matchAll(/^module\.exports\.(\w+) = nativeBinding\.\1$/gm)].map(m => m[1]);
if (!idents.length || idents.includes('emitTestLogs') || idents.includes('testEmitted')) {
  throw Error('normal loader has missing exports or unpublished test hooks');
}
const pkg = JSON.parse(await readFile(join(root, 'bindings/typescript/native/package.json'), 'utf8'));
// Package version is stamped later. The baseline is the committed package
// version used by the producer and the generated-files gate.
const baselineMatches = [...original.matchAll(/bindingPackageVersion !== '([^']+)'/g)].map(m => m[1]);
if (!baselineMatches.length || new Set(baselineMatches).size !== 1) throw Error('loader baseline version is ambiguous');
const temporary = await mkdtemp(join(tmpdir(), 'aster-napi-loader-'));
try {
  const options = { platform: true, idents, binaryName: pkg.napi.binaryName, packageName: pkg.name,
    version: baselineMatches[0], outputDir: temporary };
  await writeJsBinding(options);
  if (await readFile(join(temporary, 'index.js'), 'utf8') !== original) {
    throw Error('pinned generator did not reproduce sealed normal loader');
  }
  await writeJsBinding({ ...options, version });
  await copyFile(join(temporary, 'index.js'), output);
} finally {
  await rm(temporary, { recursive: true, force: true });
}
