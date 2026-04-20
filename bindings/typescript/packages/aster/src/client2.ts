/**
 * AsterClient2 — multiplexed-streams client (spec §6).
 *
 * Minimal client surface focused on the per-connection session layer.
 * Given an already-opened `NativeConnection`, allocates monotonic
 * per-(peer, rpcAddr) sessionIds and hands out `ClientSession`s that
 * each bind a new `IrohTransport2` routed through the per-session pool.
 *
 * The full admission / discovery layer stays in `AsterClientWrapper`
 * (runtime.ts) for Session 1; Session 2 migrates that wrapper to use
 * `AsterClient2` under the hood. This file is the minimum surface the
 * smoke test + ClientSession tests need to exercise the multiplexed
 * pool end-to-end.
 *
 * Mirrors Java `site.aster.client.ClientSession` and Python
 * `aster.runtime.ClientSession`.
 */

import type { Codec } from './codec.js';
import { JsonCodec } from './codec.js';
import { createXlangCodec } from './xlang.js';
import { createClient, type AsterClient, type ClientOptions } from './client.js';
import { RpcError, StatusCode } from './status.js';
import {
  IrohTransport2,
  type AsterCallFactory,
  type NativeConnection,
} from './transport/iroh2.js';
import type { ServiceSummary } from './trust/consumer.js';

export type { ServiceSummary } from './trust/consumer.js';

/** Decide which codec to use based on server capabilities.
 *  Mirrors `runtime.py:1803` JsonProxyCodec auto-pick. The
 *  advertised modes are strings ("xlang", "json", "row", "native") as
 *  carried on `ServiceSummary.serializationModes` from admission.
 *
 *  ForyCodec (XLANG) is preferred when the server advertises xlang support.
 *  JsonCodec is used only when the server is JSON-only. */
function pickCodec(
  explicit: Codec | undefined,
  services: readonly ServiceSummary[] | undefined,
): Codec {
  if (explicit) return explicit;
  if (services && services.length > 0) {
    const allJsonOnly = services.every((svc) => {
      const modes = svc.serializationModes;
      if (!modes || modes.length === 0) return false;
      return modes.includes('json') && !modes.includes('xlang');
    });
    if (allJsonOnly) return new JsonCodec();
  }
  // Default to ForyCodec (XLANG) — the TS binding now speaks Fory natively.
  // Callers that need JSON pass it explicitly via `codec:`.
  return createXlangCodec();
}

/** Options for [`AsterClient2`]. */
export interface AsterClient2Options {
  /** Already-opened native QUIC connection to the producer. */
  connection: NativeConnection;
  /** napi `AsterCall` factory (the class imported from
   *  `@aster-rpc/transport`). Injected so tests can substitute a fake. */
  asterCall: AsterCallFactory;
  /** Optional codec override. If omitted, derived from `services` or
   *  defaults to [`JsonCodec`]. */
  codec?: Codec;
  /** Service summaries advertised by the producer. Used for codec
   *  auto-pick when `codec` is not provided. */
  services?: readonly ServiceSummary[];
  /** Stable key identifying the (peer, rpcAddr) the connection targets.
   *  Used as the session-id counter namespace. Defaults to a single
   *  synthetic key, which is fine when one `AsterClient2` instance
   *  holds exactly one connection. */
  rpcAddrKey?: string;
}

/** Options for opening a session. */
export interface OpenSessionOptions<T> {
  /** Service class the session will drive. When passed together with
   *  [`ClientSession.getClient`], the session becomes a typed stub
   *  bound to this service. */
  serviceClass?: new (...args: any[]) => T;
}

/**
 * Multiplexed-streams client. One instance wraps exactly one native
 * connection and hands out [`ClientSession`]s pinned to it.
 */
export class AsterClient2 {
  private readonly connection: NativeConnection;
  private readonly asterCall: AsterCallFactory;
  private readonly codec: Codec;
  private readonly services: readonly ServiceSummary[] | undefined;
  private readonly rpcAddrKey: string;
  // Per-(peer, rpcAddr) monotonic sessionId counter. Mirrors Python's
  // `AsterClient._next_session_id` and Java's `IrohConnection.nextSessionId`.
  // Sessionid 0 is reserved for the SHARED pool; the counter starts at 1.
  private readonly sessionIdCounters = new Map<string, number>();

  constructor(opts: AsterClient2Options) {
    this.connection = opts.connection;
    this.asterCall = opts.asterCall;
    this.codec = pickCodec(opts.codec, opts.services);
    this.services = opts.services;
    this.rpcAddrKey = opts.rpcAddrKey ?? '__default__';
  }

  /** The underlying native QUIC connection. Useful for diagnostics. */
  get nativeConnection(): NativeConnection {
    return this.connection;
  }

  /** The effective codec (possibly auto-picked). */
  get effectiveCodec(): Codec {
    return this.codec;
  }

  /** **TEST-ONLY**. The `AsterCall` factory this client was built
   *  with. Exposed for tier-2 chaos tests that build
   *  `ClientSession.forTest` handles against adversarial session ids;
   *  production code uses {@link openSession}. */
  get asterCallFactory(): AsterCallFactory {
    return this.asterCall;
  }

  /** Open a new session on this connection. Allocates a monotonic
   *  sessionId scoped to the connection's `rpcAddrKey`. */
  async openSession<T extends object = object>(
    opts?: OpenSessionOptions<T>,
  ): Promise<ClientSession> {
    void opts; // reserved for future per-session options (interceptors, etc.)
    const sessionId = this.nextSessionId(this.rpcAddrKey);
    const transport = new IrohTransport2({
      connection: this.connection,
      asterCall: this.asterCall,
      codec: this.codec,
      sessionId,
    });
    return new ClientSession({
      parent: this,
      connection: this.connection,
      sessionId,
      transport,
      codec: this.codec,
      services: this.services,
    });
  }

  /** Open a SHARED-pool transport (`sessionId = 0`). Stateless calls
   *  go through the connection's SHARED stream pool. */
  sharedTransport(): IrohTransport2 {
    return new IrohTransport2({
      connection: this.connection,
      asterCall: this.asterCall,
      codec: this.codec,
      sessionId: 0,
    });
  }

  /** Best-effort close: closes the underlying native connection with a
   *  normal status. Mirrors the Python / Java convenience. */
  async close(): Promise<void> {
    this.connection.close(0, 'normal close');
  }

  private nextSessionId(rpcAddrKey: string): number {
    const current = (this.sessionIdCounters.get(rpcAddrKey) ?? 0) + 1;
    this.sessionIdCounters.set(rpcAddrKey, current);
    return current;
  }
}

// -- ClientSession ------------------------------------------------------------

interface ClientSessionInit {
  parent: AsterClient2;
  connection: NativeConnection;
  sessionId: number;
  transport: IrohTransport2;
  codec: Codec;
  services: readonly ServiceSummary[] | undefined;
}

/**
 * Client-side handle for a server-side SESSION-scoped service instance
 * (multiplexed-streams spec §6 / §7.5).
 *
 * A `ClientSession` pins one native connection and one client-allocated
 * `sessionId`; every call routed through it carries that `sessionId` on
 * its `StreamHeader`, so the server resolves the same session-scoped
 * instance on every invocation.
 *
 * Sessions are created implicitly on the server the first time it sees
 * a stream with the allocated `sessionId`; there is no explicit "open
 * session" RPC. Sessions are reaped server-side when the underlying
 * QUIC connection drops, so {@link close} here does no wire traffic —
 * it's a no-op kept for ergonomic try/finally patterns.
 */
export class ClientSession {
  private readonly parentClient: AsterClient2;
  private readonly connectionRef: NativeConnection;
  private readonly sessionIdValue: number;
  private readonly transportRef: IrohTransport2;
  private readonly codec: Codec;
  private readonly services: readonly ServiceSummary[] | undefined;
  private closed = false;
  private readonly stubs: Array<{ close(): Promise<void> }> = [];

  constructor(init: ClientSessionInit) {
    this.parentClient = init.parent;
    this.connectionRef = init.connection;
    this.sessionIdValue = init.sessionId;
    this.transportRef = init.transport;
    this.codec = init.codec;
    this.services = init.services;
  }

  /**
   * **TEST-ONLY** factory. Construct a {@link ClientSession} against
   * an explicit `sessionId`, bypassing {@link AsterClient2.openSession}'s
   * monotonic allocator. Mirrors Java's `ClientSession.forTest`.
   *
   * Production code MUST use {@link AsterClient2.openSession} so the
   * spec §6 "first stream arrival creates the session" invariant holds
   * under the client's allocation order. This factory exists so tier-2
   * chaos tests can drive the server's lookup-or-create / graveyard
   * logic with adversarial sessionId sequences (out-of-order, replayed,
   * past-the-cap) that the allocator would never produce.
   */
  static forTest(
    parent: AsterClient2,
    connection: NativeConnection,
    sessionId: number,
  ): ClientSession {
    const transport = new IrohTransport2({
      connection,
      asterCall: parent.asterCallFactory,
      codec: parent.effectiveCodec,
      sessionId,
    });
    return new ClientSession({
      parent,
      connection,
      sessionId,
      transport,
      codec: parent.effectiveCodec,
      services: undefined,
    });
  }

  /** The server-allocated id this session routes through. Useful for
   *  logs and tests. */
  get sessionId(): number {
    return this.sessionIdValue;
  }

  /** The underlying native connection. Useful for tests that want to
   *  inspect peer identity. */
  get connection(): NativeConnection {
    return this.connectionRef;
  }

  /** The parent client instance. */
  get parent(): AsterClient2 {
    return this.parentClient;
  }

  /** The underlying [`IrohTransport2`] bound to this session. */
  get transport(): IrohTransport2 {
    return this.transportRef;
  }

  /** Build a typed stub bound to this session's transport. Returned
   *  stub's `close()` does not close the session; close the session
   *  itself to drop its stubs. */
  getClient<T extends new (...args: any[]) => any>(
    serviceClass: T,
    options?: ClientOptions,
  ): AsterClient<InstanceType<T>> {
    if (this.closed) {
      throw new RpcError(
        StatusCode.FAILED_PRECONDITION,
        `ClientSession(${this.sessionIdValue}) is closed`,
      );
    }
    const stub = createClient(serviceClass, this.transportRef, options);
    this.stubs.push(stub as unknown as { close(): Promise<void> });
    return stub;
  }

  /** Service summaries the parent client was built with (if any).
   *  Exposed for diagnostics. */
  get advertisedServices(): readonly ServiceSummary[] | undefined {
    return this.services;
  }

  /** Close hook. No wire traffic — sessions are reaped server-side
   *  when the underlying QUIC connection drops (spec §7.5). Idempotent.
   *  Calls `close()` on every stub that was handed out via
   *  [`getClient`]. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    // Close stubs in reverse order so nested dependencies unwind cleanly.
    for (let i = this.stubs.length - 1; i >= 0; i--) {
      try {
        await this.stubs[i]!.close();
      } catch {
        // Best-effort close.
      }
    }
    this.stubs.length = 0;
  }

  /** Unused in v2 (codec is immutable once the session is open), but
   *  kept on the interface so the session object can be passed to
   *  codec-aware helpers alongside Java's `ClientSession.codec()`. */
  get effectiveCodec(): Codec {
    return this.codec;
  }
}
