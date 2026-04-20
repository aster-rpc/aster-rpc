/**
 * AsterServer2 — reactor-driven, multiplexed-streams server (spec §6 / §7.5).
 *
 * Port of Java `site.aster.server.AsterServer` (resolveInstance, ConnectionState,
 * session graveyard + per-connection cap). Drives the napi reactor surface
 * (see `bindings/typescript/native/src/reactor.rs`).
 *
 * Scope for Session 1: single reactor dispatch path, all four RPC patterns,
 * session resolution with the full spec §6 check order (FAILED_PRECONDITION /
 * NOT_FOUND / RESOURCE_EXHAUSTED), and per-connection cleanup on
 * ConnectionClosed events. Interceptor parity with v1 lands in Session 2.
 */

import type { Codec } from './codec.js';
import { JsonCodec } from './codec.js';
import {
  encodeFrame,
  COMPRESSED,
  END_STREAM,
  TRAILER,
} from './framing.js';
import { RpcPattern } from './types.js';
import { StreamHeader, RpcStatus } from './protocol.js';
import { StatusCode, RpcError } from './status.js';
import type { ServiceInfo, MethodInfo } from './service.js';

// -- Native-shape structural interfaces --------------------------------------

/** Structural shape of the napi `IrohNode` (subset we need). */
export interface NativeNode {
  nodeId(): string;
}

/** Structural shape of the napi `ReactorResponseSender`. */
export interface NativeResponseSender {
  submit(responseFrame: Uint8Array, trailerFrame: Uint8Array): void;
  sendFrame(frame: Uint8Array): void;
  sendTrailer(trailerFrame: Uint8Array): void;
}

/** Structural shape of the napi `ReactorRequestReceiver`. */
export interface NativeRequestReceiver {
  recv(): Promise<{ payload: Uint8Array; flags: number; done: boolean }>;
}

/** Structural shape of the napi `ReactorCancelFlag`. */
export interface NativeCancelFlag {
  readonly isCancelled: boolean;
}

/** Structural shape of the napi `ReactorEvent`. */
export interface NativeReactorEvent {
  readonly kind: 'call' | 'connection_closed';
  readonly connectionId: bigint;
  readonly peerId: string;
  readonly callId: bigint;
  readonly headerPayload: Uint8Array | null;
  readonly headerFlags: number;
  readonly requestPayload: Uint8Array | null;
  readonly requestFlags: number;
  takeSender(): NativeResponseSender | null;
  takeRequestReceiver(): NativeRequestReceiver | null;
  takeCancelFlag(): NativeCancelFlag | null;
  readonly closeKind: string | null;
  readonly closeCode: bigint | null;
  readonly closeReason: Uint8Array | null;
}

/** Structural shape of the napi `ReactorHandle`. */
export interface NativeReactorHandle {
  nextEvent(): Promise<NativeReactorEvent | null>;
}

/** Factory for constructing a reactor over an `IrohNode`. The caller
 *  injects `startReactor` from `@aster-rpc/transport`. */
export type StartReactorFn = (
  node: NativeNode,
  channelCapacity: number,
) => NativeReactorHandle;

// -- Service registration ----------------------------------------------------

/** A registered shared-scope service. */
export interface SharedRegistration {
  info: ServiceInfo;
  /** Singleton instance used for every call. */
  instance: object;
}

/** A registered session-scope service. The factory is invoked lazily on
 *  first-use per `(connectionId, sessionId)` and the resulting instance
 *  is cached in `state.sessionInstances`. */
export interface SessionRegistration {
  info: ServiceInfo;
  /** Invoked lazily on first-use per (connectionId, sessionId). The
   *  returned object is cached and reused for every subsequent call on
   *  that session. If it exposes an `onClose()` method, the server calls
   *  it when the underlying QUIC connection drops. */
  factory: (connectionId: bigint, sessionId: number, peerId: string) => object;
}

interface InternalRegistration {
  info: ServiceInfo;
  instance: object | null;
  factory:
    | ((connectionId: bigint, sessionId: number, peerId: string) => object)
    | null;
}

// -- Per-connection state ----------------------------------------------------

function cacheKey(sessionId: number, implKey: string): string {
  return `${sessionId}::${implKey}`;
}

class ConnectionState {
  readonly activeSessions = new Set<string>();
  lastOpenedSessionId = 0;
  readonly maxSessions: number;
  readonly sessionInstances = new Map<string, object>();

  constructor(maxSessions: number) {
    this.maxSessions = maxSessions;
  }
}

// -- Error types -------------------------------------------------------------

class SessionScopeMismatchError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'SessionScopeMismatchError';
  }
}

class SessionNotFoundError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'SessionNotFoundError';
  }
}

class SessionLimitError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'SessionLimitError';
  }
}

// -- Server ------------------------------------------------------------------

/** Default per-connection session cap (spec §7.5). */
export const DEFAULT_MAX_SESSIONS_PER_CONNECTION = 1024;

/** Options for [`AsterServer2`]. */
export interface AsterServer2Options {
  node: NativeNode;
  startReactor: StartReactorFn;
  codec?: Codec;
  shared?: readonly SharedRegistration[];
  session?: readonly SessionRegistration[];
  maxSessionsPerConnection?: number;
  channelCapacity?: number;
}

/**
 * Reactor-driven Aster server. Replaces v1's `RpcServer` for the
 * multiplexed-streams architecture. Session 1 scope: single dispatch
 * path, spec-faithful session resolution, four-pattern dispatch. Session
 * 2 adds interceptor wiring + logger / metrics parity with v1.
 */
export class AsterServer2 {
  private readonly node: NativeNode;
  private readonly codec: Codec;
  private readonly handle: NativeReactorHandle;
  private readonly services = new Map<string, InternalRegistration>();
  private readonly maxSessionsPerConnection: number;
  private readonly connections = new Map<string, ConnectionState>();
  private running = true;
  private pollPromise: Promise<void> | null = null;
  private stopResolve: (() => void) | null = null;
  private readonly stopPromise: Promise<void>;

  constructor(opts: AsterServer2Options) {
    this.node = opts.node;
    this.codec = opts.codec ?? new JsonCodec();
    this.maxSessionsPerConnection =
      opts.maxSessionsPerConnection ?? DEFAULT_MAX_SESSIONS_PER_CONNECTION;

    for (const entry of opts.shared ?? []) {
      this.services.set(entry.info.name, {
        info: entry.info,
        instance: entry.instance,
        factory: null,
      });
    }
    for (const entry of opts.session ?? []) {
      this.services.set(entry.info.name, {
        info: entry.info,
        instance: null,
        factory: entry.factory,
      });
    }

    this.stopPromise = new Promise<void>((resolve) => {
      this.stopResolve = resolve;
    });

    this.handle = opts.startReactor(this.node, opts.channelCapacity ?? 256);
  }

  /** Node id (hex). */
  get nodeId(): string {
    return this.node.nodeId();
  }

  /** Kick off the poll loop. Resolves on successful launch; the loop
   *  runs in the background until [`close`] is called. */
  start(): void {
    if (this.pollPromise !== null) return;
    this.pollPromise = this.pollLoop();
  }

  /** Wait until `close()` has been called and the poll loop has exited. */
  async waitUntilStopped(): Promise<void> {
    await this.stopPromise;
  }

  /**
   * **TEST-ONLY**. Snapshot of per-connection state for assertions in
   * chaos tests. Returns a map from opaque connection key → tuple of
   * (activeSessionCount, lastOpenedSessionId). Production code must
   * NOT read this — it is only exposed so tier-2 tests can verify
   * reap semantics (connection entries drop on close) without
   * peeking at private fields.
   */
  debugConnectionSnapshot(): ReadonlyMap<
    string,
    { activeSessionCount: number; lastOpenedSessionId: number }
  > {
    const out = new Map<
      string,
      { activeSessionCount: number; lastOpenedSessionId: number }
    >();
    for (const [key, state] of this.connections) {
      out.set(key, {
        activeSessionCount: state.activeSessions.size,
        lastOpenedSessionId: state.lastOpenedSessionId,
      });
    }
    return out;
  }

  /** Stop accepting new calls. The poll loop exits after the current
   *  `nextEvent()` resolves (there's no cancel hook in the napi surface
   *  yet, so the exit is cooperative). */
  close(): void {
    if (!this.running) return;
    this.running = false;
    this.stopResolve?.();
    this.stopResolve = null;
  }

  private async pollLoop(): Promise<void> {
    while (this.running) {
      let event: NativeReactorEvent | null;
      try {
        event = await this.handle.nextEvent();
      } catch (e) {
        if (!this.running) return;
        // Unrecoverable — surface to stderr and exit the loop.
        // Session 2 will thread this through AsterLogger.
        console.error('[AsterServer2] reactor poll error:', e);
        return;
      }
      if (event === null) return;
      if (event.kind === 'connection_closed') {
        this.handleConnectionClosed(event);
        continue;
      }
      // Dispatch in the background so one slow handler doesn't block
      // the poll loop. The napi channel already applies backpressure
      // via its bounded capacity.
      this.dispatchCall(event).catch((e) => {
        console.error('[AsterServer2] dispatch error:', e);
      });
    }
  }

  private handleConnectionClosed(event: NativeReactorEvent): void {
    const key = connectionKey(event.connectionId);
    const state = this.connections.get(key);
    if (state === undefined) return;
    this.connections.delete(key);
    // Best-effort: invoke `onClose()` on each cached session instance.
    for (const instance of state.sessionInstances.values()) {
      const maybeClose = (instance as { onClose?: () => void | Promise<void> })
        .onClose;
      if (typeof maybeClose === 'function') {
        try {
          const result = maybeClose.call(instance);
          if (result && typeof (result as Promise<unknown>).then === 'function') {
            (result as Promise<unknown>).catch(() => {
              // Session onClose hooks are fire-and-forget.
            });
          }
        } catch {
          // Best effort.
        }
      }
    }
    state.sessionInstances.clear();
  }

  // ── Dispatch ────────────────────────────────────────────────────────────

  private async dispatchCall(event: NativeReactorEvent): Promise<void> {
    const sender = event.takeSender();
    if (sender === null) {
      // Non-Call event slipped through — shouldn't happen since we
      // branched on `kind` above, but be defensive.
      return;
    }

    // Decode the StreamHeader from the inline header payload.
    let header: StreamHeader;
    try {
      if (event.headerPayload === null || event.headerPayload.byteLength === 0) {
        throw new Error('missing StreamHeader payload on inbound call');
      }
      const decoded = this.codec.decode(event.headerPayload);
      header = decoded as StreamHeader;
    } catch (e) {
      this.submitErrorTrailer(
        sender,
        StatusCode.INVALID_ARGUMENT,
        `failed to decode StreamHeader: ${e instanceof Error ? e.message : String(e)}`,
      );
      return;
    }

    const reg = this.services.get(header.service);
    if (reg === undefined) {
      this.submitErrorTrailer(
        sender,
        StatusCode.UNIMPLEMENTED,
        `unknown service: ${header.service}`,
      );
      return;
    }

    const method = reg.info.methods.get(header.method);
    if (method === undefined) {
      this.submitErrorTrailer(
        sender,
        StatusCode.UNIMPLEMENTED,
        `unknown method: ${header.service}/${header.method}`,
      );
      return;
    }

    let instance: object;
    try {
      instance = this.resolveInstance(reg, event, header);
    } catch (e) {
      if (e instanceof SessionScopeMismatchError) {
        this.submitErrorTrailer(sender, StatusCode.FAILED_PRECONDITION, e.message);
        return;
      }
      if (e instanceof SessionNotFoundError) {
        this.submitErrorTrailer(sender, StatusCode.NOT_FOUND, e.message);
        return;
      }
      if (e instanceof SessionLimitError) {
        this.submitErrorTrailer(sender, StatusCode.RESOURCE_EXHAUSTED, e.message);
        return;
      }
      throw e;
    }

    try {
      switch (method.pattern) {
        case RpcPattern.UNARY:
          await this.handleUnary(instance, method, event, sender);
          break;
        case RpcPattern.SERVER_STREAM:
          await this.handleServerStream(instance, method, event, sender);
          break;
        case RpcPattern.CLIENT_STREAM:
          await this.handleClientStream(instance, method, event, sender);
          break;
        case RpcPattern.BIDI_STREAM:
          await this.handleBidiStream(instance, method, event, sender);
          break;
        default:
          this.submitErrorTrailer(
            sender,
            StatusCode.INTERNAL,
            `unknown pattern: ${method.pattern}`,
          );
      }
    } catch (e) {
      this.submitThrownError(sender, e);
    }
  }

  /** Multiplexed-streams spec §6 / §7.5 lookup-or-create. Ports Java
   *  `AsterServer.resolveInstance` verbatim. */
  private resolveInstance(
    reg: InternalRegistration,
    event: NativeReactorEvent,
    header: StreamHeader,
  ): object {
    const sessionId = header.sessionId ?? 0;
    const isSessionScope = reg.info.scoped === 'session';

    if (!isSessionScope) {
      if (sessionId !== 0) {
        throw new SessionScopeMismatchError(
          `service '${reg.info.name}' is SHARED but call carried sessionId=${sessionId}`,
        );
      }
      if (reg.instance === null) {
        throw new SessionScopeMismatchError(
          `service '${reg.info.name}' has no singleton instance registered`,
        );
      }
      return reg.instance;
    }

    if (sessionId === 0) {
      throw new SessionScopeMismatchError(
        `service '${reg.info.name}' is SESSION-scoped; call must carry a non-zero sessionId`,
      );
    }

    if (reg.factory === null) {
      throw new SessionScopeMismatchError(
        `service '${reg.info.name}' is SESSION-scoped but no factory was registered`,
      );
    }

    const connKey = connectionKey(event.connectionId);
    let state = this.connections.get(connKey);
    if (state === undefined) {
      state = new ConnectionState(this.maxSessionsPerConnection);
      this.connections.set(connKey, state);
    }
    const implKey = reg.info.name;
    const key = cacheKey(sessionId, implKey);

    if (state.activeSessions.has(key)) {
      const cached = state.sessionInstances.get(key);
      if (cached !== undefined) return cached;
      // Active-set says yes but cache miss — shouldn't happen, but if
      // it does, recreate lazily via the factory.
      const instance = reg.factory(event.connectionId, sessionId, event.peerId);
      state.sessionInstances.set(key, instance);
      return instance;
    }
    if (sessionId <= state.lastOpenedSessionId) {
      throw new SessionNotFoundError(
        `session ${sessionId} was previously opened on this connection and is now closed`,
      );
    }
    // Spec §7.5: cap counts active sessions only; a rejected fresh id
    // does NOT bump the graveyard counter, so a retry with the same id
    // surfaces RESOURCE_EXHAUSTED again rather than NOT_FOUND.
    if (state.activeSessions.size >= state.maxSessions) {
      throw new SessionLimitError(
        `connection has reached max_sessions_per_connection=${state.maxSessions}`,
      );
    }
    state.lastOpenedSessionId = sessionId;
    state.activeSessions.add(key);
    const instance = reg.factory(event.connectionId, sessionId, event.peerId);
    state.sessionInstances.set(key, instance);
    return instance;
  }

  // ── Pattern dispatchers ─────────────────────────────────────────────────

  private async handleUnary(
    instance: object,
    method: MethodInfo,
    event: NativeReactorEvent,
    sender: NativeResponseSender,
  ): Promise<void> {
    if (event.requestPayload === null) {
      this.submitErrorTrailer(
        sender,
        StatusCode.INVALID_ARGUMENT,
        'unary call missing inline request frame',
      );
      return;
    }
    const compressed = (event.requestFlags & COMPRESSED) !== 0;
    const request = compressed
      ? this.codec.decodeCompressed(event.requestPayload, true, method.requestType)
      : this.codec.decode(event.requestPayload, method.requestType);

    const handler = method.handler;
    if (!handler) {
      this.submitErrorTrailer(sender, StatusCode.INTERNAL, 'no handler');
      return;
    }
    const response = await handler.call(instance, request);
    const [respPayload, respCompressed] = this.codec.encodeCompressed(response);
    const respFrame = encodeFrame(respPayload, respCompressed ? COMPRESSED : 0);
    const trailerFrame = encodeFrame(this.codec.encode(new RpcStatus()), TRAILER);
    sender.submit(respFrame, trailerFrame);
  }

  private async handleServerStream(
    instance: object,
    method: MethodInfo,
    event: NativeReactorEvent,
    sender: NativeResponseSender,
  ): Promise<void> {
    if (event.requestPayload === null) {
      this.submitErrorTrailer(
        sender,
        StatusCode.INVALID_ARGUMENT,
        'server-stream call missing inline request frame',
      );
      return;
    }
    const compressed = (event.requestFlags & COMPRESSED) !== 0;
    const request = compressed
      ? this.codec.decodeCompressed(event.requestPayload, true, method.requestType)
      : this.codec.decode(event.requestPayload, method.requestType);

    const handler = method.handler;
    if (!handler) {
      this.submitErrorTrailer(sender, StatusCode.INTERNAL, 'no handler');
      return;
    }
    const gen = handler.call(instance, request) as AsyncIterable<unknown>;
    try {
      for await (const response of gen) {
        const [payload, compressed2] = this.codec.encodeCompressed(response);
        sender.sendFrame(encodeFrame(payload, compressed2 ? COMPRESSED : 0));
      }
    } catch (e) {
      this.submitStreamingError(sender, e);
      return;
    }
    sender.sendTrailer(encodeFrame(this.codec.encode(new RpcStatus()), TRAILER));
  }

  private async handleClientStream(
    instance: object,
    method: MethodInfo,
    event: NativeReactorEvent,
    sender: NativeResponseSender,
  ): Promise<void> {
    const handler = method.handler;
    if (!handler) {
      this.submitErrorTrailer(sender, StatusCode.INTERNAL, 'no handler');
      return;
    }
    const receiver = event.takeRequestReceiver();
    const codec = this.codec;
    const requestType = method.requestType;

    const firstPayload = event.requestPayload;
    const firstFlags = event.requestFlags;

    async function* requestIter(): AsyncIterable<unknown> {
      // First request frame arrives inline on the event.
      if (firstPayload !== null && firstPayload.byteLength > 0) {
        const firstCompressed = (firstFlags & COMPRESSED) !== 0;
        yield firstCompressed
          ? codec.decodeCompressed(firstPayload, true, requestType)
          : codec.decode(firstPayload, requestType);
      }
      if ((firstFlags & END_STREAM) !== 0) return;
      if (receiver === null) return;
      while (true) {
        const frame = await receiver.recv();
        if (frame.done) return;
        const compressed = (frame.flags & COMPRESSED) !== 0;
        yield compressed
          ? codec.decodeCompressed(frame.payload, true, requestType)
          : codec.decode(frame.payload, requestType);
        if ((frame.flags & END_STREAM) !== 0) return;
      }
    }

    try {
      const response = await handler.call(instance, requestIter());
      const [payload, compressed] = codec.encodeCompressed(response);
      const respFrame = encodeFrame(payload, compressed ? COMPRESSED : 0);
      const trailerFrame = encodeFrame(codec.encode(new RpcStatus()), TRAILER);
      sender.submit(respFrame, trailerFrame);
    } catch (e) {
      this.submitThrownError(sender, e);
    }
  }

  private async handleBidiStream(
    instance: object,
    method: MethodInfo,
    event: NativeReactorEvent,
    sender: NativeResponseSender,
  ): Promise<void> {
    const handler = method.handler;
    if (!handler) {
      this.submitErrorTrailer(sender, StatusCode.INTERNAL, 'no handler');
      return;
    }
    const receiver = event.takeRequestReceiver();
    const codec = this.codec;
    const requestType = method.requestType;

    const firstPayload = event.requestPayload;
    const firstFlags = event.requestFlags;

    async function* requestIter(): AsyncIterable<unknown> {
      if (firstPayload !== null && firstPayload.byteLength > 0) {
        const firstCompressed = (firstFlags & COMPRESSED) !== 0;
        yield firstCompressed
          ? codec.decodeCompressed(firstPayload, true, requestType)
          : codec.decode(firstPayload, requestType);
      }
      if ((firstFlags & END_STREAM) !== 0) return;
      if (receiver === null) return;
      while (true) {
        const frame = await receiver.recv();
        if (frame.done) return;
        const compressed = (frame.flags & COMPRESSED) !== 0;
        yield compressed
          ? codec.decodeCompressed(frame.payload, true, requestType)
          : codec.decode(frame.payload, requestType);
        if ((frame.flags & END_STREAM) !== 0) return;
      }
    }

    try {
      const gen = handler.call(instance, requestIter()) as AsyncIterable<unknown>;
      for await (const response of gen) {
        const [payload, compressed] = codec.encodeCompressed(response);
        sender.sendFrame(encodeFrame(payload, compressed ? COMPRESSED : 0));
      }
      sender.sendTrailer(encodeFrame(codec.encode(new RpcStatus()), TRAILER));
    } catch (e) {
      this.submitStreamingError(sender, e);
    }
  }

  // ── Error helpers ───────────────────────────────────────────────────────

  private submitErrorTrailer(
    sender: NativeResponseSender,
    code: number,
    message: string,
  ): void {
    const status = new RpcStatus({ code, message });
    const trailer = encodeFrame(this.codec.encode(status), TRAILER);
    try {
      sender.submit(new Uint8Array(0), trailer);
    } catch {
      // Best effort.
    }
  }

  private submitStreamingError(sender: NativeResponseSender, e: unknown): void {
    const [code, message] = this.extractErrorStatus(e);
    const status = new RpcStatus({ code, message });
    try {
      sender.sendTrailer(encodeFrame(this.codec.encode(status), TRAILER));
    } catch {
      // Best effort.
    }
  }

  private submitThrownError(sender: NativeResponseSender, e: unknown): void {
    const [code, message] = this.extractErrorStatus(e);
    this.submitErrorTrailer(sender, code, message);
  }

  private extractErrorStatus(e: unknown): [number, string] {
    if (e instanceof RpcError) return [e.code, e.message];
    if (e instanceof Error) return [StatusCode.INTERNAL, e.message];
    return [StatusCode.INTERNAL, String(e)];
  }
}

function connectionKey(id: bigint): string {
  return id.toString();
}
