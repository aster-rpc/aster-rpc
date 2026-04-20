/**
 * Aster RPC server — QUIC accept loop + stream dispatch.
 *
 * Spec reference: S8.1 (Server API), S8.2 (Server accept loop)
 *
 * Accepts connections on an IrohNode, reads StreamHeaders, dispatches
 * to registered service handlers, and writes responses + trailers.
 *
 * This module handles real QUIC streams (via NAPI-RS native addon).
 * For in-process testing, use LocalTransport instead.
 */

import type { Codec } from './codec.js';
import { JsonCodec } from './codec.js';
import {
  writeFrame,
  readFrame,
  HEADER,
  TRAILER,
  COMPRESSED,
  CANCEL,
  END_STREAM,
} from './framing.js';
import { SessionServer } from './session.js';
import { StreamHeader, RpcStatus } from './protocol.js';
import { StatusCode, RpcError } from './status.js';
import { RpcPattern } from './types.js';
import type { ServiceRegistry, ServiceInfo, MethodInfo } from './service.js';
import {
  type Interceptor,
  CallContext,
  buildCallContext,
  applyRequestInterceptors,
  applyResponseInterceptors,
} from './interceptors/base.js';
import { withRequestContext, type AsterLogger, createLogger } from './logging.js';
import { validateMetadata, MAX_HANDLER_TIMEOUT_S, MAX_CLIENT_STREAM_ITEMS } from './limits.js';
import { DeadlineInterceptor } from './interceptors/deadline.js';
import type { PeerAttributeStore } from './peer-store.js';

/** QUIC connection interface (matches NAPI IrohConnection). */
interface ServerConnection {
  acceptBi(): Promise<{ takeSend(): ServerSendStream; takeRecv(): ServerRecvStream }>;
  remoteNodeId(): string;
  close(code: number, reason: string): void;
}

/** Send stream interface (matches NAPI IrohSendStream). */
interface ServerSendStream {
  writeAll(data: Uint8Array): Promise<void>;
  finish(): Promise<void>;
}

/** Recv stream interface (matches NAPI IrohRecvStream). */
interface ServerRecvStream {
  readExact(n: number): Promise<Uint8Array>;
}

/** Node interface for accepting connections (matches NAPI IrohNode). */
interface ServerNode {
  acceptAster(): Promise<ServerConnection>;
  nodeId(): string;
}

export interface ServerOptions {
  registry: ServiceRegistry;
  codec?: Codec;
  interceptors?: Interceptor[];
  logger?: AsterLogger;
  /** Per-peer admission attributes used to populate CallContext.attributes. */
  peerStore?: PeerAttributeStore;
}

/**
 * Aster RPC server over real QUIC streams.
 *
 * @example
 * ```ts
 * const server = new RpcServer({
 *   registry,
 *   codec: new JsonCodec(),
 * });
 * await server.serve(node);
 * ```
 */
export class RpcServer {
  private registry: ServiceRegistry;
  private codec: Codec;
  private interceptors: Interceptor[];
  private logger: AsterLogger;
  private peerStore?: PeerAttributeStore;
  private _serving = false;
  private _connections = new Set<ServerConnection>();

  /** Mark the server as serving (enables handleConnection loops). */
  setServing(value = true): void { this._serving = value; }

  constructor(opts: ServerOptions) {
    this.registry = opts.registry;
    this.codec = opts.codec ?? new JsonCodec();
    this.interceptors = opts.interceptors ?? [new DeadlineInterceptor()];
    this.logger = opts.logger ?? createLogger();
    this.peerStore = opts.peerStore;
  }

  /** Start accepting connections. Runs until close() is called. */
  async serve(node: ServerNode): Promise<void> {
    this._serving = true;
    this.logger.info('server starting', { node_id: node.nodeId() });

    while (this._serving) {
      try {
        const conn = await node.acceptAster();
        this._connections.add(conn);
        this.handleConnection(conn).catch(e => {
          this.logger.error('connection error', { error: String(e) });
        }).finally(() => {
          this._connections.delete(conn);
        });
      } catch (e) {
        if (!this._serving) break;
        this.logger.error('accept error', { error: String(e) });
      }
    }
  }

  /** Handle a single connection (accept streams in a loop). */
  async handleConnection(conn: ServerConnection): Promise<void> {
    const peerId = conn.remoteNodeId();
    this.logger.debug('connection opened', { peer: peerId });

    try {
      while (this._serving) {
        try {
          const bi = await conn.acceptBi();
          const send = bi.takeSend();
          const recv = bi.takeRecv();
          this.handleStream(conn, send, recv).catch(e => {
            this.logger.error('stream error', { error: String(e), peer: peerId });
          });
        } catch (e) {
          const msg = String(e);
          if (msg.includes('normal close') || msg.includes('code 0')) break;
          if (!this._serving) break;
          throw e;
        }
      }
    } finally {
      this.logger.debug('connection closed', { peer: peerId });
    }
  }

  /** Handle a single RPC stream.
   *
   * Multiplexed-streams note (spec §6): a single bi-stream may carry
   * multiple sequential calls. We loop reading StreamHeaders until the
   * peer EOFs its send side. Each call's handler writes its
   * response+trailer but does NOT `finish()` the server's send side,
   * so the stream stays reusable. v1 TS clients that still
   * `send.finish()` after one call cause the loop to exit cleanly on
   * the next read; new-protocol clients (Python, Java, TS v2) reuse
   * the stream from their per-connection pool and the loop handles
   * multiple calls per stream.
   */
  private async handleStream(
    conn: ServerConnection,
    send: ServerSendStream,
    recv: ServerRecvStream,
  ): Promise<void> {
    while (true) {
      const shouldContinue = await this.handleOneCallOnStream(conn, send, recv);
      if (!shouldContinue) return;
    }
  }

  /** Handle one call on a stream. Returns `true` if the stream is
   *  still alive and the caller should loop for the next call;
   *  `false` on EOF, error, or terminal condition that should exit
   *  the stream. */
  private async handleOneCallOnStream(
    conn: ServerConnection,
    send: ServerSendStream,
    recv: ServerRecvStream,
  ): Promise<boolean> {
    try {
      // Read StreamHeader (first frame, HEADER flag)
      const frame = await readFrame(recv, 0);
      if (!frame) return false; // clean EOF between calls
      const [payload, flags] = frame;

      if (!(flags & HEADER)) {
        await this.writeErrorTrailer(send, StatusCode.INTERNAL, 'first frame must have HEADER flag');
        return false;
      }

      // Sniff the first byte: '{' (0x7B) means JSON, anything else is binary
      // (Fory XLANG). Try JSON decoding first if payload looks like JSON,
      // since clients may send JSON even when the server advertises Fory
      // (e.g., if the manifest incorrectly said JSON-only).
      let header: StreamHeader | null = null;
      if (payload && payload[0] === 0x7b /* '{' */) {
        // Try JSON decoding first
        try {
          const jsonCodec = new JsonCodec();
          header = jsonCodec.decode(payload) as StreamHeader;
        } catch {
          // JSON decode failed, try Fory
        }
      }
      if (!header) {
        header = this.codec.decode(payload) as StreamHeader;
      }

      if (!header.service) {
        await this.writeErrorTrailer(send, StatusCode.INVALID_ARGUMENT, 'missing service name');
        return false;
      }

      // Look up service
      const svcInfo = this.registry.lookup(header.service, header.version);
      if (!svcInfo) {
        await this.writeErrorTrailer(send, StatusCode.NOT_FOUND, `service '${header.service}' v${header.version} not found`);
        return false;
      }

      // ── Session discriminator check ─────────────────────────────────
      // Spec §6 (multiplexed streams): the method name always lives in
      // the StreamHeader. Session-scoped services are indicated by
      // `StreamHeader.sessionId != 0`, NOT by an empty method.
      // Legacy v1 clients that still send `method=''` + CALL frames
      // are routed through the pre-migration `SessionServer` path as
      // long as the service is session-scoped AND the client used
      // the legacy discriminator. Everyone else sends `method=<name>`
      // and is dispatched through the normal per-pattern path
      // regardless of scope.
      const isLegacySessionStream = (header.method === '');
      const isSessionService = (svcInfo.scoped === 'session');
      const sessionId = (header as { sessionId?: number }).sessionId ?? 0;

      if (isLegacySessionStream && !isSessionService) {
        // Legacy session shape on a shared service — error out.
        const msg = `'${header.service}' is shared: send a method name instead of opening a session stream (method='')`;
        this.logger.warn(`scope mismatch: ${msg}`);
        await this.writeErrorTrailer(send, StatusCode.FAILED_PRECONDITION, msg);
        return false;
      }

      if (!isLegacySessionStream && isSessionService && sessionId === 0) {
        // Session-scoped service called with method name but no
        // sessionId. Pre-migration behaviour: reject (the client
        // should open a session stream).
        const msg = `'${header.service}' is session-scoped: open a session stream (method='') instead of calling method '${header.method}' directly`;
        this.logger.warn(`scope mismatch: ${msg}`);
        await this.writeErrorTrailer(send, StatusCode.FAILED_PRECONDITION, msg);
        return false;
      }

      if (isLegacySessionStream) {
        let peerId: string | undefined;
        try { peerId = conn.remoteNodeId(); } catch { /* ignore */ }

        let attributes: Record<string, string> | undefined;
        if (peerId && this.peerStore) {
          const m = this.peerStore.getAttributes(peerId);
          if (m.size > 0) attributes = Object.fromEntries(m);
        }

        const sessionServer = new SessionServer(this.codec, this.interceptors);
        await sessionServer.handleSession(recv, send, svcInfo, header, peerId, attributes);
        // Legacy session protocol owns the whole stream; after it
        // returns the stream is done.
        try { await send.finish(); } catch { /* best effort */ }
        return false;
      }
      // ── End session discriminator ──────────────────────────────────

      // Look up method (shared services only — session dispatch is above)
      const methodInfo = svcInfo.methods.get(header.method);
      if (!methodInfo) {
        await this.writeErrorTrailer(send, StatusCode.UNIMPLEMENTED, `method '${header.service}.${header.method}' not found`);
        return false;
      }

      // Validate metadata
      if (header.metadataKeys.length > 0) {
        try {
          validateMetadata(header.metadataKeys, header.metadataValues);
        } catch {
          await this.writeErrorTrailer(send, StatusCode.RESOURCE_EXHAUSTED, 'metadata exceeds limits');
          return false;
        }
      }

      // Build call context
      let peerId: string | undefined;
      try { peerId = conn.remoteNodeId(); } catch { /* ignore */ }

      // Pull admission attributes from the peer store so authorization
      // interceptors (CapabilityInterceptor) see the caller's roles.
      let attributes: Record<string, string> | undefined;
      if (peerId && this.peerStore) {
        const m = this.peerStore.getAttributes(peerId);
        if (m.size > 0) {
          attributes = Object.fromEntries(m);
        }
      }

      const callCtx = buildCallContext({
        service: header.service,
        method: header.method,
        metadata: this.buildMetadata(header),
        deadlineSecs: header.deadline,
        peer: peerId,
        pattern: methodInfo.pattern as any,
        idempotent: methodInfo.idempotent,
        callId: header.callId ? String(header.callId) : undefined,
        attributes,
      });

      // Dispatch by pattern
      await withRequestContext(
        { service: header.service, method: header.method, requestId: callCtx.callId, peer: peerId },
        () => this.dispatchRpc(svcInfo, methodInfo, callCtx, send, recv),
      );

      // Spec §6: every stream is multiplexed. Don't finish the send
      // side; loop for the next call on the same bi-stream. Legacy
      // v1 clients that `send.finish()` after one call will cause
      // the next read to return null and we'll exit cleanly.
      return true;

    } catch (e) {
      if (e instanceof RpcError) {
        await this.writeErrorTrailer(send, e.code, e.message);
      } else {
        this.logger.error('stream handler error', { error: String(e) });
        await this.writeErrorTrailer(send, StatusCode.INTERNAL, String(e));
      }
      return false;
    }
  }

  /** Dispatch an RPC based on pattern. */
  private async dispatchRpc(
    svcInfo: ServiceInfo,
    methodInfo: MethodInfo,
    callCtx: CallContext,
    send: ServerSendStream,
    recv: ServerRecvStream,
  ): Promise<void> {
    const handler = methodInfo.handler;
    if (!handler) {
      await this.writeErrorTrailer(send, StatusCode.INTERNAL, 'no handler');
      return;
    }

    // Run authorization-style interceptors BEFORE pattern dispatch.
    // CallContext-only interceptors (CapabilityInterceptor, deadline, metrics,
    // rate-limit) execute here and can reject the stream before any frames
    // are read. This guarantees auth checks fire on every pattern, including
    // bidi streams that might never produce a request frame.
    try {
      await applyRequestInterceptors(this.interceptors, callCtx, null);
    } catch (e) {
      if (e instanceof RpcError) {
        await this.writeErrorTrailer(send, e.code, e.message);
        return;
      }
      throw e;
    }

    switch (methodInfo.pattern) {
      case RpcPattern.UNARY:
        await this.handleUnary(svcInfo, methodInfo, handler, callCtx, send, recv);
        break;
      case RpcPattern.SERVER_STREAM:
        await this.handleServerStream(svcInfo, methodInfo, handler, callCtx, send, recv);
        break;
      case RpcPattern.CLIENT_STREAM:
        await this.handleClientStream(svcInfo, methodInfo, handler, callCtx, send, recv);
        break;
      case RpcPattern.BIDI_STREAM:
        await this.handleBidiStream(svcInfo, methodInfo, handler, callCtx, send, recv);
        break;
      default:
        await this.writeErrorTrailer(send, StatusCode.INTERNAL, `unknown pattern: ${methodInfo.pattern}`);
    }
  }

  private async handleUnary(
    svcInfo: ServiceInfo, methodInfo: MethodInfo, handler: Function,
    callCtx: CallContext, send: ServerSendStream, recv: ServerRecvStream,
  ): Promise<void> {
    // Read request
    const reqFrame = await readFrame(recv, 0);
    if (!reqFrame) {
      await this.writeErrorTrailer(send, StatusCode.UNAVAILABLE, 'stream ended before request');
      return;
    }
    const [reqPayload, reqFlags] = reqFrame;
    const compressed = !!(reqFlags & COMPRESSED);
    // Pass the request type as the codec hint so JsonCodec can do
    // strict shape validation. Wrong/extra/missing fields raise
    // CONTRACT_VIOLATION before the handler runs.
    const reqType = methodInfo.requestType as unknown;
    let request = compressed
      ? (this.codec as any).decodeCompressed(reqPayload, true, reqType)
      : this.codec.decode(reqPayload, reqType);

    // Interceptors
    request = await applyRequestInterceptors(this.interceptors, callCtx, request);

    // Invoke handler with deadline enforcement + CallContext injection
    const acceptsCtx = methodInfo.acceptsCtx === true;
    let response: any;
    try {
      response = await CallContext.runWith(callCtx, () =>
        this.runWithDeadline(callCtx, () =>
          acceptsCtx
            ? handler.call(svcInfo.instance, request, callCtx)
            : handler.call(svcInfo.instance, request),
        ),
      );
    } catch (e) {
      if (e instanceof RpcError && e.code === StatusCode.DEADLINE_EXCEEDED) {
        await this.writeErrorTrailer(send, StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
        return;
      }
      throw e;
    }
    response = await applyResponseInterceptors(this.interceptors, callCtx, response);

    // Write response + trailer. Spec §6: don't finish the send side —
    // the outer loop in `handleStream` will read the next StreamHeader
    // on this multiplexed bi-stream.
    const respType = methodInfo.responseType as unknown;
    const [respPayload, respCompressed] = this.codec.encodeCompressed(response, respType);
    await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    await this.writeOkTrailer(send);
  }

  private async handleServerStream(
    svcInfo: ServiceInfo, methodInfo: MethodInfo, handler: Function,
    callCtx: CallContext, send: ServerSendStream, recv: ServerRecvStream,
  ): Promise<void> {
    const reqFrame = await readFrame(recv, 0);
    if (!reqFrame) {
      await this.writeErrorTrailer(send, StatusCode.UNAVAILABLE, 'stream ended');
      return;
    }
    const [reqPayload, reqFlags] = reqFrame;
    const compressed = !!(reqFlags & COMPRESSED);
    const reqType = methodInfo.requestType as unknown;
    let request = compressed
      ? (this.codec as any).decodeCompressed(reqPayload, true, reqType)
      : this.codec.decode(reqPayload, reqType);

    request = await applyRequestInterceptors(this.interceptors, callCtx, request);

    const acceptsCtx = methodInfo.acceptsCtx === true;
    const gen = CallContext.runWith(callCtx, () =>
      acceptsCtx
        ? handler.call(svcInfo.instance, request, callCtx)
        : handler.call(svcInfo.instance, request),
    );
    const deadlineMs = Date.now() + this.handlerTimeoutMs(callCtx);
    const respType = methodInfo.responseType as unknown;
    for await (let response of gen) {
      if (Date.now() > deadlineMs) {
        await this.writeErrorTrailer(send, StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
        return;
      }
      response = await applyResponseInterceptors(this.interceptors, callCtx, response);
      const [respPayload, respCompressed] = this.codec.encodeCompressed(response, respType);
      await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    }

    await this.writeOkTrailer(send);
    // Spec §6: don't finish — let `handleStream` loop for the next call.
  }

  private async handleClientStream(
    svcInfo: ServiceInfo, methodInfo: MethodInfo, handler: Function,
    callCtx: CallContext, send: ServerSendStream, recv: ServerRecvStream,
  ): Promise<void> {
    // Collect requests until the client marks end-of-input.
    //
    // Two protocols recognised (spec §6 multiplexed streams replaced
    // the old shape but legacy v1 clients still send it):
    //   - NEW: the last request frame carries `FLAG_END_STREAM` and
    //     there is NO trailing empty frame. The client then waits
    //     for the server's response on the same stream.
    //   - OLD: the client sends request frames and then an empty
    //     `FLAG_TRAILER` frame as explicit end-of-input.
    // Either shape terminates collection.
    const requests: unknown[] = [];
    const reqType = methodInfo.requestType as unknown;
    while (true) {
      const frame = await readFrame(recv, 0);
      if (!frame) break;
      const [payload, flags] = frame;
      if (flags & TRAILER) {
        try {
          const eoi = this.codec.decode(payload) as RpcStatus;
          if (eoi.code !== StatusCode.OK) {
            await this.writeErrorTrailer(send, StatusCode.INTERNAL,
              `client sent non-OK EoI trailer (code=${eoi.code})`);
            return;
          }
        } catch { /* best-effort validation */ }
        break;
      }
      if (flags & CANCEL) continue;
      if (requests.length >= MAX_CLIENT_STREAM_ITEMS) {
        await this.writeErrorTrailer(send, StatusCode.RESOURCE_EXHAUSTED,
          `client stream exceeded ${MAX_CLIENT_STREAM_ITEMS} items`);
        return;
      }
      const compressed = !!(flags & COMPRESSED);
      let request = compressed
        ? (this.codec as any).decodeCompressed(payload, true, reqType)
        : this.codec.decode(payload, reqType);
      request = await applyRequestInterceptors(this.interceptors, callCtx, request);
      requests.push(request);
      // Spec §6: last request is marked with FLAG_END_STREAM.
      // Consume the frame into `requests` first (payload may carry a
      // real request, not just an empty marker), then exit the loop.
      if (flags & END_STREAM) break;
    }

    // Provide as async iterable
    async function* requestIter() { for (const r of requests) yield r; }

    const acceptsCtxCS = methodInfo.acceptsCtx === true;
    let response: any;
    try {
      response = await CallContext.runWith(callCtx, () =>
        this.runWithDeadline(callCtx, () =>
          acceptsCtxCS
            ? handler.call(svcInfo.instance, requestIter(), callCtx)
            : handler.call(svcInfo.instance, requestIter()),
        ),
      );
    } catch (e) {
      if (e instanceof RpcError && e.code === StatusCode.DEADLINE_EXCEEDED) {
        await this.writeErrorTrailer(send, StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
        return;
      }
      throw e;
    }
    response = await applyResponseInterceptors(this.interceptors, callCtx, response);

    const respType = methodInfo.responseType as unknown;
    const [respPayload, respCompressed] = this.codec.encodeCompressed(response, respType);
    await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    await this.writeOkTrailer(send);
    // Spec §6: don't finish — let `handleStream` loop for the next call.
  }

  private async handleBidiStream(
    svcInfo: ServiceInfo, methodInfo: MethodInfo, handler: Function,
    callCtx: CallContext, send: ServerSendStream, recv: ServerRecvStream,
  ): Promise<void> {
    // Auth interceptors already ran in dispatchRpc() before this method.
    // No need to run them again here.

    // Create request iterable from incoming frames with error capture.
    // Wrap the error in an object so TS doesn't narrow it to `never`
    // after the for-await loop (TS can't track closure mutations to a
    // local variable, but it can track property access through one).
    const self = this;
    const errBox: { value: Error | null } = { value: null };
    const reqType = methodInfo.requestType as unknown;
    async function* requestIter() {
      try {
        while (true) {
          const frame = await readFrame(recv, 0);
          if (!frame) break;
          const [payload, flags] = frame;
          if (flags & TRAILER) break; // legacy explicit end-of-input
          if (flags & CANCEL) continue;
          const compressed = !!(flags & COMPRESSED);
          const request = compressed
            ? (self.codec as any).decodeCompressed(payload, true, reqType)
            : self.codec.decode(payload, reqType);
          yield request;
          // Spec §6: the client marks the last request with
          // FLAG_END_STREAM on the same frame as the payload. After
          // yielding it, stop reading so the server handler can
          // start writing responses and the outer stream loop can
          // proceed to the next call.
          if (flags & END_STREAM) break;
        }
      } catch (e) {
        errBox.value = e instanceof Error ? e : new Error(String(e));
      }
    }

    const acceptsCtxBD = methodInfo.acceptsCtx === true;
    const gen = CallContext.runWith(callCtx, () =>
      acceptsCtxBD
        ? handler.call(svcInfo.instance, requestIter(), callCtx)
        : handler.call(svcInfo.instance, requestIter()),
    );
    const deadlineMs = Date.now() + this.handlerTimeoutMs(callCtx);
    const respType = methodInfo.responseType as unknown;
    for await (const response of gen) {
      if (Date.now() > deadlineMs) {
        await this.writeErrorTrailer(send, StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
        return;
      }
      const [respPayload, respCompressed] = this.codec.encodeCompressed(response, respType);
      await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    }

    if (errBox.value !== null) {
      await this.writeErrorTrailer(send, StatusCode.INTERNAL,
        `bidi stream reader error: ${errBox.value.message}`);
      return;
    }

    await this.writeOkTrailer(send);
    // Spec §6: don't finish — let `handleStream` loop for the next call.
  }

  // -- Helpers ----------------------------------------------------------------

  private handlerTimeoutMs(callCtx: CallContext): number {
    const maxMs = MAX_HANDLER_TIMEOUT_S * 1000;
    const deadline = callCtx.deadline;
    if (deadline == null || deadline <= 0) return maxMs;
    const remaining = deadline * 1000 - Date.now();
    return Math.max(0, Math.min(remaining, maxMs));
  }

  /** Run a handler with deadline enforcement, clearing the timer when done. */
  private async runWithDeadline<T>(
    callCtx: CallContext,
    fn: () => Promise<T>,
  ): Promise<T> {
    const timeout = this.handlerTimeoutMs(callCtx);
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        fn(),
        new Promise<never>((_, reject) => {
          timer = setTimeout(
            () => reject(new RpcError(StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded')),
            timeout,
          );
        }),
      ]);
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  private buildMetadata(header: StreamHeader): Record<string, string> {
    const metadata: Record<string, string> = {};
    for (let i = 0; i < header.metadataKeys.length; i++) {
      metadata[header.metadataKeys[i]!] = header.metadataValues[i] ?? '';
    }
    return metadata;
  }

  private async writeOkTrailer(send: ServerSendStream): Promise<void> {
    const status = new RpcStatus({ code: StatusCode.OK });
    await writeFrame(send, this.codec.encode(status), TRAILER);
  }

  private async writeErrorTrailer(send: ServerSendStream, code: number, message: string): Promise<void> {
    const status = new RpcStatus({ code, message });
    try {
      await writeFrame(send, this.codec.encode(status), TRAILER);
      await send.finish();
    } catch {
      // Best effort — stream may already be closed
    }
  }

  /** Stop accepting new connections. */
  close(): void {
    this._serving = false;
    for (const conn of this._connections) {
      try { conn.close(0, 'server shutdown'); } catch { /* ignore */ }
    }
    this._connections.clear();
  }

  /** The service registry used by this server. */
  get serviceRegistry(): ServiceRegistry {
    return this.registry;
  }

  /**
   * Wait until the server is stopped (i.e., close() is called).
   * Returns a promise that resolves when serving ends.
   */
  async waitUntilStopped(): Promise<void> {
    return new Promise<void>(resolve => {
      const check = (): void => {
        if (!this._serving) { resolve(); return; }
        setTimeout(check, 50);
      };
      check();
    });
  }
}
