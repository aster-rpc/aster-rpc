/**
 * Upcalls — Rust asking JavaScript a question (plan D6 / R6).
 *
 * Several Rust extension points are implemented in JavaScript: a lease
 * backend ({@link ForeignLeaseSerializer}), a fenced resource
 * ({@link ForeignFencedResource}), the lease authorizer
 * (`MemorySerializer.setAuthorizer`), the topology `admitted` filter
 * ({@link AdmittedFilter}), the placement manager's providers
 * ({@link PlacementManager}), expose authorization (`Expose.exposeSocket`)
 * and Gate 0 (`node.serveAdmissionUpcalls`). Rust cannot call a JavaScript
 * function directly, so each of them is an *upcall*: Rust emits a typed
 * event into an {@link Upcalls} table, the table runs your (async) handler
 * and answers by id.
 *
 * ```ts
 * const upcalls = new Upcalls({ policies: { lease_authorize: { timeoutMs: 500, failureMode: 'fail_open' } } });
 * const serializer = new ForeignLeaseSerializer(myBackend, { upcalls });
 * // … use it …
 * upcalls.close();
 * ```
 *
 * - **Timeouts and failure modes.** Every kind has a policy (default 5 s,
 *   fail-closed). A *decision* kind (lease authorize, topology admitted,
 *   expose authorize, Gate 0) never fails: without a usable answer (handler
 *   error, timeout, overload, close) Rust applies the failure mode —
 *   `fail_open` allows, `fail_closed` denies. Every other kind surfaces the
 *   failure as an error to its Rust caller.
 * - **Cancellation.** When Rust stops waiting (timeout, its caller gave up,
 *   {@link Upcalls.close}) the handler's {@link UpcallContext.signal} is
 *   aborted with an {@link UpcallCancelledError}; a late answer is ignored.
 * - **Ordering.** None: answers are matched by id, so a slow handler holds
 *   nothing else up.
 * - **Lifetime.** A table holds its callback until {@link Upcalls.close}
 *   (it never keeps the process alive on its own). Adapters created without
 *   an explicit `upcalls` own a private table that their `close()` closes.
 *
 * One handler per kind per table: two JavaScript lease backends need two
 * tables (or let each adapter create its own).
 * @module
 */

import type {
  Upcalls as NativeUpcalls,
  UpcallEventJs as NativeUpcallEvent,
  UpcallKindInfo,
} from '@aster-rpc/transport';
import { loadNative } from './native.js';

/** Every upcall kind (the Rust `UpcallKind` codes). @group Upcalls */
export const UPCALL_KINDS = Object.freeze({
  cancel: 0,
  lease_transition: 1,
  lease_snapshot: 2,
  lease_authorize: 3,
  fenced_mutate: 4,
  topology_admitted: 16,
  topology_snapshot: 17,
  placement_policy: 32,
  placement_provision: 33,
  expose_authorize: 48,
  expose_http: 49,
  admission_connect: 64,
  admission_handshake: 65,
} as const);

/** An upcall kind other than `cancel`. @group Upcalls */
export type UpcallKindName = Exclude<keyof typeof UPCALL_KINDS, 'cancel'>;

/** Answer statuses of `respond`. @group Upcalls */
export const UPCALL_STATUS = Object.freeze({ ok: 0, error: 1, unimplemented: 2 } as const);

/** Why Rust stopped waiting for an upcall. @group Upcalls */
export type UpcallCancelReason = 'timeout' | 'abandoned' | 'shutdown';

const CANCEL_REASONS: Record<number, UpcallCancelReason> = { 1: 'timeout', 2: 'abandoned', 3: 'shutdown' };

/** The answer Rust uses for a decision kind without a usable answer. @group Upcalls */
export type UpcallFailureMode = 'fail_open' | 'fail_closed';

/** Timeout and failure mode for one kind (failure mode: decision kinds only). @group Upcalls */
export interface UpcallPolicy {
  timeoutMs?: number;
  failureMode?: UpcallFailureMode;
}

/** Options for {@link Upcalls}. @group Upcalls */
export interface UpcallsOptions {
  /**
   * Most upcalls queued for JavaScript and not yet taken (default 1024).
   * Beyond it upcalls are refused at once (decisions fall back to their
   * failure mode, other kinds fail).
   */
  capacity?: number;
  /** Policy for kinds without their own (default 5 000 ms, fail-closed). */
  defaultPolicy?: UpcallPolicy;
  /** Per-kind policies. */
  policies?: Partial<Record<UpcallKindName, UpcallPolicy>>;
}

/** What a handler gets besides the request. @group Upcalls */
export interface UpcallContext {
  readonly invocationId: number;
  readonly kind: UpcallKindName;
  /**
   * Aborted (reason: an {@link UpcallCancelledError}) when Rust stops
   * waiting — its timeout passed, its caller gave up, or the table closed.
   */
  readonly signal: AbortSignal;
  /** When Rust stops waiting. */
  readonly deadline: Date;
}

/** The abort reason of {@link UpcallContext.signal}. @group Upcalls */
export class UpcallCancelledError extends Error {
  override readonly name = 'UpcallCancelledError';
  constructor(readonly reason: UpcallCancelReason) {
    super(`upcall cancelled: ${reason}`);
  }
}

/** Delivery counters of an {@link Upcalls} table. @group Upcalls */
export interface UpcallStats {
  delivered: number;
  refused: number;
  tsfnCalls: number;
  tsfnCallbacks: number;
  tsfnFailures: number;
  watchdogRescues: number;
  queued: number;
  pending: number;
  /** Handlers currently running. */
  running: number;
}

/**
 * A raw handler: gets the decoded native request object, returns the
 * native response object (encoded by the kind's UCv1 response codec).
 * @internal
 */
export type RawUpcallHandler = (request: never, ctx: UpcallContext) => unknown;

let kindTable: Map<number, UpcallKindInfo> | null = null;
function kinds(): Map<number, UpcallKindInfo> {
  if (!kindTable) kindTable = new Map(loadNative().upcallKinds().map((k) => [k.code, k]));
  return kindTable;
}

const NAMES: Record<number, UpcallKindName> = Object.fromEntries(
  Object.entries(UPCALL_KINDS).map(([name, code]) => [code, name]),
) as Record<number, UpcallKindName>;

function errorMessage(e: unknown): string {
  if (e instanceof Error) return e.message || e.name;
  return String(e);
}

/**
 * One upcall table: the events Rust's adapters issue, the handlers that
 * answer them, and the per-kind policies.
 * @group Upcalls
 */
export class Upcalls implements Disposable {
  /** @internal */
  readonly native: NativeUpcalls;
  private readonly handlers = new Map<number, RawUpcallHandler>();
  private readonly running = new Map<number, AbortController>();

  constructor(options: UpcallsOptions = {}) {
    const native = loadNative();
    this.native = new native.Upcalls(
      (events: NativeUpcallEvent[]) => this.onEvents(events),
      options.capacity != null ? { capacity: options.capacity } : undefined,
    );
    if (options.defaultPolicy) this.setDefaultPolicy(options.defaultPolicy);
    for (const [kind, policy] of Object.entries(options.policies ?? {})) {
      if (policy) this.setPolicy(kind as UpcallKindName, policy);
    }
  }

  /** Set `kind`'s policy (unset fields keep their current value). */
  setPolicy(kind: UpcallKindName, policy: UpcallPolicy): void {
    const code = UPCALL_KINDS[kind];
    if (code == null) throw new TypeError(`unknown upcall kind ${String(kind)}`);
    const current = this.native.policy(code);
    this.native.setPolicy(code, {
      timeoutMs: policy.timeoutMs ?? current.timeoutMs,
      failureMode: policy.failureMode ?? current.failureMode,
    });
  }

  /** Set the policy for kinds without their own. */
  setDefaultPolicy(policy: UpcallPolicy): void {
    // The default is what a kind without its own policy reports; code 0
    // (cancel) never gets one.
    const current = this.native.policy(0);
    this.native.setDefaultPolicy({
      timeoutMs: policy.timeoutMs ?? current.timeoutMs,
      failureMode: policy.failureMode ?? current.failureMode,
    });
  }

  /** The policy `kind` runs under. */
  policy(kind: UpcallKindName): Required<UpcallPolicy> {
    return this.native.policy(UPCALL_KINDS[kind]);
  }

  /** Upcalls waiting for an answer. */
  get pendingCount(): number {
    return this.native.pendingCount;
  }

  get closed(): boolean {
    return this.native.closed;
  }

  stats(): UpcallStats {
    return { ...this.native.stats(), running: this.running.size };
  }

  /**
   * Fail every pending upcall (decisions fall back to their failure mode),
   * refuse later ones, abort running handlers and release the callback.
   * Idempotent.
   */
  close(): void {
    this.native.close();
    for (const [id, ctrl] of this.running) {
      this.running.delete(id);
      ctrl.abort(new UpcallCancelledError('shutdown'));
    }
  }

  [Symbol.dispose](): void {
    this.close();
  }

  /**
   * Install the handler for `kind`; returns its uninstaller. Throws when the
   * kind already has one (use another table).
   * @internal
   */
  handle(kind: UpcallKindName, handler: RawUpcallHandler): () => void {
    const code = UPCALL_KINDS[kind];
    if (this.handlers.has(code)) {
      throw new Error(`upcall kind ${kind} already has a handler on this table (use another Upcalls)`);
    }
    this.handlers.set(code, handler);
    return () => {
      if (this.handlers.get(code) === handler) this.handlers.delete(code);
    };
  }

  private respond(id: number, status: number, payload?: Buffer): void {
    // `false` = nobody waits any more (cancelled / timed out): ignore.
    this.native.respond(id, status, payload);
  }

  private onEvents(events: NativeUpcallEvent[]): void {
    for (const ev of events) {
      if (ev.kind === UPCALL_KINDS.cancel) {
        const ctrl = this.running.get(ev.invocationId);
        if (ctrl) {
          this.running.delete(ev.invocationId);
          ctrl.abort(new UpcallCancelledError(CANCEL_REASONS[ev.cancelReason ?? 0] ?? 'abandoned'));
        }
        continue;
      }
      this.run(ev);
    }
  }

  private run(ev: NativeUpcallEvent): void {
    const id = ev.invocationId;
    if (ev.error != null) {
      this.respond(id, UPCALL_STATUS.error, Buffer.from(ev.error));
      return;
    }
    const handler = this.handlers.get(ev.kind);
    const responseType = kinds().get(ev.kind)?.responseType;
    if (!handler || !responseType) {
      this.respond(id, UPCALL_STATUS.unimplemented);
      return;
    }
    const ctrl = new AbortController();
    this.running.set(id, ctrl);
    const ctx: UpcallContext = {
      invocationId: id,
      kind: NAMES[ev.kind],
      signal: ctrl.signal,
      deadline: new Date(ev.deadlineUnixMs),
    };
    const done = (status: number, payload?: Buffer) => {
      if (this.running.get(id) === ctrl) this.running.delete(id);
      if (ctrl.signal.aborted) return;
      this.respond(id, status, payload);
    };
    let result: unknown;
    try {
      result = handler(ev.request as never, ctx);
    } catch (e) {
      done(UPCALL_STATUS.error, Buffer.from(errorMessage(e)));
      return;
    }
    Promise.resolve(result).then(
      (value) => {
        let bytes: Buffer;
        try {
          bytes = loadNative().upcallCodecEncode(responseType, value as never);
        } catch (e) {
          done(UPCALL_STATUS.error, Buffer.from(`invalid ${NAMES[ev.kind]} answer: ${errorMessage(e)}`));
          return;
        }
        done(UPCALL_STATUS.ok, bytes);
      },
      (e: unknown) => done(UPCALL_STATUS.error, Buffer.from(errorMessage(e))),
    );
  }
}

/**
 * Resolve the table an adapter runs on: the caller's, or a private one the
 * adapter owns (and closes).
 * @internal
 */
export function adapterTable(upcalls: Upcalls | undefined): { upcalls: Upcalls; owned: boolean } {
  return upcalls ? { upcalls, owned: false } : { upcalls: new Upcalls(), owned: true };
}

/**
 * Decode one UCv1 upcall message (a `conformance/vectors/upcalls.json` type
 * name) into its native object form. Throws on non-canonical input.
 * @group Upcalls
 */
export function upcallCodecDecode(typeName: string, bytes: Uint8Array): unknown {
  return loadNative().upcallCodecDecode(typeName, Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength));
}

/**
 * Encode a native-form object as the UCv1 message `typeName` (inverse of
 * {@link upcallCodecDecode}).
 * @group Upcalls
 */
export function upcallCodecEncode(typeName: string, value: unknown): Uint8Array {
  return new Uint8Array(loadNative().upcallCodecEncode(typeName, value as never));
}
