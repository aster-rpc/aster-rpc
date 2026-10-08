/**
 * Typed loader for the NAPI-RS addon (`@aster-rpc/transport`).
 *
 * The addon is a CommonJS package; this module is ESM, so it resolves the
 * addon through `createRequire(import.meta.url)`. That works on plain Node
 * ESM as well as Bun and bundlers. The type of the addon comes from the
 * `index.d.ts` that `napi build` generates next to the native loader.
 */

import { createRequire } from 'node:module';
import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import type * as NativeModule from '@aster-rpc/transport';

/** The full NAPI addon surface, typed from the generated `index.d.ts`. */
export type NativeAddon = typeof NativeModule;

/** A CommonJS `require` scoped to this module (ESM has no global one). */
export const requireCjs = createRequire(import.meta.url);

const PLATFORM_BINARIES = [
  'aster-transport.darwin-arm64.node',
  'aster-transport.darwin-x64.node',
  'aster-transport.linux-x64-gnu.node',
  'aster-transport.linux-arm64-gnu.node',
  'aster-transport.win32-x64-msvc.node',
];

let _native: NativeAddon | null = null;
let _attempted = false;

function tryLoad(path: string): NativeAddon | null {
  try {
    const addon = requireCjs(path) as Partial<NativeAddon> | null;
    // A loadable NAPI module may still be unrelated or too old for this SDK.
    // Reject it before callers try to construct a missing native handle.
    if (!addon || typeof addon.Node !== 'function'
      || typeof addon.RpcConnection !== 'function'
      || typeof addon.DispatchServer !== 'function') return null;
    return addon as NativeAddon;
  } catch {
    return null;
  }
}

/**
 * Load the native addon, or return `null` if it can't be found.
 *
 * Resolution order:
 *   1. the `@aster-rpc/transport` package (workspace link or npm install);
 *   2. a `.node` binary in `$ASTER_NATIVE_PATH`;
 *   3. a `.node` binary in the usual monorepo locations.
 */
export function tryLoadNative(): NativeAddon | null {
  if (_native || _attempted) return _native;
  _attempted = true;

  _native = tryLoad('@aster-rpc/transport');
  if (_native) return _native;

  const searchDirs: string[] = [];
  const envPath = process.env.ASTER_NATIVE_PATH;
  if (envPath) searchDirs.push(envPath);
  const thisDir = dirname(fileURLToPath(import.meta.url));
  searchDirs.push(resolve(thisDir, '../../../native')); // packages/aster/{src,dist} -> native
  searchDirs.push(resolve(thisDir, '../../native'));
  searchDirs.push(resolve(process.cwd(), 'bindings/typescript/native'));
  searchDirs.push(resolve(process.cwd(), 'node_modules/@aster-rpc/transport'));

  for (const dir of searchDirs) {
    for (const name of PLATFORM_BINARIES) {
      const full = resolve(dir, name);
      if (existsSync(full)) {
        _native = tryLoad(full);
        if (_native) return _native;
      }
    }
  }
  return null;
}

/** Load the native addon, throwing a descriptive error if unavailable. */
export function loadNative(): NativeAddon {
  const native = tryLoadNative();
  if (!native) {
    throw new Error(
      'Aster native addon (@aster-rpc/transport) is not available. ' +
      'Install it, build it with `bun run build` in bindings/typescript/native, ' +
      'or point ASTER_NATIVE_PATH at a directory containing the .node binary.',
    );
  }
  return native;
}
