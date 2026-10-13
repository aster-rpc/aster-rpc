import { HttpListener, type HttpListenerOptions, type RpcAuthenticator } from './http-listener.js';
/**
 * dispatch.ts -- JS handlers on `aster::rpc::Server` (plan T4, design
 * `dynamic-dispatch-design.md` §2, §4.3).
 *
 * Rust owns the wire: accept, framing, header decode, routing, sessions
 * (§7.5), Gate 3 (`requires`), deadlines and frame limits. This module owns
 * what only JS can do:
 *
 * - the handler table, indexed `[serviceIdx][methodIdx]` from the lowered
 *   `DynamicService`s (`lowering.ts`);
 * - per-session instances (`session_key` → instance; `close()` hooks on
 *   `SessionClosed`);
 * - the `CallContext` built from the call event, and the interceptor chain
 *   (pre-dispatch with `request = null`, then request, handler, response);
 * - payload decode/encode with the codec;
 * - mapping a handler's outcome to a status (design §2.7): an `RpcError`
 *   passes through, a payload that does not decode is `INVALID_ARGUMENT`,
 *   anything else `INTERNAL`.
 *
 * Deadlines are Rust's: when one passes, Rust answers `DEADLINE_EXCEEDED`
 * and the handler's `ctx.signal` aborts. A peer cancel (streaming calls)
 * aborts it too.
 */

import type { AsterConfig as NativeAsterConfig, IrohConnection } from '@aster-rpc/transport';
import { toNativeConfig, type AsterConfig } from './config.js';
import type { Codec } from './codec.js';
import { createXlangCodec } from './xlang.js';
import { RpcError, StatusCode } from './status.js';
import { RpcPattern } from './types.js';
import type { ServiceInfo, MethodInfo } from './service.js';
import { ServiceRegistry } from './service.js';
import {
  type Interceptor,
  CallContext,
  applyRequestInterceptors,
  applyResponseInterceptors,
} from './interceptors/base.js';
import { withRequestContext, type AsterLogger, createLogger } from './logging.js';
import { withInboundTrace } from './tracing.js';
import { MAX_CLIENT_STREAM_ITEMS } from './limits.js';
import { lowerService } from './lowering.js';
import {
  DispatchLoop,
  DispatchEventKind,
  DispatchFlags,
  CancelReason,
  SubmitResult,
  submitWithBackpressure,
  type DispatchDelivery,
  type DispatchEvent,
  type DispatchServerTuning,
  type NativeDispatchServer,
} from './dispatch-server.js';

const EMPTY = new Uint8Array(0);
const CANCELLED: unique symbol = Symbol('cancelled');
/** Request payloads taken per `recvBatch` crossing (client-stream / bidi). */
const RECV_BATCH = 256;

/** A payload that did not decode: `INVALID_ARGUMENT` (design §2.7). */
class DecodeError extends Error {}

/** One method's handler-table entry. */
interface MethodEntry {
  readonly info: MethodInfo;
  readonly name: string;
  readonly handler: (...args: any[]) => any;
  readonly pattern: string;
  readonly acceptsCtx: boolean;
}

/** One service's handler-table entry. */
interface ServiceEntry {
  readonly info: ServiceInfo;
  readonly session: boolean;
  /** The registered instance (shared services). */
  readonly instance: any;
  /** Per-session instances by Rust `session_key`. */
  readonly sessions: Map<number, any>;
  readonly methods: MethodEntry[];
}

/** Per-call cancellation state, kept while the handler runs. */
class CallState {
  cancelled = 0; // 0 = live; otherwise a CancelReason
  private controller: AbortController | undefined;
  private wake: (() => void) | undefined;
  private cancelledP: Promise<void> | undefined;

  signal(): AbortSignal {
    if (!this.controller) {
      this.controller = new AbortController();
      if (this.cancelled) this.controller.abort(this.abortReason());
    }
    return this.controller.signal;
  }

  /** Resolves when the call is cancelled. */
  whenCancelled(): Promise<void> {
    if (!this.cancelledP) {
      this.cancelledP = this.cancelled
        ? Promise.resolve()
        : new Promise<void>((r) => { this.wake = r; });
    }
    return this.cancelledP;
  }

  cancel(reason: number): void {
    if (this.cancelled) return;
    this.cancelled = reason || CancelReason.Peer;
    this.controller?.abort(this.abortReason());
    this.wake?.();
  }

  private abortReason(): RpcError {
    return this.cancelled === CancelReason.Deadline
      ? new RpcError(StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded')
      : new RpcError(StatusCode.CANCELLED, 'call cancelled');
  }
}

/** The `CallContext` of a dispatched call (lazy transport, cancel signal). */
class DispatchCallContext extends CallContext {
  constructor(
    init: ConstructorParameters<typeof CallContext>[0],
    private readonly state: CallState,
    native: NativeDispatchServer,
    connectionId: number,
    attributes: ((peer: string) => Record<string, string> | undefined) | undefined,
  ) {
    super(init);
    if (connectionId) this._transport = () => native.transportSnapshot(connectionId) as any;
    if (attributes && !init.attributes && init.peer) {
      const peer = init.peer;
      this._attributesLoader = () => attributes(peer);
    }
  }

  override get signal(): AbortSignal {
    return this.state.signal();
  }
}

/** Options for {@link RpcServer}. */
export interface ServerOptions extends DispatchServerTuning {
  /** The services to serve (registered instances). */
  registry: ServiceRegistry;
  /** Payload codec (default: Fory XLANG). */
  codec?: Codec;
  /** Server interceptors, run in order around every handler. */
  interceptors?: Interceptor[];
  logger?: AsterLogger;
  /** Event delivery (default `tsfn-batched`; TSFN variants use a bounded coalesced drain). */
  delivery?: DispatchDelivery;
  /**
   * Connections on non-RPC ALPNs no Rust handler claimed (an application
   * ALPN in `config.alpns`). Without a handler they are closed.
   */
  onNonRpc?: (alpn: string, connection: IrohConnection) => void;
  /** Called once per closed `aster/1` connection. */
  onConnectionClosed?: (connectionId: number, peer: string) => void;
  /**
   * Attribute bridge (design §2.6), for `start(node)` only: Gate 3 also
   * reads each caller's attributes from this gate's admission store, so
   * `requires` admits peers admitted there with attributes. Costs one task
   * per call in Rust.
   */
  gateAttributes?: { native: unknown };
  /**
   * `ctx.attributes` for calls Rust delivered without attributes (no
   * `requires` anywhere): looked up on first access.
   */
  peerAttributes?: (peer: string) => Record<string, string> | undefined;
}

/**
 * Aster RPC server: the registry's services served by `aster::rpc::Server`
 * on a node (started with `aster/1` among its ALPNs).
 *
 * @example
 * ```ts
 * const server = new RpcServer({ registry, codec });
 * await server.start(node);   // or `await server.serve(node)` (resolves on close)
 * ```
 */
export class RpcServer {
  private readonly registry: ServiceRegistry;
  private readonly codec: Codec;
  private readonly interceptors: Interceptor[];
  private readonly logger: AsterLogger;
  private readonly opts: ServerOptions;
  private table: ServiceEntry[] = [];
  private readonly calls = new Map<number, CallState>();
  private loop: DispatchLoop | null = null;
  private native: NativeDispatchServer | null = null;
  private readonly httpListeners = new Set<HttpListener>();
  private closedP: Promise<void> | null = null;
  private resolveClosed: (() => void) | null = null;
  private closing: Promise<void> | null = null;

  constructor(opts: ServerOptions) {
    this.opts = opts;
    this.registry = opts.registry;
    this.codec = opts.codec ?? createXlangCodec();
    this.interceptors = opts.interceptors ?? [];
    this.logger = opts.logger ?? createLogger();
  }

  /** The service registry used by this server. */
  get serviceRegistry(): ServiceRegistry {
    return this.registry;
  }

  /** The native dispatch server (after `start`). */
  get dispatchServer(): NativeDispatchServer | null {
    return this.native;
  }

  /** Calls Rust still tracks (in flight, unanswered). */
  get liveCalls(): number {
    return this.native?.liveCalls() ?? 0;
  }

  /**
   * Attribute bridge (design §2.6): the attributes Gate 3 checks `requires`
   * against for `peer`. `null` clears them.
   */
  setPeerAttributes(peer: string, attributes: Record<string, string> | Map<string, string> | null): void {
    const attrs = attributes instanceof Map ? Object.fromEntries(attributes) : attributes;
    this.native?.setPeerAttributes(peer, attrs && Object.keys(attrs).length ? attrs : null);
  }

  /** Start serving on `node` (the native `Node`, or the `Node` wrapper). */
  async start(node: unknown): Promise<void> {
    if (this.loop) return;
    const nativeNode = (node as { native?: unknown }).native ?? node;
    await this.startLoop({ node: nativeNode });
  }

  /**
   * Start the Rust `AsterServer` from `config` (its own node) and serve on
   * it. This is the path `AsterServer` uses: the configuration's
   * `[enrollment]` section installs Enrollment there, so Gate 0 admits
   * enrolled peers only and `requires` reads each peer's credential roles.
   * `close()` shuts the node down; reach it with `dispatchServer.node()`.
   */
  async startWithConfig(config: AsterConfig): Promise<void> {
    if (this.loop) return;
    await this.startLoop({ config: toNativeConfig(config) });
  }

  private async startLoop(target: { node?: unknown; config?: NativeAsterConfig }): Promise<void> {
    this.table = this.registry.getAllServices().map((info) => this.entryOf(info));
    const services = this.table.map((e) => lowerService(e.info));
    this.closedP = new Promise((r) => { this.resolveClosed = r; });
    this.loop = await DispatchLoop.start({
      ...target,
      services,
      delivery: this.opts.delivery,
      builtinNodeInfo: this.opts.builtinNodeInfo,
      builtinTopology: this.opts.builtinTopology,
      topologyRequires: this.opts.topologyRequires,
      maxSessionsPerConnection: this.opts.maxSessionsPerConnection,
      handlerTimeoutMs: this.opts.handlerTimeoutMs,
      ringCapacity: this.opts.ringCapacity,
      maxForeignInflight: this.opts.maxForeignInflight,
      gateAttributes: target.node ? this.opts.gateAttributes?.native : undefined,
      onCall: (ev, srv) => this.onCall(ev, srv),
      onEvent: (ev, srv) => this.onEvent(ev, srv),
    });
    this.native = this.loop.native;
    this.logger.debug('server started', { services: services.map((s) => s.name).join(',') });
  }

  /** Start serving on `node` and resolve once `close()` is called. */
  async serve(node: unknown): Promise<void> {
    await this.start(node);
    await this.closedP;
  }

  /** Resolves once the server is closed. */
  async waitUntilStopped(): Promise<void> {
    if (this.closedP) await this.closedP;
  }

  /** Start an authenticated PEM HTTPS/optional WebTransport listener. */
  async startHttp(options: HttpListenerOptions, authenticate: RpcAuthenticator): Promise<HttpListener> {
    if (!this.native || this.closing) throw new Error('RPC server is not running');
    const listener = await HttpListener.start(this.native, options, authenticate,
      (closed) => this.httpListeners.delete(closed));
    if (this.closing) {
      await listener.close();
      throw new Error('RPC server is closing');
    }
    this.httpListeners.add(listener);
    return listener;
  }

  /** Explicit anonymous HTTPS/WebTransport. Attribute gates remain enforced; sessions are disabled. */
  async startPublicHttp(options: HttpListenerOptions): Promise<HttpListener> {
    if (!this.native || this.closing) throw new Error('RPC server is not running');
    const listener = await HttpListener.startPublic(this.native, options,
      (closed) => this.httpListeners.delete(closed));
    if (this.closing) {
      await listener.close();
      throw new Error('RPC server is closing');
    }
    this.httpListeners.add(listener);
    return listener;
  }

  /** Stop serving. The node stays up. */
  close(): Promise<void> {
    if (this.closing) return this.closing;
    this.closing = (async () => {
      for (const st of this.calls.values()) st.cancel(CancelReason.Peer);
      if (this.loop) await this.loop.close();
      for (const listener of this.httpListeners) await listener.close();
      this.httpListeners.clear();
      this.resolveClosed?.();
    })();
    return this.closing;
  }

  // ── table ────────────────────────────────────────────────────────────────

  private entryOf(info: ServiceInfo): ServiceEntry {
    const methods: MethodEntry[] = [];
    for (const [name, mi] of info.methods) {
      const handler = mi.handler;
      if (!handler) throw new Error(`${info.name}.${name}: no handler`);
      methods.push({
        info: mi,
        name,
        handler,
        pattern: mi.pattern,
        acceptsCtx: mi.acceptsCtx === true,
      });
    }
    return {
      info,
      session: info.scoped === 'session' || (info.scoped as string) === 'stream',
      instance: info.instance,
      sessions: new Map(),
      methods,
    };
  }

  private instanceFor(svc: ServiceEntry, ev: DispatchEvent): any {
    if (!svc.session) return svc.instance;
    const key = ev.sessionKey;
    let inst = svc.sessions.get(key);
    if (inst === undefined) {
      // NEW_SESSION, or a key not seen yet: a fresh instance of the class.
      const Ctor = (svc.instance as { constructor: new () => any }).constructor;
      inst = new Ctor();
      svc.sessions.set(key, inst);
    }
    return inst;
  }

  private closeSession(key: number): void {
    for (const svc of this.table) {
      const inst = svc.sessions.get(key);
      if (inst === undefined) continue;
      svc.sessions.delete(key);
      const hook = inst?.close ?? inst?.onSessionClose;
      if (typeof hook === 'function') {
        try {
          const r = hook.call(inst);
          if (r && typeof r.catch === 'function') {
            r.catch((e: unknown) => this.logger.warn('session close() failed', { error: String(e) }));
          }
        } catch (e) {
          this.logger.warn('session close() failed', { error: String(e) });
        }
      }
      return;
    }
  }

  // ── events ───────────────────────────────────────────────────────────────

  private onEvent(ev: DispatchEvent, srv: NativeDispatchServer): void {
    switch (ev.kind) {
      case DispatchEventKind.Cancelled:
        this.calls.get(ev.callId)?.cancel(ev.reason);
        break;
      case DispatchEventKind.SessionClosed:
        this.closeSession(ev.sessionKey);
        break;
      case DispatchEventKind.ConnectionClosed:
        this.opts.onConnectionClosed?.(ev.connectionId, ev.peer ?? '');
        break;
      case DispatchEventKind.NonRpc: {
        const conn = srv.takeConnection(ev.connectionId);
        if (!conn) break;
        if (this.opts.onNonRpc) {
          try {
            this.opts.onNonRpc(ev.alpn ?? '', conn);
          } catch (e) {
            this.logger.error('non-RPC connection handler failed', { error: String(e) });
          }
        } else {
          try { conn.close(0, 'unexpected alpn'); } catch { /* ignore */ }
        }
        break;
      }
      default:
        // ResponseWritable / ResponseClosed: this binding waits with
        // `waitWritable` rather than the event-driven reserve.
        break;
    }
  }

  private onCall(ev: DispatchEvent, srv: NativeDispatchServer): void {
    const id = ev.callId;
    const svc = this.table[ev.serviceIdx];
    const m = svc?.methods[ev.methodIdx];
    if (!svc || !m) {
      srv.respond(id, EMPTY, StatusCode.UNIMPLEMENTED, 'method not registered');
      return;
    }
    const state = new CallState();
    this.calls.set(id, state);
    const ctx = new DispatchCallContext(
      {
        service: svc.info.name,
        method: m.name,
        callId: String(ev.wireCallId || id),
        sessionId: ev.sessionId ? String(ev.sessionId) : undefined,
        peer: ev.peer ?? undefined,
        irohAuthenticated: !!(ev.flags & DispatchFlags.IrohAuthenticated),
        metadata: ev.metadata ?? {},
        attributes: ev.attributes ?? undefined,
        deadline: ev.deadlineMs ? ev.deadlineMs / 1000 : undefined,
        isStreaming: m.pattern !== RpcPattern.UNARY,
        pattern: m.pattern as RpcPattern,
        idempotent: m.info.idempotent,
      },
      state,
      srv,
      ev.connectionId,
      (ev.flags & DispatchFlags.IrohAuthenticated) ? this.opts.peerAttributes : undefined,
    );
    let run: Promise<void>;
    try {
      const instance = this.instanceFor(svc, ev);
      run = withRequestContext(
        { service: svc.info.name, method: m.name, requestId: ctx.callId, peer: ctx.peer },
        // The call's server span is the current trace context for the handler.
        () => withInboundTrace(ev.traceparent, ev.baggage, () => this.run(srv, ev, m, instance, ctx, state)),
      );
    } catch (e) {
      run = Promise.reject(e);
    }
    run
      .catch((e) => this.fail(srv, id, m, e, state))
      .finally(() => this.calls.delete(id));
  }

  /** Answer a failed call with its status (design §2.7). */
  private fail(srv: NativeDispatchServer, id: number, m: MethodEntry, e: unknown, state: CallState): void {
    if (state.cancelled) return; // Rust already answered (deadline) or the peer left.
    let code: number = StatusCode.INTERNAL;
    let message: string;
    let details: Record<string, string> | undefined;
    if (e instanceof RpcError) {
      code = e.code;
      message = e.message.replace(/^\[[^\]]+\] /, '');
      details = Object.keys(e.details).length ? e.details : undefined;
    } else if (e instanceof DecodeError) {
      code = StatusCode.INVALID_ARGUMENT;
      message = e.message;
    } else {
      message = e instanceof Error ? e.message : String(e);
      this.logger.error('handler error', { method: m.name, error: String(e) });
    }
    const streaming = m.pattern === RpcPattern.SERVER_STREAM || m.pattern === RpcPattern.BIDI_STREAM;
    const op = streaming
      ? () => srv.finish(id, code, message, details)
      : () => srv.respond(id, EMPTY, code, message, details);
    const r = op();
    if (r > 0) void submitWithBackpressure(srv, id, op);
  }

  private decode(payload: Uint8Array, type: unknown): unknown {
    try {
      return this.codec.decode(payload, type);
    } catch (e) {
      throw new DecodeError(`request payload did not decode: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  private async run(
    srv: NativeDispatchServer,
    ev: DispatchEvent,
    m: MethodEntry,
    instance: any,
    ctx: DispatchCallContext,
    state: CallState,
  ): Promise<void> {
    const id = ev.callId;
    const ints = this.interceptors;
    if (ints.length) await applyRequestInterceptors(ints, ctx, null);
    const reqType = m.info.requestType;
    const respType = m.info.responseType;
    const call = (arg: unknown) =>
      CallContext.runWith(ctx, () => (m.acceptsCtx ? m.handler.call(instance, arg, ctx) : m.handler.call(instance, arg)));

    switch (m.pattern) {
      case RpcPattern.UNARY: {
        let request = this.decode(ev.request!, reqType);
        if (ints.length) request = await applyRequestInterceptors(ints, ctx, request);
        let response = await call(request);
        if (ints.length) response = await applyResponseInterceptors(ints, ctx, response);
        return this.respond(srv, id, this.codec.encode(response, respType));
      }
      case RpcPattern.CLIENT_STREAM: {
        let response = await call(this.requests(srv, ev, reqType, ctx, true));
        if (ints.length) response = await applyResponseInterceptors(ints, ctx, response);
        return this.respond(srv, id, this.codec.encode(response, respType));
      }
      case RpcPattern.SERVER_STREAM: {
        let request = this.decode(ev.request!, reqType);
        if (ints.length) request = await applyRequestInterceptors(ints, ctx, request);
        return this.stream(srv, id, call(request), respType, ctx, state);
      }
      case RpcPattern.BIDI_STREAM:
        return this.stream(srv, id, call(this.requests(srv, ev, reqType, ctx, false)), respType, ctx, state);
      default:
        throw new RpcError(StatusCode.INTERNAL, `unknown pattern: ${m.pattern}`);
    }
  }

  private respond(srv: NativeDispatchServer, id: number, payload: Uint8Array): void | Promise<void> {
    const r = srv.respond(id, payload);
    if (r > 0) {
      return submitWithBackpressure(srv, id, () => srv.respond(id, payload)).then(() => undefined);
    }
  }

  /** The request stream: the event's first request, then `recv` until the end. */
  private async *requests(
    srv: NativeDispatchServer,
    ev: DispatchEvent,
    reqType: unknown,
    ctx: CallContext,
    intercept: boolean,
  ): AsyncGenerator<unknown> {
    const ints = intercept ? this.interceptors : [];
    let n = 0;
    let batch: Uint8Array[] = ev.request ? [ev.request] : [];
    const firstIsEnd = (ev.flags & DispatchFlags.FirstIsEnd) !== 0;
    while (batch.length) {
      for (const payload of batch) {
        if (++n > MAX_CLIENT_STREAM_ITEMS) {
          throw new RpcError(StatusCode.RESOURCE_EXHAUSTED, `client stream exceeded ${MAX_CLIENT_STREAM_ITEMS} items`);
        }
        let request = this.decode(payload, reqType);
        if (ints.length) request = await applyRequestInterceptors(ints, ctx, request);
        yield request;
      }
      if (n === 1 && firstIsEnd) return;
      // One crossing per burst of requests already received.
      batch = await srv.recvBatch(ev.callId, RECV_BATCH);
    }
  }

  /** Drive a handler's async iterable of responses, then the trailer. */
  private async stream(
    srv: NativeDispatchServer,
    id: number,
    source: AsyncIterable<unknown> | Promise<AsyncIterable<unknown>>,
    respType: unknown,
    ctx: CallContext,
    state: CallState,
  ): Promise<void> {
    const ints = this.interceptors;
    const iterable = await source;
    const it = iterable[Symbol.asyncIterator]();
    const cancelled = state.whenCancelled().then((): typeof CANCELLED => CANCELLED);
    try {
      while (true) {
        const step = await Promise.race([it.next(), cancelled]);
        if (step === CANCELLED || state.cancelled) return;
        if (step.done) break;
        let item = step.value;
        if (ints.length) item = await applyResponseInterceptors(ints, ctx, item);
        const payload = this.codec.encode(item, respType);
        let r = srv.send(id, payload);
        if (r > 0) r = await submitWithBackpressure(srv, id, () => srv.send(id, payload));
        if (r < 0) {
          // Closed / gone (peer cancel, deadline): stop producing.
          if (r === SubmitResult.Invalid) throw new RpcError(StatusCode.INTERNAL, 'empty stream item');
          return;
        }
      }
      let r = srv.finish(id);
      if (r > 0) r = await submitWithBackpressure(srv, id, () => srv.finish(id));
    } finally {
      if (state.cancelled) {
        // Let the generator run its `finally` blocks.
        void Promise.resolve(it.return?.()).catch(() => {});
      }
    }
  }
}
