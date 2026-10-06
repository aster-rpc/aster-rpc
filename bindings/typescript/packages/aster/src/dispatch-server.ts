/**
 * dispatch-server.ts -- the event loop under JS services on
 * `aster::rpc::Server` through the native `DispatchServer`
 * (dynamic-dispatch-design.md §4.3).
 *
 * Rust routes every call (gates, sessions, deadlines, frame limits) and hands
 * JS one event per call; JS answers by `callId`. This module drives the
 * event-delivery variants measured in T4.0
 * (`docs/_internal/bindings/t4-dispatch-benchmark.md`):
 *
 * | delivery | how events arrive |
 * |---|---|
 * | `tsfn-batched` (default) | Events enter a bounded ring; one threadsafe-function notification drains the ready batch |
 * | `tsfn` | Compatibility alias for the same bounded, coalesced TSFN delivery |
 * | `ring` / `ring-await` | SPSC ring; JS loops on `await nextBatch()` |
 * | `ring-poll` | SPSC ring; sync `pollBatch()` from `setImmediate`, parking on `nextBatch()` when empty |
 *
 * `dispatch.ts` builds the handler table, `CallContext` and interceptors on
 * top of it; `RpcServer` / `AsterServer` use that.
 */

import type {
  AsterConfig,
  DispatchEvent,
  DispatchServer as NativeDispatchServer,
} from '@aster-rpc/transport';
import { loadNative } from './native.js';
import type { DynamicServiceSpec } from './lowering.js';

export type { DispatchEvent };
export type { NativeDispatchServer };
export type { DynamicServiceSpec, DynamicMethodSpec } from './lowering.js';

/** How events reach JS (see the module doc). `ring` = `ring-await`. */
export type DispatchDelivery = 'tsfn' | 'tsfn-batched' | 'ring' | 'ring-await' | 'ring-poll';

/** The default delivery (T4.0 decision). */
export const DEFAULT_DISPATCH_DELIVERY: DispatchDelivery = 'tsfn-batched';

export const DISPATCH_DELIVERIES: readonly DispatchDelivery[] = [
  'tsfn',
  'tsfn-batched',
  'ring-await',
  'ring-poll',
];

/** `DispatchEvent.kind` values. */
export const DispatchEventKind = {
  Call: 1,
  Cancelled: 2,
  ResponseWritable: 3,
  ResponseClosed: 4,
  SessionClosed: 5,
  ConnectionClosed: 6,
  /** A connection on a non-RPC ALPN no Rust handler claimed (`alpn`); take it with `takeConnection(connectionId)`. */
  NonRpc: 7,
} as const;

/** `DispatchEvent.reason` for `Cancelled`. */
export const CancelReason = {
  Peer: 1,
  Deadline: 2,
  InvalidRequest: 3,
} as const;

/** `DispatchEvent.flags` bits. */
export const DispatchFlags = {
  FirstIsEnd: 1,
  NewSession: 2,
  IrohAuthenticated: 4,
  /** The event carries `metadata`. */
  HasMetadata: 0x100,
  /** The event carries `attributes`. */
  HasAttributes: 0x200,
} as const;

/** Results of the native `respond` / `send` / `finish` (`> 0` = queue full, bytes needed). */
export const SubmitResult = {
  Ok: 0,
  Closed: -1,
  NotFound: -2,
  TooLarge: -3,
  Invalid: -4,
} as const;

/** Native server options passed through to `DispatchServer.serve/start`. */
export interface DispatchServerTuning {
  /** Serve the baseline `aster.ops.NodeInfo` (default true). */
  builtinNodeInfo?: boolean;
  /** Serve the baseline `aster.net.Topology` (default false). */
  builtinTopology?: boolean;
  /** Cap on concurrently open sessions per connection (default 1024). */
  maxSessionsPerConnection?: number;
  /** Cap on any handler in ms (Rust default 300 000; 0 = none). */
  handlerTimeoutMs?: number;
  /** Dispatch ring capacity for TSFN and ring deliveries (default 4096, minimum 16). */
  ringCapacity?: number;
  /** Per-kind maximum for foreign calls, sessions, RPC connections, and non-RPC connections (default 4096). */
  maxForeignInflight?: number;
}

export interface DispatchLoopOptions extends DispatchServerTuning {
  /** Serve on this native node (started with `aster/1`). Else a node is started from `config`. */
  node?: unknown;
  /** With `node`: Gate 3 also reads attributes from this native `GatePolicy`'s store. */
  gateAttributes?: unknown;
  config?: AsterConfig;
  services: DynamicServiceSpec[];
  delivery?: DispatchDelivery;
  /** Max events per pull (ring variants). Default 256. */
  batchMax?: number;
  /** Called synchronously for every call event. Must not throw (a throw abandons the call). */
  onCall: (call: DispatchEvent, server: NativeDispatchServer) => void;
  /** Every other event (cancelled, session/connection closed, ...). */
  onEvent?: (event: DispatchEvent, server: NativeDispatchServer) => void;
}

/** A running prototype dispatch server: the native server plus its event loop. */
export class DispatchLoop {
  private closed = false;

  private constructor(
    readonly native: NativeDispatchServer,
    readonly delivery: DispatchDelivery,
  ) {}

  /** The server's `aster1…` ticket (own-node servers only; `''` when serving on a caller's node). */
  get address(): string {
    return this.native.address;
  }

  static async start(opts: DispatchLoopOptions): Promise<DispatchLoop> {
    const native = loadNative();
    const delivery: DispatchDelivery = opts.delivery === 'ring' ? 'ring-await' : (opts.delivery ?? DEFAULT_DISPATCH_DELIVERY);
    const batchMax = opts.batchMax ?? 256;
    const json = opts.services.map((s) => JSON.stringify(s));
    let loop: DispatchLoop | undefined;
    // `handle` closes over `loop` so the TSFN callback, created before the
    // native server exists, can reach it once `start` resolves. A peer that
    // already knows the node (`serve`, or a persistent identity) can call
    // before then: hold those events until `loop` exists.
    let early: DispatchEvent[] = [];
    const handle = (events: DispatchEvent[]): void => {
      if (!loop) {
        for (let i = 0; i < events.length; i++) early.push(events[i]);
        return;
      }
      const srv = loop.native;
      for (let i = 0; i < events.length; i++) {
        const ev = events[i];
        if (ev.kind === DispatchEventKind.Call) {
          try {
            opts.onCall(ev, srv);
          } catch {
            srv.abandon(ev.callId);
          }
        } else if (opts.onEvent) {
          try {
            opts.onEvent(ev, srv);
          } catch {
            /* ignore */
          }
        }
      }
    };
    const common = {
      builtinNodeInfo: opts.builtinNodeInfo ?? true,
      builtinTopology: opts.builtinTopology,
      maxSessionsPerConnection: opts.maxSessionsPerConnection,
      handlerTimeoutMs: opts.handlerTimeoutMs,
      ringCapacity: opts.ringCapacity,
      maxForeignInflight: opts.maxForeignInflight,
    };
    const tsfn = delivery === 'tsfn' || delivery === 'tsfn-batched';
    const nativeOpts = { ...common, delivery: tsfn ? delivery : 'ring' };
    const onEvents = tsfn ? handle : undefined;
    const srv: NativeDispatchServer = opts.node
      ? await native.DispatchServer.serve(opts.node as any, json, nativeOpts, onEvents, opts.gateAttributes as any)
      : await native.DispatchServer.start(opts.config, json, nativeOpts, onEvents);
    loop = new DispatchLoop(srv, delivery);
    const held = early;
    early = [];
    if (held.length) handle(held);
    if (delivery === 'ring-await') loop.runAwait(handle, batchMax);
    else if (delivery === 'ring-poll') loop.runPoll(handle, batchMax);
    return loop;
  }

  /** R1: one awaited pull per batch. */
  private runAwait(handle: (events: DispatchEvent[]) => void, max: number): void {
    const srv = this.native;
    void (async () => {
      while (!this.closed) {
        const batch = await srv.nextBatch(max);
        if (batch.length === 0) {
          if (this.closed) break;
          continue;
        }
        handle(batch);
      }
    })();
  }

  /** R2: sync polls from `setImmediate` while there is work; park on R1 when empty. */
  private runPoll(handle: (events: DispatchEvent[]) => void, max: number): void {
    const srv = this.native;
    const poll = (): void => {
      if (this.closed) return;
      const batch = srv.pollBatch(max);
      if (batch.length === 0) {
        park();
        return;
      }
      handle(batch);
      setImmediate(poll);
    };
    const park = (): void => {
      srv.nextBatch(max).then((batch) => {
        if (this.closed) return;
        if (batch.length) handle(batch);
        setImmediate(poll);
      });
    };
    park();
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await this.native.close();
  }
}

/**
 * Submit with backpressure: retry `op` after `waitWritable` while it reports
 * a full queue. Resolves the final result (`0` sent, `< 0` an error).
 */
export async function submitWithBackpressure(
  srv: NativeDispatchServer,
  callId: number,
  op: () => number,
): Promise<number> {
  let r = op();
  while (r > 0) {
    if (!(await srv.waitWritable(callId, r))) return SubmitResult.Closed;
    r = op();
  }
  return r;
}

/** `respond`, waiting for capacity when the response queue is full. */
export function respondCall(
  srv: NativeDispatchServer,
  callId: number,
  payload: Uint8Array,
  code?: number,
  message?: string,
): number | Promise<number> {
  const r = srv.respond(callId, payload, code, message);
  if (r <= 0) return r;
  return submitWithBackpressure(srv, callId, () => srv.respond(callId, payload, code, message));
}

/** One stream frame, waiting for capacity when the response queue is full. */
export function sendFrame(
  srv: NativeDispatchServer,
  callId: number,
  payload: Uint8Array,
): number | Promise<number> {
  const r = srv.send(callId, payload);
  if (r <= 0) return r;
  return submitWithBackpressure(srv, callId, () => srv.send(callId, payload));
}

/** The trailer, waiting for capacity when the response queue is full. */
export function finishCall(
  srv: NativeDispatchServer,
  callId: number,
  code?: number,
  message?: string,
): number | Promise<number> {
  const r = srv.finish(callId, code, message);
  if (r <= 0) return r;
  return submitWithBackpressure(srv, callId, () => srv.finish(callId, code, message));
}
