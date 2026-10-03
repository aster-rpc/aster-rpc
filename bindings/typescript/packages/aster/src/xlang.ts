/**
 * xlang.ts -- the one place the TypeScript binding configures Apache Fory.
 *
 * Mirrors `core/src/xlang.rs::new_fory`: every Aster binding pins the same
 * Fory release and the same options for call payloads:
 *
 * | option     | value | why |
 * |------------|-------|-----|
 * | xlang      | on    | cross-language wire format (Fory JS is xlang-only) |
 * | compatible | on    | named-compatible structs; field add/remove tolerated |
 * | ref        | off   | no shared/cyclic graphs on the wire |
 *
 * The RPC envelope (`StreamHeader` / `CallHeader` / `RpcStatus`) is **not**
 * Fory-encoded; see `protocol.ts` (AEv1, Aster-SPEC §6.9).
 *
 * @example
 * ```ts
 * import { createXlangCodec } from '@aster-rpc/aster';
 * const codec = createXlangCodec();
 * ```
 */

import { ForyCodec } from './codec.js';
import { requireCjs } from './native.js';

/**
 * The Fory release Aster pins across bindings (Rust `fory-core`, pyfory,
 * `@apache-fory/core`). The installed npm package may lag by a patch
 * release while it clears the dependency cooldown; see package.json.
 */
export const FORY_VERSION = '1.7.6';

/** The options every Aster Fory runtime is built with. */
export const FORY_OPTIONS = Object.freeze({ ref: false, compatible: true });

// Cached Fory instance and Type - shared across all modules
let _cachedFory: any | undefined;
let _cachedType: any | undefined;
let _cachedCodec: ForyCodec | undefined;

function loadFory(): { Fory: any; Type: any } {
  // @apache-fory/core is CommonJS; load it through createRequire.
  const foryModule = requireCjs('@apache-fory/core');
  return { Fory: foryModule.default, Type: foryModule.Type };
}

/**
 * Get the shared Fory instance and Type, creating them if needed.
 * All modules should use this instead of creating their own Fory instances.
 */
export function getXlangForyAndType(): { fory: any; Type: any } {
  if (_cachedFory) return { fory: _cachedFory, Type: _cachedType! };
  const { Fory, Type } = loadFory();
  _cachedFory = new Fory({ ...FORY_OPTIONS });
  _cachedType = Type;
  return { fory: _cachedFory, Type };
}

/**
 * Build a fresh, uncached Fory instance + Type helper. Primarily for tests
 * that need isolation between register calls; production code should use
 * {@link getXlangForyAndType}.
 */
export function newXlangFory(): { fory: any; Type: any } {
  const { Fory, Type } = loadFory();
  return { fory: new Fory({ ...FORY_OPTIONS }), Type };
}

/**
 * Create the payload codec: a {@link ForyCodec} over a Fory runtime with
 * Aster's standard options.
 *
 * When called without arguments, returns a codec over the shared cached
 * Fory instance (client-side usage). When called with a Fory instance,
 * wraps that one (server-side, where the same instance must also receive
 * the generated `BUILD_ALL_TYPES` registrations).
 *
 * @param fory - Optional pre-created Fory instance (from getXlangForyAndType)
 */
export function createXlangCodec(fory?: any, _Type?: any): ForyCodec {
  if (!fory) {
    if (_cachedCodec) return _cachedCodec;
    _cachedCodec = new ForyCodec(getXlangForyAndType().fory);
    return _cachedCodec;
  }
  return new ForyCodec(fory);
}
