/**
 * Session-scoped services -- multiplexed calls on a single QUIC stream.
 *
 * Spec reference: Aster-session-scoped-services.md
 *
 * Instead of opening a new stream per RPC, session-scoped services
 * keep a single bidirectional stream open. Each call is demarcated
 * by CALL/CANCEL frames:
 *
 *   Stream: [StreamHeader] [CALL + CallHeader] [request] [response] [TRAILER] ...
 *
 * The StreamHeader has an empty method ("") to indicate session mode.
 * Each CALL frame carries a CallHeader with the method name and call ID.
 */

import type { Codec } from './codec.js';
import { JsonCodec } from './codec.js';
import {
  writeFrame,
  readFrame,
  TRAILER,
  CALL,
  CANCEL,
  COMPRESSED,
} from './framing.js';
import { CallHeader, RpcStatus, StreamHeader } from './protocol.js';
import { StatusCode, RpcError } from './status.js';
import type { ServiceInfo, MethodInfo } from './service.js';
import { RpcPattern } from './types.js';
import {
  type Interceptor,
  buildCallContext,
  applyRequestInterceptors,
  applyResponseInterceptors,
} from './interceptors/base.js';

/**
 * Server-side session handler.
 *
 * Reads CALL frames from a single stream, dispatches to the appropriate
 * method handler on a per-session service instance, and writes responses.
 */
export class SessionServer {
  private codec: Codec;
  private interceptors: Interceptor[];

  constructor(codec?: Codec, interceptors?: Interceptor[]) {
    this.codec = codec ?? new JsonCodec();
    this.interceptors = interceptors ?? [];
  }

  /**
   * Handle a session stream. Creates a fresh service instance, then reads
   * CALL frames in a loop, dispatching each to the appropriate method
   * handler until the stream ends.
   */
  async handleSession(
    recv: { readExact(n: number): Promise<Uint8Array> },
    send: { writeAll(data: Uint8Array): Promise<void> },
    serviceInfo: ServiceInfo,
    _streamHeader: StreamHeader,
    peer?: string,
    attributes?: Record<string, string>,
  ): Promise<void> {
    // Create a fresh service instance per session so each client gets
    // its own state (matches Python SessionServer behaviour).
    let instance: any;
    try {
      const ctor = (serviceInfo.instance as any).constructor;
      instance = new ctor();
    } catch {
      instance = serviceInfo.instance;
    }

    // Read CALL frames in a loop
    while (true) {
      const frame = await readFrame(recv as any, 0);
      if (!frame) break; // stream ended

      const [payload, flags] = frame;

      if (flags & CANCEL) {
        continue;
      }

      if (!(flags & CALL)) {
        const status = new RpcStatus({ code: StatusCode.INTERNAL, message: 'expected CALL frame in session stream' });
        await writeFrame(send as any, this.codec.encode(status), TRAILER);
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
        await writeFrame(send as any, this.codec.encode(status), TRAILER);
        continue;
      }

      // Build per-call context with peer attributes for auth interceptors
      const callCtx = buildCallContext({
        service: serviceInfo.name,
        method: callHeader.method,
        callId: callHeader.callId || undefined,
        peer,
        pattern: methodInfo.pattern as any,
        idempotent: methodInfo.idempotent,
        attributes,
      });

      // Run auth interceptors before reading the request
      try {
        await applyRequestInterceptors(this.interceptors, callCtx, null);
      } catch (e) {
        if (e instanceof RpcError) {
          const status = new RpcStatus({ code: e.code, message: e.message });
          await writeFrame(send as any, this.codec.encode(status), TRAILER);
          // Drain the request frame(s) so the stream stays in sync
          await this.drainRequestFrame(recv);
          continue;
        }
        throw e;
      }

      // Dispatch based on pattern
      try {
        switch (methodInfo.pattern) {
          case RpcPattern.UNARY:
            await this.handleUnary(instance, methodInfo, callCtx, send, recv);
            break;
          case RpcPattern.SERVER_STREAM:
            await this.handleServerStream(instance, methodInfo, callCtx, send, recv);
            break;
          case RpcPattern.CLIENT_STREAM:
            await this.handleClientStream(instance, methodInfo, callCtx, send, recv);
            break;
          case RpcPattern.BIDI_STREAM:
            await this.handleBidiStream(instance, methodInfo, callCtx, send, recv);
            break;
          default: {
            const status = new RpcStatus({
              code: StatusCode.UNIMPLEMENTED,
              message: `unsupported pattern: ${methodInfo.pattern}`,
            });
            await writeFrame(send as any, this.codec.encode(status), TRAILER);
          }
        }
      } catch (e) {
        const err = e instanceof RpcError ? e : new RpcError(StatusCode.INTERNAL, String(e));
        const status = new RpcStatus({ code: err.code, message: err.message });
        try {
          await writeFrame(send as any, this.codec.encode(status), TRAILER);
        } catch { /* stream may be gone */ }
      }
    }
  }

  // -- Pattern handlers -------------------------------------------------------

  private async handleUnary(
    instance: any, methodInfo: MethodInfo, callCtx: any,
    send: { writeAll(data: Uint8Array): Promise<void> },
    recv: { readExact(n: number): Promise<Uint8Array> },
  ): Promise<void> {
    const reqFrame = await readFrame(recv as any, 0);
    if (!reqFrame) return;
    const [reqPayload, reqFlags] = reqFrame;
    const compressed = !!(reqFlags & COMPRESSED);
    let request = compressed
      ? (this.codec as any).decodeCompressed(reqPayload, true)
      : this.codec.decode(reqPayload);

    request = await applyRequestInterceptors(this.interceptors, callCtx, request);

    let response = await methodInfo.handler!.call(instance, request);
    response = await applyResponseInterceptors(this.interceptors, callCtx, response);

    const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
    await writeFrame(send as any, respPayload, respCompressed ? COMPRESSED : 0);
    await this.writeOkTrailer(send);
  }

  private async handleServerStream(
    instance: any, methodInfo: MethodInfo, callCtx: any,
    send: { writeAll(data: Uint8Array): Promise<void> },
    recv: { readExact(n: number): Promise<Uint8Array> },
  ): Promise<void> {
    const reqFrame = await readFrame(recv as any, 0);
    if (!reqFrame) return;
    const [reqPayload, reqFlags] = reqFrame;
    const compressed = !!(reqFlags & COMPRESSED);
    let request = compressed
      ? (this.codec as any).decodeCompressed(reqPayload, true)
      : this.codec.decode(reqPayload);

    request = await applyRequestInterceptors(this.interceptors, callCtx, request);

    const gen = methodInfo.handler!.call(instance, request);
    for await (let response of gen) {
      response = await applyResponseInterceptors(this.interceptors, callCtx, response);
      const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
      await writeFrame(send as any, respPayload, respCompressed ? COMPRESSED : 0);
    }

    await this.writeOkTrailer(send);
  }

  private async handleClientStream(
    instance: any, methodInfo: MethodInfo, callCtx: any,
    send: { writeAll(data: Uint8Array): Promise<void> },
    recv: { readExact(n: number): Promise<Uint8Array> },
  ): Promise<void> {
    const requests: unknown[] = [];
    while (true) {
      const frame = await readFrame(recv as any, 0);
      if (!frame) break;
      const [p, f] = frame;
      if (f & TRAILER) break;
      if (f & CANCEL) continue;
      const compressed = !!(f & COMPRESSED);
      const req = compressed
        ? (this.codec as any).decodeCompressed(p, true)
        : this.codec.decode(p);
      requests.push(await applyRequestInterceptors(this.interceptors, callCtx, req));
    }

    async function* requestIter() { for (const r of requests) yield r; }
    let response = await methodInfo.handler!.call(instance, requestIter());
    response = await applyResponseInterceptors(this.interceptors, callCtx, response);

    const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
    await writeFrame(send as any, respPayload, respCompressed ? COMPRESSED : 0);
    await this.writeOkTrailer(send);
  }

  private async handleBidiStream(
    instance: any, methodInfo: MethodInfo, _callCtx: any,
    send: { writeAll(data: Uint8Array): Promise<void> },
    recv: { readExact(n: number): Promise<Uint8Array> },
  ): Promise<void> {
    const self = this;
    async function* requestIter() {
      while (true) {
        const frame = await readFrame(recv as any, 0);
        if (!frame) break;
        const [p, f] = frame;
        if (f & TRAILER) break;
        if (f & CANCEL) continue;
        const compressed = !!(f & COMPRESSED);
        const req = compressed
          ? (self.codec as any).decodeCompressed(p, true)
          : self.codec.decode(p);
        yield req;
      }
    }

    const gen = methodInfo.handler!.call(instance, requestIter());
    for await (const response of gen) {
      const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
      await writeFrame(send as any, respPayload, respCompressed ? COMPRESSED : 0);
    }

    await this.writeOkTrailer(send);
  }

  // -- Helpers ----------------------------------------------------------------

  private async writeOkTrailer(send: { writeAll(data: Uint8Array): Promise<void> }): Promise<void> {
    const status = new RpcStatus({ code: StatusCode.OK });
    await writeFrame(send as any, this.codec.encode(status), TRAILER);
  }

  private async drainRequestFrame(recv: { readExact(n: number): Promise<Uint8Array> }): Promise<void> {
    try {
      const frame = await readFrame(recv as any, 0);
      // Just discard -- we needed to consume it so the next CALL frame
      // is properly aligned on the stream.
      void frame;
    } catch { /* ignore */ }
  }
}

// ── Session stub (client-side) ───────────────────────────────────────────────

/**
 * Client-side session stub -- tracks an active session-scoped RPC stream.
 */
export class SessionStub {
  private _cancelled = false;

  constructor(
    private readonly transport: { close?(): Promise<void> },
    readonly sessionId: string,
  ) {}

  async cancel(): Promise<void> {
    this._cancelled = true;
    if (this.transport.close) await this.transport.close();
  }

  get cancelled(): boolean {
    return this._cancelled;
  }
}

// ── Factory functions ────────────────────────────────────────────────────────

export async function createSession(
  transport: { close?(): Promise<void> },
  sessionId?: string,
): Promise<SessionStub> {
  const id = sessionId ?? crypto.randomUUID?.() ?? `session-${Date.now()}`;
  return new SessionStub(transport, id);
}
