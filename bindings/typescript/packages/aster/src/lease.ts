/**
 * Single-owner fenced leases (Rust `aster::lease`).
 *
 * The *grant* is a cheap CAS at a serializer plus a TTL; correctness comes
 * from the *fence*: every protected action carries `(resource, epoch)` and
 * the resource rejects anything that is not the current epoch from the
 * current holder.
 *
 * ```ts
 * const serializer = new MemorySerializer();
 * const outcome = await serializer.acquire('tree/42', node.id, { ttlMs: 30_000 });
 * if (outcome.granted) {
 *   await using lease = outcome.lease;       // released on scope exit
 *   const fence = lease.fence();             // take at send time; null = stop
 *   if (fence) serializer.mutateFenced(fence, node.id, () => write());
 * } else {
 *   // back off; outcome.snapshot.remainingMs is a hint
 * }
 * ```
 *
 * Renewal runs in Rust; {@link Lease.fence} returns `null` once the holder
 * must stop (released, fenced, lost, or past its self-limit).
 *
 * Backends: {@link MemorySerializer} (in-process reference) and
 * {@link GrantorSerializer} (a remote designated grantor, `node.leaseGrantor(id)`;
 * serve one with `node.serveLeaseGrantor(serializer)`). JavaScript-implemented
 * serializers and fenced resources wait on the binding upcall mechanism;
 * meanwhile {@link leaseTransition} and {@link checkFence} give a
 * JavaScript-owned resource the same rules to apply under its own lock.
 */

import type {
  FenceRejection as NativeFenceRejection,
  GrantorSerializer as NativeGrantorSerializer,
  LeaseDenied as NativeLeaseDenied,
  LeaseHandle as NativeLeaseHandle,
  LeaseSnapshot as NativeLeaseSnapshot,
  MemorySerializer as NativeMemorySerializer,
  TransitionOutcome as NativeTransitionOutcome,
} from '@aster-rpc/transport';
import { loadNative } from './native.js';
import type { NodeId } from './node.js';

/**
 * The fencing token for a protected action. `holder` is the node id the
 * epoch was issued to — bind it to the *authenticated* writer.
 * @group Lease
 */
export interface Fence {
  resource: string;
  epoch: number;
  holder: string;
}

/** Advisory view of a serializer row (stale the moment it returns). @group Lease */
export interface LeaseSnapshot {
  resource: string;
  epoch: number;
  holder?: string;
  held: boolean;
  /** Serializer-local hint of ms until expiry; absent when free. */
  remainingMs?: number;
}

/** Holder-side timing (defaults: ttl 30 000, renew 10 000, margin 1 000 ms). @group Lease */
export interface LeaseOptions {
  ttlMs?: number;
  /** Must fit inside `ttlMs - holderMarginMs`. */
  renewIntervalMs?: number;
  /** Subtracted from `ttlMs` for the holder's self-limit. */
  holderMarginMs?: number;
}

/** Why a transition was refused. @group Lease */
export type DenyReason =
  | 'held_by_other'
  | 'epoch_mismatch'
  | 'not_holder'
  | 'expired'
  | 'not_held'
  | 'unauthorized';

/** Holder-side lease status. @group Lease */
export type LeaseStatus = 'held' | 'released' | 'fenced' | 'lost';

/** The result of an acquire attempt. @group Lease */
export type AcquireOutcome =
  | { granted: true; lease: Lease }
  | { granted: false; reason: DenyReason; snapshot: LeaseSnapshot };

/** The result of one transition at a serializer. @group Lease */
export interface TransitionOutcome {
  kind: 'granted' | 'renewed' | 'released' | 'revoked' | 'denied';
  /** `granted`: the issued fence. */
  fence?: Fence;
  /** `denied`: why. */
  denyReason?: DenyReason;
  /** `denied`: the row after the decision. */
  snapshot?: LeaseSnapshot;
}

/** The durable fence fields of a serializer row. @group Lease */
export interface LeaseRowState {
  epoch: number;
  holder?: string;
  held: boolean;
  ttlMs: number;
}

/** A requested transition; `actor` is candidate / holder / authority. @group Lease */
export type LeaseOp =
  | { kind: 'acquire'; actor: string; ttlMs: number }
  | { kind: 'renew' | 'release'; actor: string; epoch: number }
  | { kind: 'revoke'; actor: string };

/** What the pure state machine decided. @group Lease */
export type LeaseTransition =
  | { kind: 'apply'; next: LeaseRowState; outcome: TransitionOutcome; resetGrant: boolean }
  | { kind: 'deny'; denyReason: DenyReason };

/** Why the resource rejected a fenced action. @group Lease */
export type FenceRejectionKind = 'stale_epoch' | 'not_holder' | 'not_held' | 'future_epoch';

/**
 * A protected action was rejected by the fence. Never routine — surface it
 * (metric + audit); never retry silently. `future_epoch` is a
 * serializer-integrity alarm.
 * @group Lease
 */
export class FenceRejectedError extends Error {
  override readonly name = 'FenceRejectedError';
  readonly kind: FenceRejectionKind;
  readonly carried?: number;
  readonly current?: number;

  constructor(r: NativeFenceRejection) {
    super(r.message);
    this.kind = r.kind;
    if (r.carried != null) this.carried = r.carried;
    if (r.current != null) this.current = r.current;
  }
}

function snapshotOut(s: NativeLeaseSnapshot): LeaseSnapshot {
  return {
    resource: s.resource,
    epoch: s.epoch,
    ...(s.holder != null ? { holder: s.holder } : {}),
    held: s.held,
    ...(s.remainingMs != null ? { remainingMs: s.remainingMs } : {}),
  };
}

function transitionOut(o: NativeTransitionOutcome): TransitionOutcome {
  return {
    kind: o.kind,
    ...(o.fence ? { fence: { ...o.fence } } : {}),
    ...(o.denyReason ? { denyReason: o.denyReason } : {}),
    ...(o.snapshot ? { snapshot: snapshotOut(o.snapshot) } : {}),
  };
}

function acquireOut(r: NativeLeaseHandle | NativeLeaseDenied): AcquireOutcome {
  if (r instanceof loadNative().LeaseHandle) {
    return { granted: true, lease: new Lease(r) };
  }
  const denied = r as NativeLeaseDenied;
  return { granted: false, reason: denied.reason, snapshot: snapshotOut(denied.snapshot) };
}

/**
 * A held lease: renewed in the background (in Rust), fence gated by the
 * self-limit. `await using` / {@link Lease.release} hands off cleanly;
 * dropping it without releasing lets the lease lapse by TTL.
 * @group Lease
 */
export class Lease implements AsyncDisposable {
  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeLeaseHandle,
  ) {}

  get resource(): string {
    return this.native.resource;
  }

  /** The epoch this lease was granted at. */
  get epoch(): number {
    return this.native.epoch;
  }

  /** The holder the epoch was issued to. */
  get holder(): string {
    return this.native.holder;
  }

  get status(): LeaseStatus {
    return this.native.status;
  }

  /**
   * The fence for a protected action, or `null` once the holder must stop.
   * Take it at send time; never cache it across awaits.
   */
  fence(): Fence | null {
    return this.native.fence();
  }

  /**
   * Status changes from now on: each new status, ending after the first
   * terminal one (`released` / `fenced` / `lost`).
   */
  async *statusChanges(): AsyncGenerator<LeaseStatus, void, undefined> {
    for await (const s of this.native.statusChanges()) yield s as LeaseStatus;
  }

  /**
   * Graceful release: stop renewing, then RELEASE at the serializer (which
   * advances the fence, so this holder's in-flight writes are rejected from
   * here on). Idempotent.
   */
  release(): Promise<void> {
    return this.native.release();
  }

  /**
   * Stop renewing and return the fence for exactly one final operation,
   * **without** releasing at the serializer; `null` when the fence is no
   * longer usable.
   */
  stopRenewing(): Fence | null {
    return this.native.stopRenewing();
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.release();
  }
}

/** A lease backend. @group Lease */
export interface LeaseSerializer {
  /** Advisory snapshot of `resource`. */
  snapshot(resource: string): Promise<LeaseSnapshot>;
}

/**
 * The reference in-memory lease serializer (also what a designated grantor
 * serves). Expiry runs on this process's clock.
 * @group Lease
 */
export class MemorySerializer implements LeaseSerializer {
  /** @internal */
  readonly native: NativeMemorySerializer;

  constructor() {
    this.native = new (loadNative().MemorySerializer)();
  }

  /** One ACQUIRE CAS for `holder` (usually a node id). */
  async acquire(resource: string, holder: NodeId | string, options?: LeaseOptions): Promise<AcquireOutcome> {
    return acquireOut(await this.native.acquire(resource, holder, options));
  }

  async snapshot(resource: string): Promise<LeaseSnapshot> {
    return snapshotOut(await this.native.snapshot(resource));
  }

  /** Apply one raw transition (tooling / tests; prefer {@link acquire}). */
  async transition(resource: string, op: LeaseOp): Promise<TransitionOutcome> {
    return transitionOut(await this.native.transition(resource, op));
  }

  /**
   * REVOKE: advances the fence unconditionally. Withdrawing *authority*
   * (e.g. a directory tombstone) is the caller's separate second step.
   */
  async revoke(resource: string, authority: string): Promise<TransitionOutcome> {
    return transitionOut(await this.native.revoke(resource, authority));
  }

  /** Simulate a restart: held rows get a fresh TTL; the fence never advances. */
  restart(): void {
    this.native.restart();
  }

  /**
   * Run `fn` under the serializer's lock iff the fence rule passes for
   * `writer` (the *authenticated* peer id — never a request field). Throws
   * {@link FenceRejectedError} without running `fn` otherwise. `fn` must be
   * synchronous and must not call back into this serializer.
   */
  mutateFenced<T>(fence: Fence, writer: NodeId | string, fn: () => T): T {
    let result: T | undefined;
    const rejection = this.native.mutateFenced(fence, writer, () => {
      result = fn();
    });
    if (rejection) throw new FenceRejectedError(rejection);
    return result as T;
  }
}

/**
 * A remote designated grantor (`aster.lease.Grantor`) as a lease backend —
 * from `node.leaseGrantor(grantorId)`. Every op is bound to this node's
 * authenticated identity.
 * @group Lease
 */
export class GrantorSerializer implements LeaseSerializer {
  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeGrantorSerializer,
  ) {}

  /** This node's id — the holder every grant is issued to. */
  get selfId(): NodeId {
    return this.native.selfId as NodeId;
  }

  /** ACQUIRE `resource` at the grantor as this node. */
  async acquire(resource: string, options?: LeaseOptions): Promise<AcquireOutcome> {
    return acquireOut(await this.native.acquire(resource, options));
  }

  async snapshot(resource: string): Promise<LeaseSnapshot> {
    return snapshotOut(await this.native.snapshot(resource));
  }

  /** REVOKE as this node (the grantor gates it; by default `operator` role only). */
  async revoke(resource: string): Promise<TransitionOutcome> {
    return transitionOut(await this.native.revoke(resource));
  }
}

/**
 * The normative lease state machine as a pure function — for a resource
 * that is its own serialization point: read the row, call this, apply
 * `next` in the same transaction.
 * @group Lease
 */
export function leaseTransition(row: LeaseRowState, expired: boolean, op: LeaseOp, resource: string): LeaseTransition {
  const t = loadNative().leaseTransition(row, expired, op, resource);
  if (t.kind === 'deny') return { kind: 'deny', denyReason: t.denyReason! };
  return { kind: 'apply', next: t.next!, outcome: transitionOut(t.outcome!), resetGrant: t.resetGrant! };
}

/**
 * The fence rule (`epoch == row.epoch ∧ writer == row.holder ∧ row.held`),
 * evaluated against `row`. Returns the rejection, or `null` when `writer`
 * may act. Evaluate it inside the resource's own serialization point.
 * @group Lease
 */
export function checkFence(row: LeaseRowState, fence: Fence, writer: NodeId | string): FenceRejectedError | null {
  const r = loadNative().leaseCheckFence(row, fence, writer);
  return r ? new FenceRejectedError(r) : null;
}
