/**
 * xlang.ts -- XLANG codec factory for cross-language interop.
 *
 * Creates a ForyCodec pre-configured with the Aster protocol types
 * (StreamHeader, CallHeader, RpcStatus) with explicit field types
 * that match the Python pyfory type annotations exactly.
 *
 * @example
 * ```ts
 * import { createXlangCodec } from '@aster-rpc/aster';
 * const codec = createXlangCodec();
 * const transport = new IrohTransport(connection, codec);
 * ```
 */

import { ForyCodec } from './codec.js';
import { StreamHeader, CallHeader, RpcStatus } from './protocol.js';

// Cached Fory instance and Type - shared across all modules
let _cachedFory: any | undefined;
let _cachedType: any | undefined;
let _cachedCodec: ForyCodec | undefined;

/**
 * Get the shared Fory instance and Type, creating them if needed.
 * All modules should use this instead of creating their own Fory instances.
 */
export function getXlangForyAndType(): { fory: any; Type: any } {
  if (_cachedFory) return { fory: _cachedFory, Type: _cachedType! };
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const foryModule = require('@apache-fory/core');
  const Fory = foryModule.default;
  const Type = foryModule.Type;
  // `compatible: true` picks NAMED_COMPATIBLE_STRUCT layout (what
  // pyfory / fory-java use by default at XLANG); `ref: true` matches
  // `pyfory.Fory(xlang=True, ref=True)` and Fory Java's
  // `withRefTracking(true)`. Stable `@apache-fory/core@0.17.0` reads
  // this as `config.ref` (flowed into `typeResolver.trackingRef`);
  // the dev source in docs/_internal uses `refTracking` but the
  // release ships `ref`. Config silently drops unknown keys, so the
  // name matters -- `refTracking: true` on stable 0.17.0 is a no-op
  // and wire bytes come out with NotNullValueFlag (0xff) instead of
  // RefValueFlag (0x00), mismatching pyfory/fory-java.
  _cachedFory = new Fory({ ref: true, compatible: true });
  _cachedType = Type;
  return { fory: _cachedFory, Type };
}

/**
 * Build a fresh, uncached Fory instance + Type helper. Primarily for tests
 * that need isolation between register calls; production code should use
 * {@link getXlangForyAndType}.
 */
export function newXlangFory(): { fory: any; Type: any } {
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const foryModule = require('@apache-fory/core');
  const Fory = foryModule.default;
  const Type = foryModule.Type;
  return { fory: new Fory({ ref: true, compatible: true }), Type };
}

/**
 * Create a ForyCodec pre-configured for cross-language (XLANG) interop.
 *
 * Registers the Aster protocol types (StreamHeader, CallHeader, RpcStatus)
 * with explicit int8/int16/int32 field types matching the IDL spec.
 *
 * Requires `@apache-fory/core` to be installed.
 *
 * When called without arguments, uses a cached Fory instance shared across
 * all calls (suitable for client-side usage). When called with arguments,
 * uses the provided Fory instance (suitable for server-side where the same
 * Fory instance must be shared with BUILD_ALL_TYPES).
 *
 * @param fory - Optional pre-created Fory instance (from getXlangForyAndType)
 * @param Type - Optional Type namespace from @apache-fory/core
 */
export function createXlangCodec(fory?: any, Type?: any): ForyCodec {
  // If called with no args and we have a cached codec, return it
  if (!fory && !Type && _cachedCodec) {
    return _cachedCodec;
  }

  // If called with no args, use the shared cached Fory instance
  if (!fory || !Type) {
    const cached = getXlangForyAndType();
    fory = cached.fory;
    Type = cached.Type;
  }

  const codec = new ForyCodec(fory);

  // Framework-internal wire types per Aster-ContractIdentity.md
  // §11.3.2.3 "Framework-Internal Wire Types" — these carry explicit
  // Fory field IDs matching the Python `pyfory.field(id=N)` / Java
  // `@ForyField(id=N)` declarations. The IDs are pinned by Aster-SPEC
  // §5 so every binding's transport layer encodes and decodes them
  // with the same TypeMeta field-ID layout. Without these setId
  // calls, TS emits a name-keyed TypeMeta (~153 bytes for
  // StreamHeader) that pyfory's id-keyed decoder reads as all-zero
  // fields -- the Python server then rejects the request with
  // "Missing service name".

  // StreamHeader: ids 0..8 match bindings/python/aster/protocol.py.
  // `Type.varInt32()` matches Python's `pyfory.int32` — that TypeVar
  // is spec `varint32` (type id 5), not fixed `int32` (type id 4). See
  // docs/_internal/fory/docs/specification/xlang_type_mapping.md.
  const streamHeaderType = Type.struct(
    { namespace: '_aster', typeName: 'StreamHeader' },
    {
      service: Type.string().setId(0),
      method: Type.string().setId(1),
      version: Type.varInt32().setId(2),
      callId: Type.varInt32().setId(3),
      deadline: Type.int16().setId(4),
      serializationMode: Type.int8().setId(5),
      metadataKeys: Type.array(Type.string()).setId(6),
      metadataValues: Type.array(Type.string()).setId(7),
      sessionId: Type.varInt32().setId(8),
    },
    { withConstructor: true },
  );
  streamHeaderType.initMeta(StreamHeader);
  codec.registerType(streamHeaderType);

  // CallHeader: ids 0..4.
  const callHeaderType = Type.struct(
    { namespace: '_aster', typeName: 'CallHeader' },
    {
      method: Type.string().setId(0),
      callId: Type.varInt32().setId(1),
      deadline: Type.int16().setId(2),
      metadataKeys: Type.array(Type.string()).setId(3),
      metadataValues: Type.array(Type.string()).setId(4),
    },
    { withConstructor: true },
  );
  callHeaderType.initMeta(CallHeader);
  codec.registerType(callHeaderType);

  // RpcStatus: ids 0..3.
  const rpcStatusType = Type.struct(
    { namespace: '_aster', typeName: 'RpcStatus' },
    {
      code: Type.varInt32().setId(0),
      message: Type.string().setId(1),
      detailKeys: Type.array(Type.string()).setId(2),
      detailValues: Type.array(Type.string()).setId(3),
    },
    { withConstructor: true },
  );
  rpcStatusType.initMeta(RpcStatus);
  codec.registerType(rpcStatusType);

  // Cache the codec if created without explicit fory/Type arguments
  if (!_cachedCodec) {
    _cachedCodec = codec;
  }

  return codec;
}
