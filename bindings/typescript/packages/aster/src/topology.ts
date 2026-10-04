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
 * The read-time `admitted` filter of `enableShared` (a JavaScript callback)
 * waits on the binding upcall mechanism; every holder of the secret counts.
 */

import type {
  Topology as NativeTopology,
  PeerView as NativePeerView,
  ClusterView as NativeClusterView,
  TopologySnapshot as NativeTopologySnapshot,
} from '@aster-rpc/transport';
import type { NodeId } from './node.js';

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
    return this.native.enableShared(secret, options);
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
