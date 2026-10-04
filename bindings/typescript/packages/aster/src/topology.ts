/**
 * Topology — "where are my peers?" (Rust `aster::topology`).
 *
 * v1 is a passive per-peer proximity view (locality ladder, RTT, loss,
 * throughput, confidence) from connections the node already makes; it needs
 * a node started with `enableMonitoring: true`. v2 joins a shared topology
 * swarm keyed by a namespace secret (usually delivered in a sealed grant)
 * and derives the swarm-wide cluster view:
 *
 * ```ts
 * const topo = node.topology();
 * topo.peers();                                  // v1
 * await topo.enableShared(secret, { bootstrap: [rootId] });
 * const snap = await topo.snapshot();            // v2: one consistent view
 * snap.clusters; snap.separated(a, b);           // 'unknown' | 'connected' | 'separated'
 * ```
 *
 * Without an `admitted` filter every holder of the secret counts. With one
 * ({@link AdmittedFilter}, or a function), record authors and vertices are
 * filtered at read time by your JavaScript — see {@link AdmittedFilter} for
 * its cache semantics (the Rust filter is synchronous and never waits for
 * JavaScript).
 */

import type {
  Topology as NativeTopology,
  PeerView as NativePeerView,
  ClusterView as NativeClusterView,
  TopologySnapshot as NativeTopologySnapshot,
} from '@aster-rpc/transport';
import type {
  TopologyAdmittedFilter as NativeAdmittedFilter,
} from '@aster-rpc/transport';
import { loadNative } from './native.js';
import type { NodeId } from './node.js';
import { adapterTable, type UpcallContext, type UpcallPolicy, type Upcalls } from './upcalls.js';

/** Locality ladder level; lower is closer. @group Topology */
export type LadderLevel = 'same_host' | 'same_lan' | 'same_site' | 'same_region' | 'far';

/** `separated()` verdict. `unknown` is a real answer, never evidence of distance. @group Topology */
export type SeparationVerdict = 'unknown' | 'connected' | 'separated';

/** One peer's proximity view (v1). @group Topology */
export interface PeerView {
  nodeId: NodeId;
  level: LadderLevel;
  /** 0 (`same_host`) … 4 (`far`). */
  levelRank: number;
  /** The signal that justified the level. */
  levelReason: string;
  /** Smoothed RTT, µs (absent until measured). */
  rttUs?: number;
  /** EWMA of |ΔRTT|, µs (absent until two samples). */
  jitterUs?: number;
  lossPpm: number;
  throughputBps: number;
  cwndCeilingBps: number;
  direct: boolean;
  relayUrl?: string;
  /** The selected direct path's `ip:port`. */
  remoteAddr?: string;
  congestionEvents: number;
  samples: number;
  /** 0 … 0.99. */
  confidence: number;
  lastMeasured: Date;
  /** Since when RTT has held under the cluster threshold (absent = not held). */
  clusterHeldSince?: Date;
  isConnected: boolean;
}

/** One derived cluster (v2). @group Topology */
export interface ClusterView {
  /** Sorted member ids (admitted and live only). */
  members: readonly NodeId[];
  bridge: NodeId;
  /** Top members by bridge score (includes the bridge). */
  witnesses: readonly NodeId[];
}

/**
 * One immutable topology observation: every answer comes from the same
 * derived view. Take one snapshot for a placement-grade decision instead of
 * mixing `clusters()` and `separated()` calls.
 * @group Topology
 */
export interface TopologySnapshot {
  readonly clusters: readonly ClusterView[];
  /** The cluster containing `node` in this view. */
  clusterOf(node: NodeId | string): ClusterView | undefined;
  /** Separation verdict between two nodes, from this view. */
  separated(a: NodeId | string, b: NodeId | string): SeparationVerdict;
}

/** Reader timing / coverage rules (design-doc defaults when unset). @group Topology */
export interface TopologyTiming {
  minHoldMs?: number;
  livenessTtlMs?: number;
  edgeTtlMs?: number;
  maxSkewMs?: number;
  separationCoverage?: number;
  witnessR?: number;
}

/** Options for {@link Topology.enableShared}. @group Topology */
export interface TopologySwarmOptions {
  /** Peers to start doc sync with, beyond already-connected peers. */
  bootstrap?: Array<NodeId | string>;
  /** Heartbeat cadence for this node's records, ms (default 60 000). */
  positionRefreshMs?: number;
  timing?: TopologyTiming;
  /**
   * Read-time producer filter. A function gets a private
   * {@link AdmittedFilter} with default options (kept for the node's
   * lifetime); pass your own filter to tune it, `prefetch`, or push
   * revocations.
   */
  admitted?: AdmittedFilter | AdmittedFilterFn;
}

function peerOut(p: NativePeerView): PeerView {
  return {
    nodeId: p.nodeId as NodeId,
    level: p.level,
    levelRank: p.levelRank,
    levelReason: p.levelReason,
    ...(p.rttUs != null ? { rttUs: p.rttUs } : {}),
    ...(p.jitterUs != null ? { jitterUs: p.jitterUs } : {}),
    lossPpm: p.lossPpm,
    throughputBps: p.throughputBps,
    cwndCeilingBps: p.cwndCeilingBps,
    direct: p.direct,
    ...(p.relayUrl != null ? { relayUrl: p.relayUrl } : {}),
    ...(p.remoteAddr != null ? { remoteAddr: p.remoteAddr } : {}),
    congestionEvents: p.congestionEvents,
    samples: p.samples,
    confidence: p.confidence,
    lastMeasured: new Date(p.lastMeasuredUnixMs),
    ...(p.clusterHeldSinceUnixMs > 0 ? { clusterHeldSince: new Date(p.clusterHeldSinceUnixMs) } : {}),
    isConnected: p.isConnected,
  };
}

function clusterOut(c: NativeClusterView): ClusterView {
  return Object.freeze({
    members: Object.freeze(c.members as NodeId[]),
    bridge: c.bridge as NodeId,
    witnesses: Object.freeze(c.witnesses as NodeId[]),
  });
}

/**
 * This node's topology view — `node.topology()`.
 * @group Topology
 */
export class Topology {
  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeTopology,
  ) {}

  /** Whether the node runs with monitoring (the view's prerequisite). */
  get hasMonitoring(): boolean {
    return this.native.hasMonitoring;
  }

  /** Every known peer (recently disconnected ones too, decaying). Empty without monitoring. */
  peers(): PeerView[] {
    return this.native.peers().map(peerOut);
  }

  /** One peer's view, or `undefined`. */
  peer(id: NodeId | string): PeerView | undefined {
    const p = this.native.peer(id);
    return p ? peerOut(p) : undefined;
  }

  /**
   * Join the topology swarm keyed by the shared namespace `secret` (hex or
   * 32 bytes) and start publishing this node's measurements. Needs
   * monitoring.
   */
  enableShared(secret: string | Uint8Array, options?: TopologySwarmOptions): Promise<void> {
    const { admitted, ...rest } = options ?? {};
    const filter = admitted == null ? undefined : admitted instanceof AdmittedFilter ? admitted : new AdmittedFilter(admitted);
    return this.native.enableShared(secret, options ? rest : undefined, filter?.native);
  }

  /** The derived clusters of the joined swarm. */
  async clusters(): Promise<ClusterView[]> {
    return (await this.native.clusters()).map(clusterOut);
  }

  /** The cluster containing this node, or `undefined` while it is not (yet) a vertex. */
  async myCluster(): Promise<ClusterView | undefined> {
    const c = await this.native.myCluster();
    return c ? clusterOut(c) : undefined;
  }

  /** Separation verdict for two nodes (each call may see a different view; see {@link snapshot}). */
  separated(a: NodeId | string, b: NodeId | string): Promise<SeparationVerdict> {
    return this.native.separated(a, b);
  }

  /** One immutable observation, answering everything from the same view. */
  async snapshot(): Promise<TopologySnapshot> {
    const snap = await this.native.snapshot();
    const clusters = Object.freeze(snap.clusters().map(clusterOut));
    const view: TopologySnapshot = Object.freeze({
      clusters,
      clusterOf: (node: NodeId | string) => clusters.find((c) => c.members.includes(node as NodeId)),
      separated: (a: NodeId | string, b: NodeId | string) => snap.separated(a, b),
    });
    NATIVE_SNAPSHOTS.set(view, snap);
    return view;
  }
}

const NATIVE_SNAPSHOTS = new WeakMap<TopologySnapshot, NativeTopologySnapshot>();

/**
 * The native view behind a snapshot from {@link Topology.snapshot} (used by
 * the coordination placement planner).
 * @internal
 */
export function nativeTopologySnapshot(snapshot: TopologySnapshot): NativeTopologySnapshot | undefined {
  return NATIVE_SNAPSHOTS.get(snapshot);
}

// ── JavaScript admitted filter (R6 upcalls) ──────────────────────────────────

/**
 * Decides, for a batch of hex node ids, which are currently admitted
 * producers. Answer one boolean per id, in order.
 * @group Topology
 */
export type AdmittedFilterFn = (nodeIds: NodeId[], ctx: UpcallContext) => Promise<boolean[]> | boolean[];

/** Options for {@link AdmittedFilter}. @group Topology */
export interface AdmittedFilterOptions {
  /** How long an answer is served without a refresh (default 30 000 ms). */
  ttlMs?: number;
  /** How long a failed batch's fallback answer is cached (default 1 000 ms). */
  failureRetryMs?: number;
  /** Most ids per upcall (default 256). */
  maxBatch?: number;
  /** The table to answer on (default: a private one, closed by `close()`). */
  upcalls?: Upcalls;
  /**
   * Policy of the `topology_admitted` kind on that table. `failureMode` is
   * also the answer for ids not cached yet: `fail_closed` (default) filters
   * them, `fail_open` counts them.
   */
  policy?: UpcallPolicy;
}

/** Lookup counters of an {@link AdmittedFilter}. @group Topology */
export interface AdmittedFilterStats {
  /** Answered from a fresh entry. */
  hits: number;
  /** Answered from a stale entry (refresh queued). */
  stale: number;
  /** No entry: failure-mode answer, resolution queued. */
  misses: number;
  /** Batched upcalls issued. */
  upcalls: number;
}

/**
 * A topology `admitted` filter implemented in JavaScript.
 *
 * **Sync / cache semantics.** Rust's filter is synchronous and runs inside
 * every shared-view derivation (at most once a second), once per record
 * author and live vertex, so it never waits for JavaScript. It answers from
 * a cache:
 *
 * - a fresh entry answers directly;
 * - a stale entry (older than `ttlMs`) answers with the cached value while
 *   a refresh is queued (stale-while-revalidate);
 * - a miss answers with the failure mode (default: *not* admitted) while
 *   the id is queued.
 *
 * Queued ids are resolved by one background task in **batched** calls to
 * your function. So a newly seen producer counts from the first view
 * derived after its batch is answered (typically the next view, about a
 * second later), and a revoked one stops counting after `ttlMs` — or at the
 * next view if you push it with {@link set} / {@link invalidate}. A failed
 * batch (error, timeout) caches the fallback for `failureRetryMs` without
 * overwriting a real cached answer. Warm the cache with {@link prefetch}
 * before `enableShared` to skip the initial fallback window.
 * @group Topology
 */
export class AdmittedFilter implements Disposable {
  /** @internal */
  readonly native: NativeAdmittedFilter;
  private readonly table: Upcalls;
  private readonly owned: boolean;
  private readonly remove: () => void;

  constructor(filter: AdmittedFilterFn | { admitted: AdmittedFilterFn }, options: AdmittedFilterOptions = {}) {
    const fn: AdmittedFilterFn = typeof filter === 'function' ? filter : (ids, ctx) => filter.admitted(ids, ctx);
    const { upcalls, owned } = adapterTable(options.upcalls);
    this.table = upcalls;
    this.owned = owned;
    try {
      if (options.policy) upcalls.setPolicy('topology_admitted', options.policy);
      this.remove = upcalls.handle('topology_admitted', (async (req: { nodeIds: string[] }, ctx: UpcallContext) => {
        const answer = await fn(req.nodeIds as NodeId[], ctx);
        if (!Array.isArray(answer) || answer.length !== req.nodeIds.length) {
          throw new Error(`admitted filter answered ${Array.isArray(answer) ? answer.length : 'no list'} for ${req.nodeIds.length} ids`);
        }
        return { admitted: answer.map((a) => a === true) };
      }) as never);
      this.native = new (loadNative().TopologyAdmittedFilter)(upcalls.native, {
        ...(options.ttlMs != null ? { ttlMs: options.ttlMs } : {}),
        ...(options.failureRetryMs != null ? { failureRetryMs: options.failureRetryMs } : {}),
        ...(options.maxBatch != null ? { maxBatch: options.maxBatch } : {}),
      });
    } catch (e) {
      if (owned) upcalls.close();
      throw e;
    }
  }

  /** The table this filter answers on. */
  get upcalls(): Upcalls {
    return this.table;
  }

  /** The answer the filter gives `node` right now (a lookup: queues a refresh on a miss / stale entry). */
  isAdmitted(node: NodeId | string): boolean {
    return this.native.isAdmitted(node);
  }

  /** Resolve `nodes` now with one call to your function and cache the answers. */
  prefetch(nodes: Array<NodeId | string>): Promise<void> {
    return this.native.prefetch(nodes);
  }

  /** Push an answer (e.g. on revocation); served for `ttlMs`, effective at the next view. */
  set(node: NodeId | string, admitted: boolean): void {
    this.native.set(node, admitted);
  }

  /** Forget one id; the next lookup misses and re-resolves. */
  invalidate(node: NodeId | string): void {
    this.native.invalidate(node);
  }

  /** Forget every id. */
  clear(): void {
    this.native.clear();
  }

  stats(): AdmittedFilterStats {
    return this.native.stats();
  }

  /**
   * Stop answering (later lookups get the failure mode once their entries
   * expire). Closes a private table.
   */
  close(): void {
    this.remove();
    if (this.owned) this.table.close();
  }

  [Symbol.dispose](): void {
    this.close();
  }
}
