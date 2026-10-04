/**
 * Node — a running Aster node (QUIC endpoint + blobs / docs / gossip),
 * wrapping the Rust `aster::Node` (plan D1).
 *
 * @example
 * ```ts
 * import { AsterConfig, Node } from '@aster-rpc/aster';
 *
 * await using node = await Node.start({
 *   ...AsterConfig.fromEnv(),
 *   storagePath: './data',            // omit for an in-memory node
 *   secretKeyFile: './data/node.key', // restart-stable identity
 * });
 * console.log(node.id);
 * ```
 */

import type {
  AdmissionRequests,
  BlobsClient,
  DocsClient,
  GossipClient,
  IrohConnection,
  Node as NativeNode,
} from '@aster-rpc/transport';
import { type AsterConfig, toNativeConfig } from './config.js';
import { Expose } from './expose.js';
import type { AdmissionHooks, AdmissionRequest, GatePolicy } from './gate.js';
import { ForeignLeaseSerializer, GrantorSerializer, type MemorySerializer } from './lease.js';
import { loadNative } from './native.js';
import { Topology } from './topology.js';
import { adapterTable, type UpcallContext, type Upcalls } from './upcalls.js';

/**
 * A node id: the node's Ed25519 public key as 64 lowercase hex characters.
 * Branded so ids are not confused with other hex strings.
 * @group Node
 */
export type NodeId = string & { readonly __asterId: 'NodeId' };

const NODE_ID_RE = /^[0-9a-f]{64}$/;

/** @group Node */
export const NodeId = {
  /** Validate and brand a node id (64 hex characters, case-insensitive). */
  parse(value: string): NodeId {
    const v = value.trim().toLowerCase();
    if (!NODE_ID_RE.test(v)) {
      throw new TypeError(`invalid node id (expected 64 hex characters): ${JSON.stringify(value)}`);
    }
    return v as NodeId;
  },
  /** Whether `value` is a well-formed node id. */
  is(value: unknown): value is NodeId {
    return typeof value === 'string' && NODE_ID_RE.test(value);
  },
};

/**
 * Where to reach a peer: its id plus any known direct addresses / relay URL.
 * @group Node
 */
export interface NodeAddr {
  nodeId: NodeId | string;
  /** `"ip:port"` socket addresses. */
  directAddresses?: string[];
  relayUrl?: string;
}

/** Options for {@link Node.serveLeaseGrantor}. @group Lease */
export interface LeaseGrantorOptions {
  /** Let any admitted caller `revoke` (default: only the `operator` role). */
  openRevoke?: boolean;
}

/** Options for {@link Node.start} beyond the config. @group Node */
export interface NodeStartOptions {
  /**
   * Gate 0: decide inbound admission in Rust with this policy (turns
   * `enableHooks` on). Without it, a node with `enableHooks` hands each
   * decision to {@link Node.admission}.
   */
  gate?: GatePolicy;
}

/**
 * A running Aster node. Start one with {@link Node.start}; close it with
 * `close()` or `await using`.
 *
 * @group Node
 */
export class Node implements AsyncDisposable {
  private _closed = false;
  private _closing: Promise<void> | null = null;

  private constructor(
    /**
     * The NAPI node handle, for the binding's own RPC / hook plumbing.
     * Not a stable API.
     * @internal
     */
    readonly native: NativeNode,
  ) {}

  /**
   * Start a node. In-memory unless `config.storagePath` is set. Only the
   * given config is used — merge {@link AsterConfig.fromEnv} in yourself to
   * honour `ASTER_*` variables.
   */
  static async start(config: AsterConfig = {}, options: NodeStartOptions = {}): Promise<Node> {
    const native = loadNative();
    return new Node(await native.Node.start(toNativeConfig(config), options.gate?.native));
  }

  /** This node's id. */
  get id(): NodeId {
    return this.native.nodeId() as NodeId;
  }

  /** This node's dialable address (id + current direct addresses). */
  get addr(): NodeAddr {
    return { nodeId: this.id, directAddresses: this.directAddresses() };
  }

  /**
   * Direct socket addresses (`"ip:port"`) currently advertised. Empty until
   * the endpoint has bound and discovered its local addresses.
   */
  directAddresses(): string[] {
    return this.native.directAddresses();
  }

  /**
   * The node's 32-byte secret key. Persist it (or set `secretKeyFile`) for a
   * restart-stable identity — the store does not keep it.
   */
  exportSecretKey(): Uint8Array {
    return new Uint8Array(this.native.exportSecretKey());
  }

  /** Whether the node was started with admission hooks (or a gate). */
  get hasHooks(): boolean {
    return this.native.hasHooks();
  }

  /**
   * Attach a Gate-0 policy (`null` detaches it). Applies from the next
   * admission decision. Needs a node started with hooks or a gate.
   */
  setGate(gate: GatePolicy | null): void {
    this.native.setGate(gate?.native ?? null);
  }

  /**
   * Admission requests raised while no gate is attached — iterate with
   * `for await` and `accept()` / `reject(code, reason)` each. Outbound
   * dials by this node appear as `direction === 'outbound'`. One iterator
   * per node; needs a node started with `enableHooks: true`.
   */
  admission(): AsyncIterable<AdmissionRequest> & AdmissionRequests {
    return this.native.admission();
  }

  /**
   * Answer this node's admission requests with `hooks` over upcalls (kinds
   * `admission_handshake` / `admission_connect`) — the alternative to
   * {@link admission}; one or the other per node. An attached gate still
   * decides first. Without an answer (error, timeout) the upcall policy's
   * failure mode decides (fail-closed by default: a handshake is rejected
   * with `403 "hook unavailable"`); keep its timeout below the node's
   * `hookTimeoutMs`. Outbound dials are allowed unless `hooks.connect`
   * says otherwise. `close()` on the result removes the hooks (later
   * requests then get the failure mode).
   */
  serveAdmission(hooks: AdmissionHooks, options: { upcalls?: Upcalls } = {}): { readonly upcalls: Upcalls; close(): void } {
    const { upcalls, owned } = adapterTable(options.upcalls);
    const removers: Array<() => void> = [];
    const close = () => {
      for (const r of removers.splice(0)) r();
      if (owned) upcalls.close();
    };
    try {
      removers.push(
        upcalls.handle('admission_handshake', (async (
          req: { peer: string; alpn: Buffer; isAlive: boolean },
          ctx: UpcallContext,
        ) => {
          const answer = await hooks.handshake(
            { peer: req.peer as NodeId, alpn: req.alpn.toString('utf8'), alpnBytes: new Uint8Array(req.alpn), isAlive: req.isAlive },
            ctx,
          );
          if (answer === true) return { accept: true };
          if (answer === false || answer == null) return { accept: false };
          return {
            accept: false,
            ...(answer.code != null ? { code: answer.code } : {}),
            ...(answer.reason != null ? { reason: Buffer.from(answer.reason) } : {}),
          };
        }) as never),
        upcalls.handle('admission_connect', (async (req: { peer: string; alpn: Buffer }, ctx: UpcallContext) => ({
          allowed: hooks.connect
            ? (await hooks.connect(
                { peer: req.peer as NodeId, alpn: req.alpn.toString('utf8'), alpnBytes: new Uint8Array(req.alpn) },
                ctx,
              )) === true
            : true,
        })) as never),
      );
      this.native.serveAdmissionUpcalls(upcalls.native);
    } catch (e) {
      close();
      throw e;
    }
    return { upcalls, close };
  }

  /** Blob store client. */
  blobs(): BlobsClient {
    return this.native.blobsClient();
  }

  /** Document sync client. */
  docs(): DocsClient {
    return this.native.docsClient();
  }

  /** Gossip pub-sub client. */
  gossip(): GossipClient {
    return this.native.gossipClient();
  }

  /**
   * Open a connection to `peer` on `alpn`. With direct addresses / a relay
   * URL the peer is dialed directly; a bare id relies on discovery.
   */
  async connect(peer: NodeId | string | NodeAddr, alpn: string | Uint8Array): Promise<IrohConnection> {
    const alpnBuf = Buffer.from(typeof alpn === 'string' ? Buffer.from(alpn) : alpn);
    if (typeof peer === 'string') {
      return this.native.connect(peer, alpnBuf);
    }
    const direct = peer.directAddresses?.length ? peer.directAddresses : undefined;
    if (!direct && !peer.relayUrl) {
      return this.native.connect(peer.nodeId, alpnBuf);
    }
    return this.native.connectNodeAddr(peer.nodeId, alpnBuf, direct, peer.relayUrl);
  }

  /** Transport metrics in Prometheus text exposition format. */
  metricsPrometheus(): string {
    return this.native.metricsPrometheus();
  }

  /**
   * This node's topology view (v1 per-peer proximity; v2 shared swarm).
   * Populates only when the node runs with `enableMonitoring: true`.
   */
  topology(): Topology {
    return new Topology(this.native.topology());
  }

  /**
   * A lease backend on the designated grantor `grantor` (an
   * `aster.lease.Grantor` service reached over RPC). The grantor must be
   * dialable by id (discovery, or its address added beforehand).
   */
  async leaseGrantor(grantor: NodeId | string): Promise<GrantorSerializer> {
    return new GrantorSerializer(await this.native.leaseGrantor(grantor));
  }

  /**
   * Serve `aster.lease.Grantor` over `serializer`, making this node a
   * designated grantor. Start the node with `alpns: ['aster/1']`. This owns
   * the node's RPC accept loop — don't also run an RPC server on it.
   * Returns a handle whose `close()` stops serving.
   */
  serveLeaseGrantor(
    serializer: MemorySerializer | ForeignLeaseSerializer,
    options: LeaseGrantorOptions = {},
  ): { close(): void } {
    if (serializer instanceof ForeignLeaseSerializer) {
      return this.native.serveForeignLeaseGrantor(serializer.native, options);
    }
    return this.native.serveLeaseGrantor(serializer.native, options);
  }

  /**
   * A fresh exposure registry (see {@link Expose}); its authorizers, HTTP
   * handlers and route policies answer on `options.upcalls` or a private
   * table.
   */
  expose(options: { upcalls?: Upcalls } = {}): Expose {
    return new Expose(this.native.expose(), options.upcalls);
  }

  /**
   * Own this node's accept loop for expose: inbound connections on one of
   * `alpns` (start the node with them in `alpns`) are fed to `expose`.
   * Don't also run an RPC server on the node. `close()` stops accepting.
   * Consumers open a stream with `connection.openExposeStream(serviceId)`.
   */
  serveExpose(expose: Expose, alpns: string[]): { close(): void } {
    return this.native.serveExpose(expose.native, alpns);
  }

  /** Whether `close()` has been called. */
  get closed(): boolean {
    return this._closed;
  }

  /** Shut down cleanly (flushes persistent state first). Idempotent. */
  close(): Promise<void> {
    if (!this._closing) {
      this._closed = true;
      this._closing = this.native.close();
    }
    return this._closing;
  }

  /** `await using` support. */
  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }
}
