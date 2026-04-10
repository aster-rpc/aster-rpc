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

  /** Handle a single RPC stream. */
  private async handleStream(
    conn: ServerConnection,
    send: ServerSendStream,
    recv: ServerRecvStream,
  ): Promise<void> {
    try {
      // Read StreamHeader (first frame, HEADER flag)
      const frame = await readFrame(recv, 0);
      if (!frame) return;
      const [payload, flags] = frame;

      if (!(flags & HEADER)) {
        await this.writeErrorTrailer(send, StatusCode.INTERNAL, 'first frame must have HEADER flag');
        return;
      }

      // Sniff the first byte: '{' (0x7B) means JSON, anything else is binary
      // (Fory XLANG). The TypeScript binding only speaks JSON because Fory JS
      // is not yet XLANG-compliant — refuse binary requests with a clear,
      // JSON-encoded error trailer the peer can decode either way.
      if (!payload || payload[0] !== 0x7b /* '{' */) {
        await this.writeErrorTrailer(
          send,
          StatusCode.INVALID_ARGUMENT,
          'this server only supports JSON serialization (mode 3); resend the StreamHeader as JSON',
        );
        return;
      }

      const header = this.codec.decode(payload) as StreamHeader;

      if (!header.service) {
        await this.writeErrorTrailer(send, StatusCode.INVALID_ARGUMENT, 'missing service name');
        return;
      }

      // Look up service
      const svcInfo = this.registry.lookup(header.service, header.version);
      if (!svcInfo) {
        await this.writeErrorTrailer(send, StatusCode.NOT_FOUND, `service '${header.service}' v${header.version} not found`);
        return;
      }

      // ── Session discriminator check ─────────────────────────────────
      const isSessionStream = (header.method === '');
      const isSessionService = (svcInfo.scoped === 'session');

      if (isSessionStream !== isSessionService) {
        let peerId = '';
        try { peerId = conn.remoteNodeId(); } catch { /* ignore */ }
        let msg: string;
        if (isSessionService) {
          msg = `'${header.service}' is session-scoped: open a session stream (method='') instead of calling method '${header.method}' directly`;
        } else {
          msg = `'${header.service}' is shared: send a method name instead of opening a session stream (method='')`;
        }
        this.logger.warn(`scope mismatch: ${msg}; peer=${peerId}`);
        await this.writeErrorTrailer(send, StatusCode.FAILED_PRECONDITION, msg);
        return;
      }

      if (isSessionStream) {
        let peerId: string | undefined;
        try { peerId = conn.remoteNodeId(); } catch { /* ignore */ }

        let attributes: Record<string, string> | undefined;
        if (peerId && this.peerStore) {
          const m = this.peerStore.getAttributes(peerId);
          if (m.size > 0) attributes = Object.fromEntries(m);
        }

        const sessionServer = new SessionServer(this.codec, this.interceptors);
        await sessionServer.handleSession(recv, send, svcInfo, header, peerId, attributes);
        try { await send.finish(); } catch { /* best effort */ }
        return;
      }
      // ── End session discriminator ──────────────────────────────────

      // Look up method (shared services only — session dispatch is above)
      const methodInfo = svcInfo.methods.get(header.method);
      if (!methodInfo) {
        await this.writeErrorTrailer(send, StatusCode.UNIMPLEMENTED, `method '${header.service}.${header.method}' not found`);
        return;
      }

      // Validate metadata
      if (header.metadataKeys.length > 0) {
        try {
          validateMetadata(header.metadataKeys, header.metadataValues);
        } catch {
          await this.writeErrorTrailer(send, StatusCode.RESOURCE_EXHAUSTED, 'metadata exceeds limits');
          return;
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
        deadlineEpochMs: header.deadlineEpochMs,
        peer: peerId,
        pattern: methodInfo.pattern as any,
        idempotent: methodInfo.idempotent,
        callId: header.callId || undefined,
        attributes,
      });

      // Dispatch by pattern
      await withRequestContext(
        { service: header.service, method: header.method, requestId: callCtx.callId, peer: peerId },
        () => this.dispatchRpc(svcInfo, methodInfo, callCtx, send, recv),
      );

    } catch (e) {
      if (e instanceof RpcError) {
        await this.writeErrorTrailer(send, e.code, e.message);
      } else {
        this.logger.error('stream handler error', { error: String(e) });
        await this.writeErrorTrailer(send, StatusCode.INTERNAL, String(e));
      }
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
    svcInfo: ServiceInfo, _methodInfo: MethodInfo, handler: Function,
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
    let request = compressed
      ? (this.codec as any).decodeCompressed(reqPayload, true)
      : this.codec.decode(reqPayload);

    // Interceptors
    request = await applyRequestInterceptors(this.interceptors, callCtx, request);

    // Invoke handler with deadline enforcement
    let response: any;
    try {
      response = await this.runWithDeadline(callCtx, () => handler.call(svcInfo.instance, request));
    } catch (e) {
      if (e instanceof RpcError && e.code === StatusCode.DEADLINE_EXCEEDED) {
        await this.writeErrorTrailer(send, StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
        return;
      }
      throw e;
    }
    response = await applyResponseInterceptors(this.interceptors, callCtx, response);

    // Write response + trailer
    const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
    await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    await this.writeOkTrailer(send);
    await send.finish();
  }

  private async handleServerStream(
    svcInfo: ServiceInfo, _methodInfo: MethodInfo, handler: Function,
    callCtx: CallContext, send: ServerSendStream, recv: ServerRecvStream,
  ): Promise<void> {
    const reqFrame = await readFrame(recv, 0);
    if (!reqFrame) {
      await this.writeErrorTrailer(send, StatusCode.UNAVAILABLE, 'stream ended');
      return;
    }
    const [reqPayload, reqFlags] = reqFrame;
    const compressed = !!(reqFlags & COMPRESSED);
    let request = compressed
      ? (this.codec as any).decodeCompressed(reqPayload, true)
      : this.codec.decode(reqPayload);

    request = await applyRequestInterceptors(this.interceptors, callCtx, request);

    const gen = handler.call(svcInfo.instance, request);
    const deadlineMs = Date.now() + this.handlerTimeoutMs(callCtx);
    for await (let response of gen) {
      if (Date.now() > deadlineMs) {
        await this.writeErrorTrailer(send, StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
        return;
      }
      response = await applyResponseInterceptors(this.interceptors, callCtx, response);
      const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
      await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    }

    await this.writeOkTrailer(send);
    await send.finish();
  }

  private async handleClientStream(
    svcInfo: ServiceInfo, _methodInfo: MethodInfo, handler: Function,
    callCtx: CallContext, send: ServerSendStream, recv: ServerRecvStream,
  ): Promise<void> {
    // Collect requests until stream ends
    const requests: unknown[] = [];
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
        ? (this.codec as any).decodeCompressed(payload, true)
        : this.codec.decode(payload);
      request = await applyRequestInterceptors(this.interceptors, callCtx, request);
      requests.push(request);
    }

    // Provide as async iterable
    async function* requestIter() { for (const r of requests) yield r; }

    let response: any;
    try {
      response = await this.runWithDeadline(callCtx, () => handler.call(svcInfo.instance, requestIter()));
    } catch (e) {
      if (e instanceof RpcError && e.code === StatusCode.DEADLINE_EXCEEDED) {
        await this.writeErrorTrailer(send, StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
        return;
      }
      throw e;
    }
    response = await applyResponseInterceptors(this.interceptors, callCtx, response);

    const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
    await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    await this.writeOkTrailer(send);
    await send.finish();
  }

  private async handleBidiStream(
    svcInfo: ServiceInfo, _methodInfo: MethodInfo, handler: Function,
    callCtx: CallContext, send: ServerSendStream, recv: ServerRecvStream,
  ): Promise<void> {
    // Auth interceptors already ran in dispatchRpc() before this method.
    // No need to run them again here.

    // Create request iterable from incoming frames with error capture
    const self = this;
    let readerError: Error | null = null;
    async function* requestIter() {
      try {
        while (true) {
          const frame = await readFrame(recv, 0);
          if (!frame) break;
          const [payload, flags] = frame;
          if (flags & TRAILER) break;
          if (flags & CANCEL) continue;
          const compressed = !!(flags & COMPRESSED);
          const request = compressed
            ? (self.codec as any).decodeCompressed(payload, true)
            : self.codec.decode(payload);
          yield request;
        }
      } catch (e) {
        readerError = e instanceof Error ? e : new Error(String(e));
      }
    }

    const gen = handler.call(svcInfo.instance, requestIter());
    const deadlineMs = Date.now() + this.handlerTimeoutMs(callCtx);
    for await (const response of gen) {
      if (Date.now() > deadlineMs) {
        await this.writeErrorTrailer(send, StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
        return;
      }
      const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
      await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    }

    if (readerError) {
      await this.writeErrorTrailer(send, StatusCode.INTERNAL,
        `bidi stream reader error: ${readerError.message}`);
      return;
    }

    await this.writeOkTrailer(send);
    await send.finish();
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
