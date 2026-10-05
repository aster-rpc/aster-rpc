import { existsSync, statSync } from 'node:fs';
import * as path from 'node:path';

/** Select the coordination voter executable used by native integration tests. */
export function coordinationFixture(repo: string): string | undefined {
  const explicit = process.env.ASTER_COORDINATION_FIXTURE;
  if (explicit) {
    if (!statSync(explicit, { throwIfNoEntry: false })?.isFile()) {
      throw new Error(`ASTER_COORDINATION_FIXTURE is not a file: ${explicit}`);
    }
    return explicit;
  }
  const exe = process.platform === 'win32' ? '.exe' : '';
  const target = process.env.CARGO_TARGET_DIR ?? path.join(repo, 'target');
  return [
    path.join(target, 'debug', `aster-coordination-fixture${exe}`),
    path.join(target, 'release', `aster-coordination-fixture${exe}`),
  ]
    .filter((p) => existsSync(p))
    .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
}
