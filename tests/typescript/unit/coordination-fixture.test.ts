import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { mkdirSync, mkdtempSync, rmSync, utimesSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { coordinationFixture } from '../helpers/coordination-fixture.ts';

let repo: string;
const exe = process.platform === 'win32' ? '.exe' : '';

function fixture(name: string, modified: number): string {
  const file = path.join(repo, name);
  mkdirSync(path.dirname(file), { recursive: true });
  writeFileSync(file, 'fixture placeholder');
  utimesSync(file, modified, modified);
  return file;
}

beforeEach(() => {
  repo = mkdtempSync(path.join(tmpdir(), 'aster-fixture-selector-'));
  vi.stubEnv('ASTER_COORDINATION_FIXTURE', '');
  vi.stubEnv('CARGO_TARGET_DIR', path.join(repo, 'target'));
});

afterEach(() => {
  vi.unstubAllEnvs();
  rmSync(repo, { recursive: true, force: true });
});

it('uses the explicitly selected artifact even when a cached build is newer', () => {
  const selected = fixture(`pinned/fixture${exe}`, 100);
  fixture(`target/debug/aster-coordination-fixture${exe}`, 200);
  vi.stubEnv('ASTER_COORDINATION_FIXTURE', selected);
  expect(coordinationFixture(repo)).toBe(selected);
});

it('refuses a missing explicit artifact instead of silently testing a cached build', () => {
  fixture(`target/debug/aster-coordination-fixture${exe}`, 200);
  vi.stubEnv('ASTER_COORDINATION_FIXTURE', path.join(repo, 'missing'));
  expect(() => coordinationFixture(repo)).toThrow(/ASTER_COORDINATION_FIXTURE/);
});

it('uses the newest local build when no artifact was explicitly selected', () => {
  fixture(`target/debug/aster-coordination-fixture${exe}`, 100);
  const latest = fixture(`target/release/aster-coordination-fixture${exe}`, 200);
  expect(coordinationFixture(repo)).toBe(latest);
});

it('reports an unavailable fixture when no override or local build exists', () => {
  expect(coordinationFixture(repo)).toBeUndefined();
});
