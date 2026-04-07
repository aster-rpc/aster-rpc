/**
 * Session-scoped services — multiplexed calls on a single QUIC stream.
 *
 * Spec reference: Aster-session-scoped-services.md
 *
 * Instead of opening a new stream per RPC, session-scoped services
 * keep a single bidirectional stream open. Each call is demarcated
 * by CALL/CANCEL frames:
 *
 *   Stream: [StreamHeader] [CALL + CallHeader] [request] [response] [CALL + ...] ...
 *
 * The StreamHeader has an empty method ("") to indicate session mode.
 * Each CALL frame carries a CallHeader with the method name and call ID.
 */

import type { SendStream, RecvStream } from '@aster-rpc/transport';
import type { Codec } from './codec.js';
import { JsonCodec } from './codec.js';
import {
  writeFrame,
  readFrame,
  TRAILER,
  CALL,
  CANCEL,
} from './framing.js';
import { CallHeader, RpcStatus } from './protocol.js';
import { StatusCode, RpcError } from './status.js';
import type { ServiceInfo } from './service.js';
import { RpcPattern } from './types.js';

/**
 * Server-side session handler.
 *
 * Reads CALL frames from a single stream, dispatches to the appropriate
 * method handler, and writes responses back.
 */
export class SessionServer {
  private codec: Codec;

  constructor(codec?: Codec) {
    this.codec = codec ?? new JsonCodec();
  }

  /**
   * Handle a session stream. Reads CALL/CANCEL frames and dispatches
   * to the service's methods until the stream ends.
   */
  async handleSession(
    recv: RecvStream,
    send: SendStream,
    serviceInfo: ServiceInfo,
  ): Promise<void> {
    // Read CALL frames in a loop
    while (true) {
      const frame = await readFrame(recv, 0);
      if (!frame) break; // stream ended

      const [payload, flags] = frame;

      if (flags & CANCEL) {
        // Cancel current call — in Phase 1, just skip
        continue;
      }

      if (!(flags & CALL)) {
        // Unexpected frame type in session mode
        const status = new RpcStatus({
          code: StatusCode.INTERNAL,
          message: 'expected CALL frame in session stream',
        });
        await writeFrame(send, this.codec.encode(status), TRAILER);
        break;
      }

      // Decode CallHeader
      const callHeader = this.codec.decode(payload) as CallHeader;
      const methodInfo = serviceInfo.methods.get(callHeader.method);

      if (!methodInfo) {
        const status = new RpcStatus({
          code: StatusCode.NOT_FOUND,
          message: `method ${callHeader.method} not found`,
        });
        await writeFrame(send, this.codec.encode(status), TRAILER);
        continue;
      }

      // Read request payload
      const reqFrame = await readFrame(recv, 0);
      if (!reqFrame) break;
      const [reqPayload] = reqFrame;
      const request = this.codec.decode(reqPayload);

      try {
        // Dispatch based on pattern
        if (methodInfo.pattern === RpcPattern.UNARY) {
          const response = await methodInfo.handler!.call(serviceInfo.instance, request);
          await writeFrame(send, this.codec.encode(response), 0);
        } else if (methodInfo.pattern === RpcPattern.SERVER_STREAM) {
          const gen = methodInfo.handler!.call(serviceInfo.instance, request);
          for await (const item of gen) {
            await writeFrame(send, this.codec.encode(item), 0);
          }
        }

        // Write OK trailer for this call
        const ok = new RpcStatus({ code: StatusCode.OK });
        await writeFrame(send, this.codec.encode(ok), TRAILER);
      } catch (e) {
        const err = e instanceof RpcError ? e : new RpcError(StatusCode.INTERNAL, String(e));
        const status = new RpcStatus({
          code: err.code,
          message: err.message,
        });
        await writeFrame(send, this.codec.encode(status), TRAILER);
      }
    }
  }
}
