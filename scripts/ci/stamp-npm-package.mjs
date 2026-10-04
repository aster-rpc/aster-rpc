#!/usr/bin/env node
// Stamp an npm package.json in a disposable CI tree.
//
//   node scripts/ci/stamp-npm-package.mjs <package.json> <version> [registry]
//
// Sets `version`, pins every `@aster-rpc/*` dependency and optional dependency
// to the same version (the loader and its platform packages ship in lockstep),
// and, when a registry URL is given, points `publishConfig.registry` at it so a
// development build cannot be published to npmjs by accident.
import { readFileSync, writeFileSync } from 'node:fs';

const [path, version, registry] = process.argv.slice(2);
if (!path || !version) {
  console.error('usage: stamp-npm-package.mjs <package.json> <version> [registry]');
  process.exit(64);
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error(`stamp-npm-package: '${version}' is not a semver version`);
  process.exit(64);
}

const pkg = JSON.parse(readFileSync(path, 'utf8'));
pkg.version = version;
for (const group of ['dependencies', 'optionalDependencies']) {
  for (const name of Object.keys(pkg[group] ?? {})) {
    if (name.startsWith('@aster-rpc/')) pkg[group][name] = version;
  }
}
if (registry) {
  pkg.publishConfig = { ...(pkg.publishConfig ?? {}), registry };
}
writeFileSync(path, JSON.stringify(pkg, null, 2) + '\n');
console.log(`stamped ${path} -> ${pkg.name}@${version}${registry ? ` (${registry})` : ''}`);
