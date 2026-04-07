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
import { validateMetadata } from './limits.js';

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
  private _serving = false;
  private _connections = new Set<ServerConnection>();

  constructor(opts: ServerOptions) {
    this.registry = opts.registry;
    this.codec = opts.codec ?? new JsonCodec();
    this.interceptors = opts.interceptors ?? [];
    this.logger = opts.logger ?? createLogger();
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
  private async handleConnection(conn: ServerConnection): Promise<void> {
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

      // Look up method
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

      const callCtx = buildCallContext({
        service: header.service,
        method: header.method,
        metadata: this.buildMetadata(header),
        deadlineEpochMs: header.deadlineEpochMs,
        peer: peerId,
        pattern: methodInfo.pattern as any,
        idempotent: methodInfo.idempotent,
        callId: header.callId || undefined,
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

    // Invoke handler
    let response = await handler.call(svcInfo.instance, request);
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
    for await (let response of gen) {
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
      if (flags & TRAILER) break;
      if (flags & CANCEL) continue;
      const compressed = !!(flags & COMPRESSED);
      let request = compressed
        ? (this.codec as any).decodeCompressed(payload, true)
        : this.codec.decode(payload);
      request = await applyRequestInterceptors(this.interceptors, callCtx, request);
      requests.push(request);
    }

    // Provide as async iterable
    async function* requestIter() { for (const r of requests) yield r; }

    let response = await handler.call(svcInfo.instance, requestIter());
    response = await applyResponseInterceptors(this.interceptors, callCtx, response);

    const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
    await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    await this.writeOkTrailer(send);
    await send.finish();
  }

  private async handleBidiStream(
    svcInfo: ServiceInfo, _methodInfo: MethodInfo, handler: Function,
    _callCtx: CallContext, send: ServerSendStream, recv: ServerRecvStream,
  ): Promise<void> {
    // Create request iterable from incoming frames
    const self = this;
    async function* requestIter() {
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
    }

    const gen = handler.call(svcInfo.instance, requestIter());
    for await (const response of gen) {
      const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
      await writeFrame(send, respPayload, respCompressed ? COMPRESSED : 0);
    }

    await this.writeOkTrailer(send);
    await send.finish();
  }

  // -- Helpers ----------------------------------------------------------------

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
