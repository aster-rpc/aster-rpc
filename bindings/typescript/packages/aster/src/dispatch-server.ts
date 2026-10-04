/**
 * dispatch-server.ts -- T4.0 prototype: JS services on `aster::rpc::Server`
 * through the native `DispatchServer` (dynamic-dispatch-design.md §4.3).
 *
 * Rust routes every call (gates, sessions, deadlines, frame limits) and hands
 * JS one event per call; JS answers by `callId`. This module drives the four
 * event-delivery variants that T4.0 benchmarks
 * (`docs/_internal/bindings/t4-dispatch-benchmark.md`):
 *
 * | delivery | variant | how events arrive |
 * |---|---|---|
 * | `tsfn` | T-a | Rust calls a threadsafe function once per event |
 * | `tsfn-batched` | T-b | Rust coalesces events while JS is busy, one TSFN call per batch |
 * | `ring-await` | R1 | SPSC ring; JS loops on `await nextBatch()` |
 * | `ring-poll` | R2 | SPSC ring; sync `pollBatch()` from `setImmediate`, parking on `nextBatch()` when empty |
 *
 * Not wired into `AsterServer` yet: that is T4 proper (after R3d), which
 * adds the decorator lowering, interceptors and `dispatch.ts`.
 */

import type {
  AsterConfig,
  DispatchEvent,
  DispatchServer as NativeDispatchServer,
} from '@aster-rpc/transport';
import { loadNative } from './native.js';

export type { DispatchEvent };
export type { NativeDispatchServer };

/** How events reach JS (see the module doc). */
export type DispatchDelivery = 'tsfn' | 'tsfn-batched' | 'ring-await' | 'ring-poll';

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
} as const;

/** `DispatchEvent.flags` bits. */
export const DispatchFlags = {
  FirstIsEnd: 1,
  NewSession: 2,
  IrohAuthenticated: 4,
} as const;

/** Results of the native `respond` / `send` / `finish` (`> 0` = queue full, bytes needed). */
export const SubmitResult = {
  Ok: 0,
  Closed: -1,
  NotFound: -2,
  TooLarge: -3,
  Invalid: -4,
} as const;

/** One method in a `DynamicService` registration (design §1.5). */
export interface DynamicMethodSpec {
  name: string;
  pattern: 'unary' | 'server_stream' | 'client_stream' | 'bidi_stream';
  request: { hash: string; display?: string };
  response: { hash: string; display?: string };
  idempotent?: boolean;
  default_timeout?: number | null;
  requires?: unknown;
  limits?: { max_frame_size?: number };
}

/** The JSON registration form of a `DynamicService` (design §1.5). */
export interface DynamicServiceSpec {
  name: string;
  version: number;
  scope?: 'shared' | 'session';
  requires?: unknown;
  serialization_modes?: string[];
  producer_language?: string;
  iroh_only?: boolean;
  methods: DynamicMethodSpec[];
  type_defs?: unknown[];
  manifest_json?: string | null;
}

export interface DispatchLoopOptions {
  config?: AsterConfig;
  services: DynamicServiceSpec[];
  delivery?: DispatchDelivery;
  /** Max events per pull (ring variants). Default 256. */
  batchMax?: number;
  /** Serve the baseline `aster.ops.NodeInfo` (default true). */
  builtinNodeInfo?: boolean;
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

  /** The server's `aster1…` ticket. */
  get address(): string {
    return this.native.address;
  }

  static async start(opts: DispatchLoopOptions): Promise<DispatchLoop> {
    const native = loadNative();
    const delivery = opts.delivery ?? 'tsfn-batched';
    const batchMax = opts.batchMax ?? 256;
    const json = opts.services.map((s) => JSON.stringify(s));
    let loop: DispatchLoop | undefined;
    // `handle` closes over `loop` so the TSFN callback, created before the
    // native server exists, can reach it once `start` resolves. Events
    // cannot arrive before that: the address is not published yet.
    const handle = (events: DispatchEvent[]): void => {
      const srv = loop!.native;
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
    const common = { builtinNodeInfo: opts.builtinNodeInfo ?? true };
    let srv: NativeDispatchServer;
    if (delivery === 'tsfn' || delivery === 'tsfn-batched') {
      srv = await native.DispatchServer.start(opts.config, json, { ...common, delivery }, handle);
    } else {
      srv = await native.DispatchServer.start(opts.config, json, { ...common, delivery: 'ring' });
    }
    loop = new DispatchLoop(srv, delivery);
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
