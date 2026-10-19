// AsterConfig.fromEnv / fromFile on the *built* package (plain ESM): runs
// under Node and Bun (where vitest cannot run). An `ASTER_*` variable the
// program assigns to `process.env` at run time must reach the Rust loaders.
// Bun keeps such writes out of the C environment, so a loader reading the
// process environment in Rust would miss them (the server then advertised
// the wrong addresses).
//
//   (cd bindings/typescript/packages/aster && npx tsc)
//   node tests/typescript/smoke/config-env-smoke.mjs
//   bun  tests/typescript/smoke/config-env-smoke.mjs

import { fileURLToPath } from 'node:url';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import * as path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const { AsterConfig } = await import(
  path.join(here, '../../../bindings/typescript/packages/aster/dist/index.js'));

const runtime = typeof Bun !== 'undefined' ? `bun ${Bun.version}` : `node ${process.version}`;
function check(cond, what) {
  if (!cond) throw new Error(`smoke failed (${runtime}): ${what}`);
}

process.env.ASTER_BIND_ADDR = '127.0.0.1:47011';
process.env.ASTER_ADVERTISE_ADDRS = '192.0.2.10:47011';
process.env.ASTER_ENROLLMENT_AUDIENCE = 'smoke-audience';

const env = AsterConfig.fromEnv();
check(env.bindAddr === '127.0.0.1:47011', `fromEnv bindAddr = ${JSON.stringify(env.bindAddr)}`);
check(JSON.stringify(env.advertiseAddrs) === '["192.0.2.10:47011"]',
  `fromEnv advertiseAddrs = ${JSON.stringify(env.advertiseAddrs)}`);
check(env.enrollment?.audience === 'smoke-audience',
  `fromEnv enrollment.audience = ${JSON.stringify(env.enrollment?.audience)}`);

// A deleted variable is unset again.
delete process.env.ASTER_BIND_ADDR;
check(AsterConfig.fromEnv().bindAddr === undefined, 'deleted ASTER_BIND_ADDR still seen');

// fromFile overlays the same runtime variables on the TOML.
const dir = mkdtempSync(path.join(tmpdir(), 'aster-config-smoke-'));
try {
  const file = path.join(dir, 'aster.toml');
  writeFileSync(file, '[network]\nbind_addr = "127.0.0.1:1"\n');
  process.env.ASTER_BIND_ADDR = '127.0.0.1:47012';
  const fromFile = AsterConfig.fromFile(file);
  check(fromFile.bindAddr === '127.0.0.1:47012', `fromFile bindAddr = ${JSON.stringify(fromFile.bindAddr)}`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}

console.log(`config env smoke ok (${runtime})`);
