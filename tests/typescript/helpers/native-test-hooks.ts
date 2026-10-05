/**
 * Test-only native hooks. The published addon does not export them: they are
 * compiled in by `bun run build:test-hooks` (cargo feature `test-hooks` of
 * `aster-transport-napi`), which the TypeScript test legs run after the normal
 * build. They are not part of any package's API or typings.
 */

import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.ts';

export interface NativeTestHooks {
  /** Emit `count` Rust events on target `aster::o7test` from a background thread. */
  emitTestLogs(count: number, intervalUs?: number, level?: number, prefix?: string): void;
  /** Records `emitTestLogs` has emitted so far (process-wide). */
  testEmitted(): number;
}

/** The addon with the test hooks, or an error saying how to build them. */
export function nativeTestHooks(): NativeTestHooks {
  const native = loadNative() as unknown as Partial<NativeTestHooks>;
  if (typeof native.emitTestLogs !== 'function' || typeof native.testEmitted !== 'function') {
    throw new Error(
      'the native addon was built without the test hooks: run `bun run build:test-hooks` in bindings/typescript/native',
    );
  }
  return native as NativeTestHooks;
}
