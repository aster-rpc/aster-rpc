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

/**
 * Create a ForyCodec pre-configured for cross-language (XLANG) interop.
 *
 * Registers the Aster protocol types (StreamHeader, CallHeader, RpcStatus)
 * with explicit int8/int16/int32 field types matching the IDL spec.
 *
 * Requires `@apache-fory/core` to be installed.
 */
export function createXlangCodec(): ForyCodec {
  let Fory: any;
  let Type: any;
  try {
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const foryModule = require('@apache-fory/core');
    Fory = foryModule.default;
    Type = foryModule.Type;
  } catch {
    throw new Error(
      'Cross-language (XLANG) codec requires @apache-fory/core. ' +
      'Install it with: bun add @apache-fory/core',
    );
  }

  const fory = new Fory({ refTracking: false });
  const codec = new ForyCodec(fory);

  // StreamHeader: explicit field types matching Python pyfory annotations
  const streamHeaderType = Type.struct(
    { namespace: '_aster', typeName: 'StreamHeader' },
    {
      service: Type.string(),
      method: Type.string(),
      version: Type.int32(),
      callId: Type.int32(),
      deadline: Type.int16(),
      serializationMode: Type.int8(),
      metadataKeys: Type.array(Type.string()),
      metadataValues: Type.array(Type.string()),
    },
    { withConstructor: true },
  );
  streamHeaderType.initMeta(StreamHeader);
  codec.registerType(streamHeaderType);

  // CallHeader: explicit field types
  const callHeaderType = Type.struct(
    { namespace: '_aster', typeName: 'CallHeader' },
    {
      method: Type.string(),
      callId: Type.int32(),
      deadline: Type.int16(),
      metadataKeys: Type.array(Type.string()),
      metadataValues: Type.array(Type.string()),
    },
    { withConstructor: true },
  );
  callHeaderType.initMeta(CallHeader);
  codec.registerType(callHeaderType);

  // RpcStatus: all fields are already naturally typed (int32, string, list<string>)
  const rpcStatusType = Type.struct(
    { namespace: '_aster', typeName: 'RpcStatus' },
    {
      code: Type.int32(),
      message: Type.string(),
      detailKeys: Type.array(Type.string()),
      detailValues: Type.array(Type.string()),
    },
    { withConstructor: true },
  );
  rpcStatusType.initMeta(RpcStatus);
  codec.registerType(rpcStatusType);

  return codec;
}
