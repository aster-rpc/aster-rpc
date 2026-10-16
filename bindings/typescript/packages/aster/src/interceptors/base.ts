/**
 * Base interceptor primitives and helpers.
 *
 * Interceptors form a middleware chain for RPC calls:
 * - Request path: interceptor1 -> interceptor2 -> ... -> handler
 * - Response path: same order
 * - Error path: reverse order (LIFO)
 */

import { AsyncLocalStorage } from 'node:async_hooks';

import { RpcError, StatusCode } from '../status.js';
import type { RpcPattern } from '../types.js';

/**
 * Async-local storage holding the CallContext for the currently-dispatching
 * RPC. Set by the server dispatcher before invoking a handler; used by
 * {@link CallContext.current} to provide implicit access from within
 * handlers and downstream async code.
 */
const _callContextStorage = new AsyncLocalStorage<CallContext>();

/** The `signal` of a context that is not attached to a dispatched call. */
const NEVER_ABORTED: AbortSignal = new AbortController().signal;

/** How the transport learned a call's peer id ({@link CallContext.provenance}). */
export type PeerProvenance = 'iroh_authenticated' | 'other';

/** Peer's UDP socket address when the selected QUIC path is direct. */
export interface PeerAddr {
  host: string;
  port: number;
}

/**
 * Context describing a single RPC invocation.
 * @group Interceptors
 */
export class CallContext {
  service: string;
  method: string;
  callId: string;
  sessionId: string | undefined;
  /**
   * The transport's caller id: the iroh node id over QUIC (see
   * {@link provenance}). Distinct from {@link principal} when an
   * authenticator resolved one.
   */
  peer: string | undefined;
  /**
   * The verified caller: the authenticator's principal when it resolved
   * one, else {@link peer}. Attributes and `requires` are keyed by it.
   */
  principal: string | undefined;
  /** True only for native Iroh endpoint authentication, never an HTTP/WT auth callback. */
  readonly irohAuthenticated: boolean;
  metadata: Record<string, string>;
  private _attributes: Record<string, string>;
  /** Server side: resolves `attributes` on first access when the call carried none. */
  protected _attributesLoader?: () => Record<string, string> | undefined;
  deadline: number | undefined; // Unix seconds
  isStreaming: boolean;
  pattern: RpcPattern | undefined;
  idempotent: boolean;
  attempt: number;
  private _peerAddr: PeerAddr | undefined;
  private _relayUrl: string | undefined;
  private _rttMicros: number | undefined;
  /**
   * Server side: reads the connection's selected path on first access to
   * `peerAddr` / `relayUrl` / `rttMicros` (one native call, only when
   * asked for).
   */
  protected _transport?: () => { peerAddr?: PeerAddr | null; relayUrl?: string | null; rttMicros?: number | null } | null | undefined;

  constructor(init: {
    service: string;
    method: string;
    callId?: string;
    sessionId?: string;
    peer?: string;
    principal?: string;
    irohAuthenticated?: boolean;
    metadata?: Record<string, string>;
    attributes?: Record<string, string>;
    deadline?: number;
    isStreaming?: boolean;
    pattern?: RpcPattern;
    idempotent?: boolean;
    attempt?: number;
    peerAddr?: PeerAddr;
    relayUrl?: string;
    rttMicros?: number;
  }) {
    this.service = init.service;
    this.method = init.method;
    this.callId = init.callId ?? crypto.randomUUID();
    this.sessionId = init.sessionId;
    this.peer = init.peer;
    this.principal = init.principal ?? init.peer;
    this.irohAuthenticated = init.irohAuthenticated ?? false;
    this.metadata = init.metadata ?? {};
    this._attributes = init.attributes ?? {};
    this.deadline = init.deadline;
    this.isStreaming = init.isStreaming ?? false;
    this.pattern = init.pattern;
    this.idempotent = init.idempotent ?? false;
    this.attempt = init.attempt ?? 1;
    this._peerAddr = init.peerAddr;
    this._relayUrl = init.relayUrl;
    this._rttMicros = init.rttMicros;
  }

  /** The caller's attributes (admission / Gate 3), e.g. `aster.role`. */
  get attributes(): Record<string, string> {
    const load = this._attributesLoader;
    if (load) {
      this._attributesLoader = undefined;
      try {
        const a = load();
        if (a) this._attributes = a;
      } catch { /* keep empty */ }
    }
    return this._attributes;
  }
  set attributes(v: Record<string, string>) {
    this._attributesLoader = undefined;
    this._attributes = v;
  }

  private _loadTransport(): void {
    const load = this._transport;
    if (!load) return;
    this._transport = undefined;
    try {
      const snap = load();
      if (snap) {
        if (snap.peerAddr) this._peerAddr = { host: snap.peerAddr.host, port: snap.peerAddr.port };
        this._relayUrl = snap.relayUrl ?? undefined;
        this._rttMicros = snap.rttMicros ?? undefined;
      }
    } catch { /* connection gone */ }
  }

  /**
   * Peer's UDP `{ host, port }` when the selected QUIC path is direct.
   * `undefined` when the call is being relayed.
   */
  get peerAddr(): PeerAddr | undefined {
    this._loadTransport();
    return this._peerAddr;
  }
  set peerAddr(v: PeerAddr | undefined) {
    this._loadTransport();
    this._peerAddr = v;
  }

  /**
   * Relay server URL when the selected path goes through a relay.
   * `undefined` for direct paths. The peer's own IP is not visible
   * through a relay path.
   */
  get relayUrl(): string | undefined {
    this._loadTransport();
    return this._relayUrl;
  }
  set relayUrl(v: string | undefined) {
    this._loadTransport();
    this._relayUrl = v;
  }

  /**
   * Round-trip-time for the selected path in microseconds, or
   * `undefined` if not yet measured.
   */
  get rttMicros(): number | undefined {
    this._loadTransport();
    return this._rttMicros;
  }
  set rttMicros(v: number | undefined) {
    this._loadTransport();
    this._rttMicros = v;
  }

  /**
   * Aborted when the call is cancelled on the server: the peer cancelled or
   * went away, or the deadline (enforced by Rust) passed. Pass it to work
   * the handler starts (`fetch`, timers, child calls). Outside a dispatched
   * call it never aborts.
   */
  get signal(): AbortSignal {
    return NEVER_ABORTED;
  }

  /**
   * How the transport learned {@link peer}: `iroh_authenticated` (the QUIC
   * handshake proved the node id) or `other` (any other transport: treat the
   * id as a hint, never as an authority).
   */
  get provenance(): PeerProvenance {
    return this.irohAuthenticated ? 'iroh_authenticated' : 'other';
  }

  /**
   * Whether the call is over before the handler finished: the peer
   * cancelled it, the stream failed under it, or its deadline passed.
   */
  isCancelled(): boolean {
    return this.signal.aborted || this.expired;
  }

  /**
   * Resolves once {@link isCancelled} becomes true (never for a call that
   * completes normally). Race it against long work for an early exit:
   * `await Promise.race([work(ctx.signal), ctx.cancelled()])`.
   */
  cancelled(): Promise<void> {
    const signal = this.signal;
    if (this.isCancelled()) return Promise.resolve();
    return new Promise<void>((resolve) => signal.addEventListener('abort', () => resolve(), { once: true }));
  }

  /** Seconds until deadline, or undefined if no deadline set. */
  get remainingSeconds(): number | undefined {
    if (this.deadline === undefined) return undefined;
    return Math.max(0, this.deadline - Date.now() / 1000);
  }

  /** True if deadline has passed. */
  get expired(): boolean {
    const remaining = this.remainingSeconds;
    return remaining !== undefined && remaining <= 0;
  }

  /**
   * Return the CallContext for the RPC currently being dispatched, or
   * ``undefined`` when called outside a handler invocation.
   */
  static current(): CallContext | undefined {
    return _callContextStorage.getStore();
  }

  /**
   * Run ``fn`` with this CallContext installed as the current async-local
   * context. Used by the server dispatcher; application code normally uses
   * {@link CallContext.current} to read the context.
   */
  static runWith<R>(ctx: CallContext, fn: () => R): R {
    return _callContextStorage.run(ctx, fn);
  }
}

/**
 * Return true if ``handler`` declares more than one positional parameter,
 * which we take as a signal that it expects a ``CallContext`` as the
 * second argument. TypeScript erases types at runtime, so we cannot
 * inspect the exact parameter type; ``Function.length`` is the only
 * signal available.
 */
export function handlerAcceptsCtx(handler: Function): boolean {
  return (handler as Function).length > 1;
}

/**
 * Base interceptor interface.
 * @group Interceptors
 */
export interface Interceptor {
  onRequest?(ctx: CallContext, request: unknown): Promise<unknown>;
  onResponse?(ctx: CallContext, response: unknown): Promise<unknown>;
  onError?(ctx: CallContext, error: RpcError): Promise<RpcError | null>;
}

/** Convert relative deadline (seconds) to absolute Unix seconds, or undefined. */
export function deadlineFromRelativeSecs(secs: number): number | undefined {
  return secs > 0 ? Date.now() / 1000 + secs : undefined;
}

/** Build a CallContext from common parameters. */
export function buildCallContext(opts: {
  service: string;
  method: string;
  metadata?: Record<string, string>;
  deadlineSecs?: number;
  peer?: string;
  isStreaming?: boolean;
  pattern?: RpcPattern;
  idempotent?: boolean;
  callId?: string;
  sessionId?: string;
  attributes?: Record<string, string>;
  peerAddr?: PeerAddr;
  relayUrl?: string;
  rttMicros?: number;
}): CallContext {
  return new CallContext({
    ...opts,
    deadline: deadlineFromRelativeSecs(opts.deadlineSecs ?? 0),
  });
}

/** Apply request interceptors in order. */
export async function applyRequestInterceptors(
  interceptors: Interceptor[],
  ctx: CallContext,
  request: unknown,
): Promise<unknown> {
  let current = request;
  for (const i of interceptors) {
    if (i.onRequest) current = await i.onRequest(ctx, current);
  }
  return current;
}

/** Apply response interceptors in order. */
export async function applyResponseInterceptors(
  interceptors: Interceptor[],
  ctx: CallContext,
  response: unknown,
): Promise<unknown> {
  let current = response;
  for (const i of interceptors) {
    if (i.onResponse) current = await i.onResponse(ctx, current);
  }
  return current;
}

/** Apply error interceptors in reverse order (LIFO). */
export async function applyErrorInterceptors(
  interceptors: Interceptor[],
  ctx: CallContext,
  error: RpcError,
): Promise<RpcError | null> {
  let current: RpcError | null = error;
  for (let idx = interceptors.length - 1; idx >= 0; idx--) {
    if (current === null) return null;
    const i = interceptors[idx]!;
    if (i.onError) current = await i.onError(ctx, current);
  }
  return current;
}

/** Normalize any error into an RpcError. */
export function normalizeError(error: unknown): RpcError {
  if (error instanceof RpcError) return error;
  if (error instanceof Error && error.name === 'TimeoutError') {
    return new RpcError(StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
  }
  if (error instanceof Error) {
    return new RpcError(StatusCode.UNKNOWN, error.message);
  }
  return new RpcError(StatusCode.UNKNOWN, String(error));
}
