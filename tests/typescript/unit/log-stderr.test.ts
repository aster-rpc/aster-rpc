// O0 smoke test: ASTER_LOG_STDERR=1 makes loading the native addon install the
// Rust stderr log sink. Runs in a child process: the sink is once per process.
import { describe, it, expect } from 'vitest';
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const addon = resolve(here, '../../../bindings/typescript/native/index.js');

function loadStderr(enable: boolean): string {
  const env = { ...process.env };
  delete env.ASTER_LOG_STDERR;
  delete env.ASTER_LOG;
  if (enable) env.ASTER_LOG_STDERR = '1';
  const res = spawnSync(process.execPath, ['-e', `require(${JSON.stringify(addon)})`], {
    env, encoding: 'utf8', timeout: 60_000,
  });
  expect(res.status, res.stderr).toBe(0);
  return res.stderr;
}

describe('ASTER_LOG_STDERR', () => {
  it('loading the addon installs the stderr sink only when asked', () => {
    createRequire(import.meta.url).resolve(addon); // fail fast if the addon path moved
    const on = loadStderr(true);
    expect(on).toContain('aster::otel');
    expect(on).toContain('stderr logging enabled');
    expect(loadStderr(false)).not.toContain('stderr logging enabled');
  });
});
