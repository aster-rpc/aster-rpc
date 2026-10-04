/**
 * Aster coordination client (Rust `aster::coordination::client`, wrapped —
 * never ported; plan D7 / T8).
 *
 * A durable, journaled client for a coordination group: fenced leases over
 * small registers, exact ACLs, and linearizable reads. Journaling, leader
 * redirects, retries and reply validation all run in Rust.
 *
 * ```ts
 * const client = await CoordinationClient.connect(node, groupId, {
 *   seedTickets,                           // routing hints only
 *   journalDir: './state/coordination',    // persist with the node identity
 * });
 *
 * // On startup, before any mutation: redeliver what a previous run resolved.
 * const pending = (await client.unacknowledged()) ?? (await client.resume());
 * if (pending) { await handle(pending.reply); await pending.acknowledge(); }
 *
 * const resolved = await client.acquire({ namespace: 'app.head', key: 'w-1' }, 30_000);
 * // fsynced before it returned; acknowledge once *you* durably handled it
 * await resolved.acknowledge();
 * ```
 *
 * The delivery contract: every mutation returns a {@link ResolvedMutation}
 * that was fsynced before the promise resolved. Until it is acknowledged no
 * new mutation is permitted — across calls and across restarts — and
 * {@link CoordinationClient.unacknowledged} / {@link CoordinationClient.resume}
 * redeliver it. A typed rejection (`outcome === 'rejected'`) is still a
 * resolved result and must be acknowledged too.
 *
 * Placement: the pure planner is {@link placementPlan}; the §7.7.3 manager
 * is {@link PlacementManager}, whose policy source, provisioner and
 * (optionally) topology source are JavaScript objects driven by Rust over
 * upcalls.
 */

import type {
  CoordinationClient as NativeClient,
  CoordinationFence as NativeFence,
  CoordinationLease as NativeLease,
  CoordinationMutationReply as NativeReply,
  CoordinationReadClient as NativeReadClient,
  CoordinationResolvedMutation as NativeResolved,
  CoordinationResourceKey as NativeResourceKey,
  CoordinationResourceSnapshot as NativeSnapshot,
  LeaseDenied as NativeLeaseDenied,
} from '@aster-rpc/transport';
import { loadNative } from './native.js';
import type { Node, NodeId } from './node.js';
import type { DenyReason, LeaseOptions, LeaseSnapshot, LeaseStatus } from './lease.js';
import { nativeTopologySnapshot, Topology, type ClusterView, type TopologySnapshot } from './topology.js';
import { adapterTable, type UpcallContext, type Upcalls } from './upcalls.js';

// ── Codes ────────────────────────────────────────────────────────────────────

/** §5.2 outcome codes. @group Coordination */
export const COORDINATION_OUTCOMES = Object.freeze({
  1: 'granted',
  2: 'renewed',
  3: 'mutated',
  4: 'released',
  5: 'revoked',
  6: 'grant_updated',
  7: 'admin_updated',
  8: 'membership_accepted',
  9: 'client_retired',
  10: 'membership_cancelled',
  100: 'rejected',
} as const);

/** A §5.2 outcome. @group Coordination */
export type CoordinationOutcome = (typeof COORDINATION_OUTCOMES)[keyof typeof COORDINATION_OUTCOMES];

/** §5.2 reason codes (`0` is none). @group Coordination */
export const COORDINATION_REASONS = Object.freeze({
  0: 'none',
  1: 'held_by_other',
  2: 'not_held',
  3: 'not_holder',
  4: 'epoch_mismatch',
  5: 'value_revision_mismatch',
  6: 'acl_revision_mismatch',
  7: 'admin_revision_mismatch',
  8: 'sequence_too_old',
  9: 'sequence_gap',
  10: 'sequence_conflict',
  11: 'client_retired',
  12: 'permission_changed',
  13: 'membership_revision_mismatch',
  14: 'membership_intent_active',
  15: 'counter_exhausted',
  16: 'membership_cannot_cancel',
  17: 'operation_id_conflict',
  18: 'retained_session_cap_reached',
  19: 'state_budget_exhausted',
  20: 'intent_slots_exhausted',
  21: 'active_client_cap',
  22: 'resource_slots_exhausted',
  23: 'acl_slots_exhausted',
  24: 'last_administrator',
  25: 'membership_learner_envelope',
  26: 'root_authority_immutable',
} as const);

/** A §5.2 reason. @group Coordination */
export type CoordinationReason = (typeof COORDINATION_REASONS)[keyof typeof COORDINATION_REASONS];

/** §8.1 rights bits — exact, no wildcards or inheritance. @group Coordination */
export const CoordinationRights = Object.freeze({
  READ: 0x01,
  ACQUIRE: 0x02,
  RENEW: 0x04,
  MUTATE: 0x08,
  RELEASE: 0x10,
  REVOKE: 0x20,
  /** READ | ACQUIRE | RENEW | MUTATE | RELEASE — a register holder's set. */
  HOLDER: 0x1f,
} as const);

function outcomeOf(code: number): CoordinationOutcome | 'unknown' {
  return (COORDINATION_OUTCOMES as Record<number, CoordinationOutcome>)[code] ?? 'unknown';
}

function reasonOf(code: number): CoordinationReason | 'unknown' {
  return (COORDINATION_REASONS as Record<number, CoordinationReason>)[code] ?? 'unknown';
}

// ── Errors ───────────────────────────────────────────────────────────────────

/** What a {@link CoordinationError} reports. @group Coordination */
export type CoordinationErrorKind =
  /** A resolved result awaits acknowledgement; take `unacknowledged()` first. */
  | 'awaiting_ack'
  /** Another request is unresolved on this journal; `resume()` first. */
  | 'unresolved'
  /** The journal is fatal (§10 reason 18); no further mutation is possible. */
  | 'journal_fatal'
  /** Another process (or an unclosed client) holds this journal's lock. */
  | 'journal_locked'
  /** A bad argument, or a request the client refused to journal. */
  | 'invalid'
  /** The group answered with a non-OK RPC status (`code`). */
  | 'rpc'
  /** No member answered, or the outcome is ambiguous — safe to retry. */
  | 'unavailable'
  /** The client (or this handle) is closed / already consumed. */
  | 'closed'
  /** A resolved typed rejection (outcome 100), surfaced as an error. */
  | 'rejected'
  | 'other';

/** Base class of every coordination failure. @group Coordination */
export class CoordinationError extends Error {
  override readonly name: string = 'CoordinationError';
  constructor(
    readonly kind: CoordinationErrorKind,
    message: string,
  ) {
    super(message);
  }
}

/** A result awaits acknowledgement before any new mutation. @group Coordination */
export class CoordinationAwaitingAckError extends CoordinationError {
  override readonly name = 'CoordinationAwaitingAckError';
}

/** Another request is unresolved; call `resume()`. @group Coordination */
export class CoordinationUnresolvedError extends CoordinationError {
  override readonly name = 'CoordinationUnresolvedError';
}

/** The journal is fatal; no further mutation is possible. @group Coordination */
export class CoordinationJournalFatalError extends CoordinationError {
  override readonly name = 'CoordinationJournalFatalError';
}

/** The journal's lifetime lock is held elsewhere. @group Coordination */
export class CoordinationJournalLockedError extends CoordinationError {
  override readonly name = 'CoordinationJournalLockedError';
}

/** The group answered with a non-OK RPC status. @group Coordination */
export class CoordinationRpcError extends CoordinationError {
  override readonly name = 'CoordinationRpcError';
  constructor(
    readonly code: number,
    message: string,
  ) {
    super('rpc', message);
  }
}

/** No member answered (or the outcome was ambiguous). @group Coordination */
export class CoordinationUnavailableError extends CoordinationError {
  override readonly name = 'CoordinationUnavailableError';
}

/** The client or handle is closed / already consumed. @group Coordination */
export class CoordinationClosedError extends CoordinationError {
  override readonly name = 'CoordinationClosedError';
}

/**
 * A committed typed rejection (§5.2 outcome 100). Carries the reason and the
 * full reply (the post-decision snapshot is a back-off / retry hint).
 * @group Coordination
 */
export class CoordinationRejectedError extends CoordinationError {
  override readonly name = 'CoordinationRejectedError';
  readonly reason: CoordinationReason | 'unknown';
  readonly reasonCode: number;
  constructor(readonly reply: MutationReply) {
    super('rejected', `coordination: rejected (${reply.reason}, reason ${reply.reasonCode})`);
    this.reason = reply.reason;
    this.reasonCode = reply.reasonCode;
  }
}

const TAG = /^\[coordination:([a-z_]+)(?::(-?\d+))?\] ?/;

/** @internal Turn a tagged native error into its typed class. */
export function coordinationError(err: unknown): unknown {
  if (err instanceof CoordinationError || !(err instanceof Error)) return err;
  const m = TAG.exec(err.message);
  if (!m) return err;
  const message = err.message.slice(m[0].length);
  let typed: CoordinationError;
  switch (m[1]) {
    case 'awaiting_ack':
      typed = new CoordinationAwaitingAckError('awaiting_ack', message);
      break;
    case 'unresolved':
      typed = new CoordinationUnresolvedError('unresolved', message);
      break;
    case 'journal_fatal':
      typed = new CoordinationJournalFatalError('journal_fatal', message);
      break;
    case 'journal_locked':
      typed = new CoordinationJournalLockedError('journal_locked', message);
      break;
    case 'rpc':
      typed = new CoordinationRpcError(Number(m[2] ?? 0), message);
      break;
    case 'unavailable':
      typed = new CoordinationUnavailableError('unavailable', message);
      break;
    case 'closed':
      typed = new CoordinationClosedError('closed', message);
      break;
    case 'invalid':
      typed = new CoordinationError('invalid', message);
      break;
    default:
      typed = new CoordinationError('other', message);
  }
  (typed as { cause?: unknown }).cause = err;
  return typed;
}

async function call<T>(f: () => Promise<T>): Promise<T> {
  try {
    return await f();
  } catch (e) {
    throw coordinationError(e);
  }
}

function callSync<T>(f: () => T): T {
  try {
    return f();
  } catch (e) {
    throw coordinationError(e);
  }
}

// ── Value types ──────────────────────────────────────────────────────────────

/** An exact resource identity. @group Coordination */
export interface ResourceKey {
  namespace: string;
  key: Uint8Array;
}

/** A resource identity as accepted by the API; a string key is UTF-8. @group Coordination */
export interface ResourceKeyInput {
  namespace: string;
  key: Uint8Array | string;
}

/** The `(group, resource, epoch, holder)` fence a mutation carries. @group Coordination */
export interface CoordinationFence {
  groupId: string;
  resource: ResourceKey;
  epoch: number;
  holder: NodeId | string;
}

/** A Raft log id. @group Coordination */
export interface LogId {
  term: number;
  leaderNodeId: string;
  index: number;
}

/** One resource's committed row at the reply's cut. @group Coordination */
export interface ResourceSnapshot {
  groupId: string;
  resource: ResourceKey;
  epoch: number;
  holder?: NodeId;
  held: boolean;
  ttlMs: number;
  leaseRevision: number;
  valueRevision: number;
  aclRevision: number;
  value?: Uint8Array;
  valueRedacted: boolean;
}

/** The exact reply a journaled mutation resolved to. @group Coordination */
export interface MutationReply {
  groupId: string;
  clientId: string;
  sequence: number;
  requestHash: string;
  /** Whether the sequence was consumed (`dispositionCode === 1`). */
  consumed: boolean;
  dispositionCode: number;
  outcome: CoordinationOutcome | 'unknown';
  outcomeCode: number;
  reason: CoordinationReason | 'unknown';
  reasonCode: number;
  snapshot?: ResourceSnapshot;
  aclRevision: number;
  adminRevision: number;
  operationId?: string;
  membershipLogId?: LogId;
}

/** Canonical seed material for a new member. @group Coordination */
export interface MemberSeed {
  nodeId: string;
  ticket: string;
  signedJoinPermit?: Uint8Array;
}

/** A membership change request (usually a {@link PlacementPlan}). @group Coordination */
export interface MembershipChange {
  /** 16 bytes, hex. */
  operationId: string;
  expectedMembershipLogId?: LogId;
  desiredVoters: string[];
  retainedLearners?: string[];
  memberSeeds?: MemberSeed[];
}

import type {
  CoordinationAclStatus as AclStatus,
  CoordinationAdminStatus as AdminStatus,
  CoordinationAuditRecord as AuditRecord,
  CoordinationClientProbe as ClientProbe,
  CoordinationExportManifest as ExportManifest,
  CoordinationGroupStatus as GroupStatus,
  CoordinationMembershipStatus as MembershipStatus,
  CoordinationRetireReply as RetireReply,
  CoordinationSeparationSummary as SeparationSummary,
} from '@aster-rpc/transport';
export type {
  AclStatus,
  AdminStatus,
  AuditRecord,
  ClientProbe,
  ExportManifest,
  GroupStatus,
  MembershipStatus,
  RetireReply,
  SeparationSummary,
};

const UTF8 = new TextEncoder();

function bytesIn(v: Uint8Array | string): Buffer {
  if (typeof v === 'string') return Buffer.from(UTF8.encode(v));
  return Buffer.isBuffer(v) ? v : Buffer.from(v.buffer, v.byteOffset, v.byteLength);
}

function resourceIn(r: ResourceKeyInput): NativeResourceKey {
  return { namespace: r.namespace, key: bytesIn(r.key) };
}

function fenceIn(f: CoordinationFence): NativeFence {
  return { groupId: f.groupId, resource: resourceIn(f.resource), epoch: f.epoch, holder: f.holder };
}

function fenceOut(f: NativeFence): CoordinationFence {
  return { groupId: f.groupId, resource: { namespace: f.resource.namespace, key: f.resource.key }, epoch: f.epoch, holder: f.holder as NodeId };
}

function snapshotOut(s: NativeSnapshot): ResourceSnapshot {
  return {
    groupId: s.groupId,
    resource: { namespace: s.resource.namespace, key: s.resource.key },
    epoch: s.epoch,
    ...(s.holder != null ? { holder: s.holder as NodeId } : {}),
    held: s.held,
    ttlMs: s.ttlMs,
    leaseRevision: s.leaseRevision,
    valueRevision: s.valueRevision,
    aclRevision: s.aclRevision,
    ...(s.value != null ? { value: s.value } : {}),
    valueRedacted: s.valueRedacted,
  };
}

function replyOut(r: NativeReply): MutationReply {
  return {
    groupId: r.groupId,
    clientId: r.clientId,
    sequence: r.sequence,
    requestHash: r.requestHash,
    consumed: r.dispositionCode === 1,
    dispositionCode: r.dispositionCode,
    outcome: outcomeOf(r.outcomeCode),
    outcomeCode: r.outcomeCode,
    reason: reasonOf(r.reasonCode),
    reasonCode: r.reasonCode,
    ...(r.snapshot ? { snapshot: snapshotOut(r.snapshot) } : {}),
    aclRevision: r.aclRevision,
    adminRevision: r.adminRevision,
    ...(r.operationId != null ? { operationId: r.operationId } : {}),
    ...(r.membershipLogId ? { membershipLogId: r.membershipLogId } : {}),
  };
}

/**
 * The fence a granted/renewed reply's snapshot names — what `renew`,
 * `compareAndSet`, `release` and `releaseWithValue` carry.
 * @group Coordination
 */
export function fenceOf(snapshot: ResourceSnapshot): CoordinationFence {
  if (!snapshot.held || snapshot.holder == null) {
    throw new CoordinationError('invalid', 'coordination: the snapshot names no holder, so it carries no fence');
  }
  return { groupId: snapshot.groupId, resource: snapshot.resource, epoch: snapshot.epoch, holder: snapshot.holder };
}

// ── Resolved mutation ────────────────────────────────────────────────────────

/**
 * A resolved mutation: fsynced `ResolvedAwaitingAck` before it was returned.
 * {@link acknowledge} once the application has durably handled it. Dropping
 * it unacknowledged is safe — the same reply is redelivered by
 * {@link CoordinationClient.unacknowledged}, including after a restart.
 * @group Coordination
 */
export class ResolvedMutation {
  /** The exact reply the journal holds. */
  readonly reply: MutationReply;

  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeResolved,
  ) {
    this.reply = replyOut(native.reply);
  }

  get outcome(): CoordinationOutcome | 'unknown' {
    return this.reply.outcome;
  }

  get reason(): CoordinationReason | 'unknown' {
    return this.reply.reason;
  }

  /** A committed typed rejection (still needs acknowledging). */
  get rejected(): boolean {
    return this.reply.outcomeCode === 100;
  }

  get sequence(): number {
    return this.reply.sequence;
  }

  get snapshot(): ResourceSnapshot | undefined {
    return this.reply.snapshot;
  }

  /** `false` once acknowledged (or the client closed). */
  get pending(): boolean {
    return this.native.pending;
  }

  /** The rejection as an error (or `null` when the mutation took effect). */
  rejection(): CoordinationRejectedError | null {
    return this.rejected ? new CoordinationRejectedError(this.reply) : null;
  }

  /** Free the journal for the next sequence. */
  acknowledge(): Promise<void> {
    return call(() => this.native.acknowledge());
  }
}

function resolvedOut(r: NativeResolved | null): ResolvedMutation | null {
  return r ? new ResolvedMutation(r) : null;
}

// ── Lease ────────────────────────────────────────────────────────────────────

/** The result of {@link CoordinationClient.acquireLease}. @group Coordination */
export type CoordinationLeaseOutcome =
  | { granted: true; lease: CoordinationLease }
  | { granted: false; reason: DenyReason; snapshot: LeaseSnapshot };

/**
 * A held fenced register lease: renewed in Rust, fence gated by the
 * holder's self-limit. Lease transitions are journaled and acknowledged
 * automatically. `await using` releases on scope exit.
 * @group Coordination
 */
export class CoordinationLease implements AsyncDisposable {
  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeLease,
  ) {}

  get resource(): ResourceKey {
    const r = this.native.resource;
    return { namespace: r.namespace, key: r.key };
  }

  get status(): LeaseStatus {
    return this.native.status;
  }

  /**
   * The fence for a protected action, or `null` once the holder must stop.
   * Ask immediately before each action; never cache it across awaits.
   */
  fence(): CoordinationFence | null {
    const f = callSync(() => this.native.fence());
    return f ? fenceOut(f) : null;
  }

  /**
   * `compare_and_set` under the current fence. Throws
   * {@link CoordinationRejectedError} on a typed rejection.
   */
  async compareAndSet(expectedValueRevision: number, newValue: Uint8Array | string): Promise<MutationReply> {
    const reply = replyOut(await call(() => this.native.compareAndSet(expectedValueRevision, bytesIn(newValue))));
    if (reply.outcomeCode === 100) throw new CoordinationRejectedError(reply);
    return reply;
  }

  /** Graceful release. Idempotent. */
  release(): Promise<void> {
    return call(() => this.native.release());
  }

  /**
   * Atomic final handoff: write `finalValue` and release together, or
   * neither. Throws {@link CoordinationRejectedError} on a typed rejection.
   */
  async releaseWithValue(expectedValueRevision: number, finalValue: Uint8Array | string): Promise<MutationReply> {
    const reply = replyOut(
      await call(() => this.native.releaseWithValue(expectedValueRevision, bytesIn(finalValue))),
    );
    if (reply.outcomeCode === 100) throw new CoordinationRejectedError(reply);
    return reply;
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.release();
  }
}

// ── Clients ──────────────────────────────────────────────────────────────────

/** Options for {@link CoordinationClient.connect}. @group Coordination */
export interface CoordinationClientOptions {
  /**
   * Open member tickets — routing hints only (any address material is
   * registered as a dialling hint; a credential-bearing ticket is refused).
   */
  seedTickets?: string[];
  /** Durable journal directory. Persist it together with the node identity. */
  journalDir: string;
}

/** Options for {@link CoordinationReadClient.connect}. @group Coordination */
export interface CoordinationReadClientOptions {
  seedTickets?: string[];
}

/** Options for {@link CoordinationReadClient.auditStream}. @group Coordination */
export interface AuditStreamOptions {
  /** Start strictly after this log id (default: the oldest retained row). */
  after?: LogId;
  /** Stop after this many rows (default: the end of retained history). */
  limit?: number;
  /** Rows per request, 1..=10 000 (default 1 000). */
  pageSize?: number;
}

/** The §5.2 hard cap on one `audit_stream` request. @group Coordination */
export const MAX_AUDIT_PAGE = 10_000;

/**
 * Linearizable reads and status — no journal, no mutation.
 * @group Coordination
 */
export class CoordinationReadClient {
  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeReadClient,
  ) {}

  /** Open a read-only client for `groupId` (hex) on `node`. */
  static async connect(
    node: Node,
    groupId: string,
    options: CoordinationReadClientOptions = {},
  ): Promise<CoordinationReadClient> {
    return new CoordinationReadClient(
      callSync(() => loadNative().coordinationReadClientOpen(node.native, groupId, options.seedTickets)),
    );
  }

  /** The committed row at a leader-barriered cut. */
  async readLinearizable(resource: ResourceKeyInput): Promise<ResourceSnapshot> {
    return snapshotOut(await call(() => this.native.readLinearizable(resourceIn(resource))));
  }

  /** A resource's committed ACL (administrator). */
  inspectAcl(resource: ResourceKeyInput): Promise<AclStatus> {
    return call(() => this.native.inspectAcl(resourceIn(resource)));
  }

  /** The delegated administrator set (administrator). */
  inspectAdmins(): Promise<AdminStatus> {
    return call(() => this.native.inspectAdmins());
  }

  /** One membership intent's status, or the active one (administrator). */
  membershipStatus(operationId?: string): Promise<MembershipStatus> {
    return call(() => this.native.membershipStatus(operationId));
  }

  /** The S4-STATUS-CUT group status (administrator). */
  groupStatus(): Promise<GroupStatus> {
    return call(() => this.native.groupStatus());
  }

  /** Client-session diagnostics (never authority to reassign a sequence). */
  probeClient(targetNodeId: NodeId | string, targetClientId: string): Promise<ClientProbe> {
    return call(() => this.native.probeClient(targetNodeId, targetClientId));
  }

  /** One validated `audit_stream` request: up to `limit` rows after `after`. */
  auditPage(after: LogId | undefined, limit: number): Promise<AuditRecord[]> {
    return call(() => this.native.auditStream(after, limit));
  }

  /**
   * Retained audit rows in ascending log order (administrator), paged.
   * Each page is validated in Rust (envelope, bound, strict ascent).
   */
  async *auditStream(options: AuditStreamOptions = {}): AsyncGenerator<AuditRecord, void, undefined> {
    const pageSize = options.pageSize ?? 1_000;
    if (!Number.isInteger(pageSize) || pageSize < 1 || pageSize > MAX_AUDIT_PAGE) {
      throw new CoordinationError('invalid', `auditStream: pageSize must be 1..=${MAX_AUDIT_PAGE}`);
    }
    let remaining = options.limit ?? Number.POSITIVE_INFINITY;
    let after = options.after;
    while (remaining > 0) {
      const want = Math.min(pageSize, remaining);
      const page = await this.auditPage(after, want);
      for (const row of page) yield row;
      remaining -= page.length;
      if (page.length < want) return;
      after = page[page.length - 1]!.logId;
    }
  }

  /** The canonical recovery export, verified against its manifest, in memory. */
  async recoveryExport(): Promise<{ manifest: ExportManifest; body: Uint8Array }> {
    return call(() => this.native.recoveryExport());
  }

  /** Stream the export into `path` (renamed into place only once verified). */
  recoveryExportToFile(path: string): Promise<ExportManifest> {
    return call(() => this.native.recoveryExportToFile(path));
  }
}

/**
 * The durable, journaled coordination client — the only way to mutate.
 * Binds the node's identity to one group and owns the journal's lifetime
 * lock until {@link close} (also `await using`).
 * @group Coordination
 */
export class CoordinationClient implements AsyncDisposable {
  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeClient,
  ) {}

  /**
   * Open the durable client for `groupId` (hex) on `node`. Fails with
   * {@link CoordinationJournalLockedError} if the journal is held elsewhere.
   */
  static async connect(node: Node, groupId: string, options: CoordinationClientOptions): Promise<CoordinationClient> {
    return new CoordinationClient(
      callSync(() =>
        loadNative().coordinationClientOpen(node.native, groupId, options.seedTickets, options.journalDir),
      ),
    );
  }

  /** The group this client is bound to (hex). */
  get groupId(): string {
    return callSync(() => this.native.groupId);
  }

  /** This client's actor identity, as the group authenticates it. */
  get localId(): NodeId {
    return callSync(() => this.native.localId) as NodeId;
  }

  get closed(): boolean {
    return this.native.closed;
  }

  /** The journal's current client id (hex). Diagnostic only. */
  clientId(): Promise<string> {
    return call(() => this.native.clientId());
  }

  /**
   * The result a previous run resolved and never acknowledged, if any.
   * Call on startup, before any mutation.
   */
  async unacknowledged(): Promise<ResolvedMutation | null> {
    return resolvedOut(await call(() => this.native.unacknowledged()));
  }

  /**
   * Drive any half-finished request from a previous run to resolution.
   * `null` means the journal is ready for new mutations.
   */
  async resume(): Promise<ResolvedMutation | null> {
    return resolvedOut(await call(() => this.native.resume()));
  }

  /** `acquire` (§5.2 code 1). */
  async acquire(resource: ResourceKeyInput, ttlMs: number): Promise<ResolvedMutation> {
    return new ResolvedMutation(await call(() => this.native.acquire(resourceIn(resource), ttlMs)));
  }

  /** `renew` (§5.2 code 2). */
  async renew(fence: CoordinationFence): Promise<ResolvedMutation> {
    return new ResolvedMutation(await call(() => this.native.renew(fenceIn(fence))));
  }

  /** `compare_and_set` (§5.2 code 3): fence + expected value revision. */
  async compareAndSet(
    fence: CoordinationFence,
    expectedValueRevision: number,
    newValue: Uint8Array | string,
  ): Promise<ResolvedMutation> {
    return new ResolvedMutation(
      await call(() => this.native.compareAndSet(fenceIn(fence), expectedValueRevision, bytesIn(newValue))),
    );
  }

  /** `release` (§5.2 code 4). */
  async release(fence: CoordinationFence): Promise<ResolvedMutation> {
    return new ResolvedMutation(await call(() => this.native.release(fenceIn(fence))));
  }

  /** `release_with_value` (§5.2 code 5): the atomic final-value handoff. */
  async releaseWithValue(
    fence: CoordinationFence,
    expectedValueRevision: number,
    finalValue: Uint8Array | string,
  ): Promise<ResolvedMutation> {
    return new ResolvedMutation(
      await call(() => this.native.releaseWithValue(fenceIn(fence), expectedValueRevision, bytesIn(finalValue))),
    );
  }

  /** `revoke` (§5.2 code 6); unconditional unless `expectedEpoch` is given. */
  async revoke(resource: ResourceKeyInput, options: { expectedEpoch?: number } = {}): Promise<ResolvedMutation> {
    return new ResolvedMutation(await call(() => this.native.revoke(resourceIn(resource), options.expectedEpoch)));
  }

  /** `set_grant` (§5.2 code 7): exact {@link CoordinationRights} for one node. */
  async setGrant(
    resource: ResourceKeyInput,
    targetNodeId: NodeId | string,
    rights: number,
    expectedAclRevision: number,
  ): Promise<ResolvedMutation> {
    return new ResolvedMutation(
      await call(() => this.native.setGrant(resourceIn(resource), targetNodeId, rights, expectedAclRevision)),
    );
  }

  /** `set_admin` (§5.2 code 8). */
  async setAdmin(targetNodeId: NodeId | string, enabled: boolean, expectedAdminRevision: number): Promise<ResolvedMutation> {
    return new ResolvedMutation(await call(() => this.native.setAdmin(targetNodeId, enabled, expectedAdminRevision)));
  }

  /** `change_membership` (§5.2 code 9) — validated against a fresh status cut first. */
  async changeMembership(change: MembershipChange): Promise<ResolvedMutation> {
    return new ResolvedMutation(
      await call(() =>
        this.native.changeMembership(
          change.operationId,
          change.expectedMembershipLogId,
          change.desiredVoters,
          change.retainedLearners ?? [],
          (change.memberSeeds ?? []).map((s) => ({
            nodeId: s.nodeId,
            ticket: s.ticket,
            ...(s.signedJoinPermit ? { signedJoinPermit: bytesIn(s.signedJoinPermit) } : {}),
          })),
        ),
      ),
    );
  }

  /** `cancel_membership` (§5.2 code 11). */
  async cancelMembership(operationId: string, expectedPhaseCode: number): Promise<ResolvedMutation> {
    return new ResolvedMutation(await call(() => this.native.cancelMembership(operationId, expectedPhaseCode)));
  }

  /** Retire one of this actor's *other* client ids (operational cleanup). */
  retireOtherClient(targetClientId: string): Promise<RetireReply> {
    return call(() => this.native.retireOtherClient(targetClientId));
  }

  /**
   * One committed acquire; on grant, Rust renews in the background and the
   * fence stops at the self-limit (see {@link CoordinationLease}).
   */
  async acquireLease(resource: ResourceKeyInput, options?: LeaseOptions): Promise<CoordinationLeaseOutcome> {
    const r = await call(() => this.native.acquireLease(resourceIn(resource), options));
    if (r instanceof loadNative().CoordinationLease) {
      return { granted: true, lease: new CoordinationLease(r) };
    }
    const denied = r as NativeLeaseDenied;
    const s = denied.snapshot;
    return {
      granted: false,
      reason: denied.reason,
      snapshot: {
        resource: s.resource,
        epoch: s.epoch,
        ...(s.holder != null ? { holder: s.holder } : {}),
        held: s.held,
        ...(s.remainingMs != null ? { remainingMs: s.remainingMs } : {}),
      },
    };
  }

  /** The read/status surface, sharing this client's routing. */
  reads(): CoordinationReadClient {
    return new CoordinationReadClient(callSync(() => this.native.reads()));
  }

  /**
   * Release the journal lock. Outstanding {@link ResolvedMutation} and
   * {@link CoordinationLease} handles become inert (unacknowledged results
   * stay journaled for the next open). Idempotent.
   */
  close(): void {
    this.native.close();
  }

  async [Symbol.asyncDispose](): Promise<void> {
    this.close();
  }
}

// ── Placement planner (pure) ─────────────────────────────────────────────────

/** One eligible node, optionally with the seed material to join it. @group Coordination */
export interface PlacementCandidate {
  nodeId: string;
  memberSeed?: MemberSeed;
}

/** The application-owned placement policy (§7.7.1). @group Coordination */
export interface PlacementPolicy {
  revision: number;
  /** BLAKE3 of the application's canonical policy record (hex). */
  digest: string;
  authorityPublicKey: string;
  authorityDomain: string;
  targetVoters: number;
  targetRetainedLearners: number;
  eligible: PlacementCandidate[];
  pinnedVoters?: string[];
  excluded?: string[];
  minimumSeparatedVoterPairs?: number;
  replacementGraceMs?: number;
  allowHealthyRebalance?: boolean;
}

/** One planning input (membership facts usually from `groupStatus()`). @group Coordination */
export interface PlacementInput {
  groupId: string;
  genesisHash: string;
  deploymentProfileCode: number;
  membershipLogId?: LogId;
  currentVoters: string[];
  currentLearners?: string[];
  activeMembershipIntent?: boolean;
  graceCrossedMembers?: string[];
  policy: PlacementPolicy;
}

/** A complete desired membership, ready for `changeMembership`. @group Coordination */
export interface PlacementPlan {
  desiredVoters: string[];
  retainedLearners: string[];
  memberSeeds: MemberSeed[];
  expectedMembershipLogId?: LogId;
  separation: SeparationSummary;
}

/** The planner's answer (§7.7.2). @group Coordination */
export type PlacementRecommendation =
  | { kind: 'stable'; separation: SeparationSummary }
  | { kind: 'degraded'; why: string; separation: SeparationSummary }
  | { kind: 'converging' }
  | { kind: 'needs_provisioning'; candidates: string[]; desiredVoters: string[]; retainedLearners: string[] }
  | { kind: 'insufficient_evidence'; missing: string }
  | { kind: 'invalid_policy'; why: string }
  | { kind: 'plan'; plan: PlacementPlan };

/**
 * §7.7.2's pure, deterministic voter placement planner: local advice only.
 * `topology` comes from `node.topology().snapshot()`. Submit a `plan` with
 * {@link CoordinationClient.changeMembership} plus a fresh `operationId`.
 * @group Coordination
 */
export function placementPlan(input: PlacementInput, topology: TopologySnapshot): PlacementRecommendation {
  const snap = nativeTopologySnapshot(topology);
  if (!snap) {
    throw new CoordinationError('invalid', 'placementPlan: pass a snapshot from node.topology().snapshot()');
  }
  const seedIn = (s: MemberSeed) => ({
    nodeId: s.nodeId,
    ticket: s.ticket,
    ...(s.signedJoinPermit ? { signedJoinPermit: bytesIn(s.signedJoinPermit) } : {}),
  });
  const r = callSync(() =>
    loadNative().coordinationPlacementPlan(
      {
        ...input,
        policy: {
          ...input.policy,
          eligible: input.policy.eligible.map((c) => ({
            nodeId: c.nodeId,
            ...(c.memberSeed ? { memberSeed: seedIn(c.memberSeed) } : {}),
          })),
        },
      },
      snap,
    ),
  );
  switch (r.kind) {
    case 'stable':
      return { kind: 'stable', separation: r.separation! };
    case 'degraded':
      return { kind: 'degraded', why: r.why!, separation: r.separation! };
    case 'converging':
      return { kind: 'converging' };
    case 'needs_provisioning':
      return {
        kind: 'needs_provisioning',
        candidates: r.candidates!,
        desiredVoters: r.desiredVoters!,
        retainedLearners: r.retainedLearners!,
      };
    case 'insufficient_evidence':
      return { kind: 'insufficient_evidence', missing: r.missing! };
    case 'invalid_policy':
      return { kind: 'invalid_policy', why: r.why! };
    case 'plan': {
      const p = r.plan!;
      return {
        kind: 'plan',
        plan: {
          desiredVoters: p.desiredVoters,
          retainedLearners: p.retainedLearners,
          memberSeeds: p.memberSeeds.map((s) => ({
            nodeId: s.nodeId,
            ticket: s.ticket,
            ...(s.signedJoinPermit ? { signedJoinPermit: s.signedJoinPermit } : {}),
          })),
          ...(p.expectedMembershipLogId ? { expectedMembershipLogId: p.expectedMembershipLogId } : {}),
          separation: p.separation,
        },
      };
    }
  }
}

/** A fresh random 16-byte membership operation id (hex). @group Coordination */
export function newOperationId(): string {
  const b = new Uint8Array(16);
  globalThis.crypto.getRandomValues(b);
  return Buffer.from(b).toString('hex');
}

// ── Placement manager (JavaScript providers over upcalls) ────────────────────

/**
 * The application's verified placement policy record, re-read at every
 * observation (a changed revision / digest between planning and submission
 * discards the plan). Rust reads it synchronously on a worker thread, so
 * answer promptly.
 * @group Coordination
 */
export interface PlacementPolicySource {
  policy(ctx: UpcallContext): Promise<PlacementPolicy> | PlacementPolicy;
}

/** The idempotency tuple of one provisioning call (§7.7.3 step 4). @group Coordination */
export interface ProvisionRequest {
  groupId: string;
  /** Hex. */
  policyDigest: string;
  membershipLogId?: LogId;
  /** The candidate to provision (hex). */
  nodeId: string;
}

/**
 * The application's provisioner: start (or find) a persistent-identity Join
 * candidate for `request.nodeId` and return its seed. The same tuple may
 * arrive again (after a timeout); return the same candidate.
 * @group Coordination
 */
export interface PlacementProvisioner {
  provision(request: ProvisionRequest, ctx: UpcallContext): Promise<MemberSeed> | MemberSeed;
}

/** A serialized shared topology view (what a {@link TopologySource} returns). @group Coordination */
export interface TopologyView {
  clusters: ClusterView[];
  /** Corroborated far pairs (hex ids). */
  farPairs?: Array<[string, string]>;
  /** Separation-coverage rule the view was derived under (default 0). */
  separationCoverage?: number;
}

/** One atomic topology observation per call. @group Coordination */
export interface TopologySource {
  snapshot(ctx: UpcallContext): Promise<TopologyView> | TopologyView;
}

/** What one reconciliation pass did (§7.7.3). @group Coordination */
export interface PlacementReport {
  policyRevision: number;
  /** Hex; all zeros when no policy was read. */
  policyDigest: string;
  membershipLogId?: LogId;
  currentVoters: string[];
  currentLearners: string[];
  selectedVoters: string[];
  selectedLearners: string[];
  graceCrossedMembers: string[];
  separation?: SeparationSummary;
  /** The membership operation this pass submitted or found pending (hex). */
  pendingOperationId?: string;
  reason: string;
}

/** Options for {@link PlacementManager}. @group Coordination */
export interface PlacementManagerOptions {
  policy: PlacementPolicySource;
  provisioner: PlacementProvisioner;
  /** The node's own view (`node.topology()`, after `enableShared`) or a JavaScript source. */
  topology: Topology | TopologySource;
  /** The table to answer on (default: a private one, closed by `close()`). */
  upcalls?: Upcalls;
}

function seedOut(s: MemberSeed): { nodeId: string; ticket: string; signedJoinPermit?: Buffer } {
  return {
    nodeId: s.nodeId,
    ticket: s.ticket,
    ...(s.signedJoinPermit ? { signedJoinPermit: bytesIn(s.signedJoinPermit) } : {}),
  };
}

function policyOut(p: PlacementPolicy) {
  return {
    ...p,
    eligible: p.eligible.map((c) => ({ nodeId: c.nodeId, ...(c.memberSeed ? { memberSeed: seedOut(c.memberSeed) } : {}) })),
  };
}

/**
 * §7.7.3's placement manager. Runs **beside an application administrator**
 * (its client needs the administrator predicate to submit): each
 * {@link reconcile} delivers any journalled retry first, reads one
 * linearizable status cut, takes one topology snapshot, plans, provisions
 * missing seeds through your provisioner, then re-observes and submits only
 * if nothing moved. Membership changes only through the client's durable
 * journal. Your providers run over upcalls; one that throws or times out
 * rejects that pass.
 * @group Coordination
 */
export class PlacementManager implements Disposable {
  /** @internal */
  readonly native: import('@aster-rpc/transport').CoordinationPlacementManager;
  private readonly table: Upcalls;
  private readonly owned: boolean;
  private readonly removers: Array<() => void> = [];

  constructor(client: CoordinationClient, options: PlacementManagerOptions) {
    const { upcalls, owned } = adapterTable(options.upcalls);
    this.table = upcalls;
    this.owned = owned;
    try {
      this.removers.push(
        upcalls.handle('placement_policy', (async (_req: unknown, ctx: UpcallContext) =>
          policyOut(await options.policy.policy(ctx))) as never),
        upcalls.handle('placement_provision', (async (
          req: { groupId: string; policyDigest: string; membershipLogId?: LogId | null; nodeId: string },
          ctx: UpcallContext,
        ) =>
          seedOut(
            await options.provisioner.provision(
              {
                groupId: req.groupId,
                policyDigest: req.policyDigest,
                ...(req.membershipLogId ? { membershipLogId: req.membershipLogId } : {}),
                nodeId: req.nodeId,
              },
              ctx,
            ),
          )) as never),
      );
      const topology = options.topology;
      if (!(topology instanceof Topology)) {
        this.removers.push(
          upcalls.handle('topology_snapshot', (async (_req: unknown, ctx: UpcallContext) => {
            const v = await topology.snapshot(ctx);
            return {
              clusters: v.clusters.map((c) => ({ members: [...c.members], bridge: c.bridge, witnesses: [...c.witnesses] })),
              farPairs: (v.farPairs ?? []).map(([a, b]) => [a, b]),
              separationCoverage: v.separationCoverage ?? 0,
            };
          }) as never),
        );
      }
      this.native = callSync(
        () =>
          new (loadNative().CoordinationPlacementManager)(
            client.native,
            upcalls.native,
            topology instanceof Topology ? topology.native : undefined,
          ),
      );
    } catch (e) {
      this.close();
      throw e;
    }
  }

  /** The table the providers answer on. */
  get upcalls(): Upcalls {
    return this.table;
  }

  /** Run one full reconciliation pass (passes are serialized). */
  async reconcile(): Promise<PlacementReport> {
    let r;
    try {
      r = await this.native.reconcile();
    } catch (e) {
      throw coordinationError(e);
    }
    return {
      policyRevision: r.policyRevision,
      policyDigest: r.policyDigest,
      ...(r.membershipLogId ? { membershipLogId: r.membershipLogId } : {}),
      currentVoters: r.currentVoters,
      currentLearners: r.currentLearners,
      selectedVoters: r.selectedVoters,
      selectedLearners: r.selectedLearners,
      graceCrossedMembers: r.graceCrossedMembers,
      ...(r.separation ? { separation: r.separation } : {}),
      ...(r.pendingOperationId ? { pendingOperationId: r.pendingOperationId } : {}),
      reason: r.reason,
    };
  }

  /** Remove the provider handlers (and close a private table). */
  close(): void {
    for (const r of this.removers.splice(0)) r();
    if (this.owned) this.table.close();
  }

  [Symbol.dispose](): void {
    this.close();
  }
}
