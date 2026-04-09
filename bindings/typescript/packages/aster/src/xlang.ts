/**
 * xlang.ts -- XLANG codec factory for cross-language interop.
 *
 * Creates a ForyCodec pre-configured with the Aster protocol types
 * (StreamHeader, RpcStatus) for cross-language communication with
 * Python and other language bindings.
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
 * so the transport can encode/decode the wire protocol. User-defined types
 * are serialized dynamically by Fory's compatible mode.
 *
 * Requires `@apache-fory/core` to be installed.
 */
export function createXlangCodec(): ForyCodec {
  // Dynamic import to keep @apache-fory/core optional
  let Fory: any;
  try {
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    Fory = require('@apache-fory/core').default;
  } catch {
    throw new Error(
      'Cross-language (XLANG) codec requires @apache-fory/core. ' +
      'Install it with: bun add @apache-fory/core',
    );
  }

  const fory = new Fory({ refTracking: false });
  const codec = new ForyCodec(fory);

  // Register protocol types with their wire tags
  const protocolTypes = [StreamHeader, CallHeader, RpcStatus];
  for (const cls of protocolTypes) {
    const tag = (cls as any).wireType as string;
    if (tag) {
      const [ns, name] = tag.includes('/') ? tag.split('/') : ['', tag];
      // Build Fory type description from the class
      codec.registerType(
        fory.classResolver.createTypeDescription({
          type: cls,
          namespace: ns,
          typeName: name,
        }),
      );
    }
  }

  return codec;
}
