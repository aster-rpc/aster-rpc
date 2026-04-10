/**
 * High-level AsterServer and AsterClient wrappers.
 *
 * These provide one-liner setup for the common case, hiding the
 * details of endpoint creation, admission, and transport wiring.
 */

import { ServiceRegistry } from './service.js';
import { LocalTransport } from './transport/local.js';
import { createClient, type AsterClient as ClientProxy } from './client.js';
import type { AsterTransport } from './transport/base.js';
import type { AsterConfig } from './config.js';
import { configFromEnv } from './config.js';
import { createLogger, type AsterLogger } from './logging.js';
import { HealthServer } from './health.js';
import { DEFAULT_BACKOFF, type ExponentialBackoff } from './types.js';
import { JsonCodec } from './codec.js';
import { RpcServer } from './server.js';
import { handleConsumerAdmissionConnection, type ConsumerAdmissionOpts, type ServiceSummary } from './trust/consumer.js';
import { handleDelegatedAdmissionConnection, type DelegatedAdmissionPolicy } from './trust/delegated.js';
import { MeshEndpointHook } from './trust/hooks.js';
import { PeerAttributeStore } from './peer-store.js';
import { CapabilityInterceptor } from './interceptors/capability.js';
import type { Interceptor } from './interceptors/base.js';
import { canonicalXlangBytes, contractIdFromContract, fromServiceInfo } from './contract/identity.js';
import type { ContractManifest, ManifestMethod, ManifestField } from './contract/manifest.js';

// ── Constants ────────────────────────────────────────────────────────────────

const ALPN_CONSUMER_ADMISSION = 'aster.consumer_admission';
const ALPN_PRODUCER_ADMISSION = 'aster.producer_admission';
const ALPN_DELEGATED_ADMISSION = 'aster.admission';
const RPC_ALPN = 'aster/1';

// ── AsterServer ──────────────────────────────────────────────────────────────

/** Options for AsterServer. */
export interface AsterServerOptions {
  services: object[];
  config?: Partial<AsterConfig>;
  /** Path to .aster-identity file. Overrides config.identityFile. */
  identity?: string;
  /** Peer name for identity file lookup. */
  peer?: string;
  /** Allow all consumers without credentials (dev mode). Default: true. */
  allowAllConsumers?: boolean;
  interceptors?: unknown[];
}

/**
 * High-level Aster RPC server.
 *
 * Creates an IrohNode, serves RPC over QUIC, handles consumer admission,
 * and prints a startup banner.
 *
 * @example
 * ```ts
 * const server = new AsterServer({
 *   services: [new MissionControl()],
 * });
 * await server.start();
 * console.log(server.address);
 * await server.serve();
 * ```
 */
export class AsterServer {
  readonly registry: ServiceRegistry;
  readonly config: AsterConfig;
  readonly logger: AsterLogger;
  private health: HealthServer;
  private _node: any = null;
  private _rpcServer: RpcServer | null = null;
  private _hook: MeshEndpointHook;
  private _peerStore: PeerAttributeStore;
  private _delegationPolicies: Map<string, DelegatedAdmissionPolicy> = new Map();
  private _running = false;
  private _closed = false;
  private _allowAllConsumers: boolean;
  private _userInterceptors: unknown[] = [];
  private _signalHandlers: (() => void)[] = [];
  private _serviceSummaries: ServiceSummary[] = [];
  private _registryNamespace = '';
  private _servePromise: Promise<void> | null = null;
  private _admissionAbort: AbortController | null = null;

  constructor(opts: AsterServerOptions) {
    this.config = { ...configFromEnv(), ...opts.config } as AsterConfig;
    if (opts.identity) {
      this.config.identityFile = opts.identity;
    }
    this.logger = createLogger({
      format: this.config.logFormat,
      level: this.config.logLevel,
      mask: this.config.logMask,
    });
    this.registry = new ServiceRegistry();
    this.health = new HealthServer({
      port: this.config.healthPort,
      host: this.config.healthHost,
    });
    this._hook = new MeshEndpointHook();
    this._peerStore = new PeerAttributeStore();
    this._allowAllConsumers = opts.allowAllConsumers ?? true;
    this._userInterceptors = opts.interceptors ?? [];

    for (const svc of opts.services) {
      this.registry.register(svc);
    }
  }

  /**
   * Create the IrohNode and prepare for serving. Idempotent.
   */
  async start(): Promise<void> {
    if (this._node) return;

    // Load native addon
    const native = await loadNative();
    if (!native) {
      throw new Error(
        'Aster native addon not found. Build with: cd native && npx napi build --release --platform',
      );
    }

    // Initialize contract identity binding (needed for _publishContracts)
    const { setNativeContract } = await import('./contract/identity.js');
    setNativeContract(native);

    // Create node with RPC + admission ALPNs
    const alpns = [
      Buffer.from(RPC_ALPN),
      Buffer.from(ALPN_CONSUMER_ADMISSION),
      Buffer.from(ALPN_PRODUCER_ADMISSION),
      Buffer.from(ALPN_DELEGATED_ADMISSION),
    ];
    this._node = await native.IrohNode.memoryWithAlpns(alpns);

    // Build service summaries for admission response
    // Encode the server's own node ID as the RPC channel address
    const nodeId = this._node.nodeId();
    const rpcAddr = Buffer.from(nodeId).toString('base64');
    this._serviceSummaries = [];
    for (const info of this.registry.getAllServices()) {
      this._serviceSummaries.push({
        name: info.name,
        version: info.version,
        contractId: '',
        pattern: info.scoped ?? 'shared',
        methods: Object.keys(info.methods),
        channels: { rpc: rpcAddr },
      });
    }

    // Auto-wire CapabilityInterceptor if any service declares requires=
    const interceptors: Interceptor[] = [...(this._userInterceptors as Interceptor[])];
    let anyHasRequires = false;
    for (const info of this.registry.getAllServices()) {
      if (info.requires) anyHasRequires = true;
      for (const mi of info.methods.values()) {
        if (mi.requires) anyHasRequires = true;
      }
    }
    const hasCapInterceptor = interceptors.some(i => i instanceof CapabilityInterceptor);
    if ((!this._allowAllConsumers || anyHasRequires) && !hasCapInterceptor) {
      const cap = new CapabilityInterceptor();
      // Register requirements from service/method declarations
      for (const info of this.registry.getAllServices()) {
        for (const [methodName, mi] of info.methods.entries()) {
          const req = mi.requires ?? info.requires;
          if (req) cap.setRequirement(info.name, methodName, req);
        }
      }
      interceptors.unshift(cap);
    }

    // Create the RPC server (uses JsonCodec for cross-language compat)
    this._rpcServer = new RpcServer({
      registry: this.registry,
      codec: new JsonCodec(),
      interceptors,
      logger: this.logger,
    });

    // Publish contracts to registry doc (non-fatal on failure)
    await this._publishContracts();

    await this.health.start();
    this._running = true;
    this._printBanner();

    // Always log startup info (visible even when stderr is not a TTY)
    const serviceNames = this._serviceSummaries.map(s => s.name).join(', ');
    this.logger.info(
      `server starting runtime=typescript services=[${serviceNames}] mode=${this._allowAllConsumers ? 'open-gate' : 'trusted'}`,
    );
  }

  /**
   * Create a registry doc and publish each service's contract.
   *
   * After publication, `_registryNamespace` is set to the 64-char hex
   * namespace ID so the admission response can return it.
   *
   * Non-fatal: if publication fails, the server still works — consumers
   * just won't get rich contract metadata.
   */
  private async _publishContracts(): Promise<void> {
    try {
      const dc = this._node.docsClient();
      const bc = this._node.blobsClient();

      // Step 1: Create registry doc and author
      const registryDoc = await dc.create();
      const authorId = await dc.createAuthor();

      // Step 2-10: For each service, build contract and publish
      for (const info of this.registry.getAllServices()) {
        // Build ServiceContract from service info
        const contract = fromServiceInfo(info);
        const contractId = contractIdFromContract(contract);

        // Build manifest with method field descriptors
        const manifest = this._buildManifest(info, contractId);
        const canonicalBytes = canonicalXlangBytes(contract);

        // Build collection and upload to blob store
        const { buildCollection: build } =
          await import('./contract/publication.js');
        const entries = build(manifest, canonicalBytes);
        const collectionHash = await bc.addCollection(
          entries.map(([name, data]) => [name, Buffer.from(data)] as [string, Buffer]),
        );
        const ticket = bc.createCollectionTicket(collectionHash);

        // Write ArtifactRef to registry doc
        const { contractKey, versionKey } = await import('./registry/keys.js');
        const artifactRef = {
          contract_id: contractId,
          collection_hash: collectionHash,
          ticket,
          published_by: authorId,
          published_at_epoch_ms: Date.now(),
          collection_format: 'index',
        };
        const encoder = new TextEncoder();
        await registryDoc.setBytes(
          authorId,
          contractKey(contractId),
          Buffer.from(encoder.encode(JSON.stringify(artifactRef))),
        );

        // Write manifest shortcut (avoids blob download round-trip)
        const { manifestToJson } = await import('./contract/manifest.js');
        await registryDoc.setBytes(
          authorId,
          `manifests/${contractId}`,
          Buffer.from(encoder.encode(manifestToJson(manifest))),
        );

        // Write version pointer
        await registryDoc.setBytes(
          authorId,
          versionKey(info.name, info.version),
          Buffer.from(encoder.encode(contractId)),
        );

        // Update service summary with contract ID
        const summary = this._serviceSummaries.find(s => s.name === info.name);
        if (summary) summary.contractId = contractId;

        this.logger.debug(
          `published contract ${contractId.slice(0, 12)} for ${info.name} v${info.version}`,
        );
      }

      // Share registry doc (read-only) and store namespace ID
      await registryDoc.shareWithAddr('read');
      this._registryNamespace = registryDoc.docId();
      this.logger.debug(`registry doc ready — namespace: ${this._registryNamespace.slice(0, 16)}`);

    } catch (err) {
      // Non-fatal: server still works without registry
      this.logger.warn(`contract publication failed (non-fatal): ${err}`);
    }
  }

  /**
   * Build a ContractManifest from a ServiceInfo with field-level detail.
   */
  private _buildManifest(info: any, contractId: string): ContractManifest {
    const WIRE_TYPE_KEY = Symbol.for('aster.wire_type');
    const methods: ManifestMethod[] = [];

    for (const [methodName, mi] of info.methods.entries()) {
      const patternStr =
        mi.pattern === 1 ? 'server_stream' :
        mi.pattern === 2 ? 'client_stream' :
        mi.pattern === 3 ? 'bidi_stream' : 'unary';

      // Extract field descriptors from @WireType request type
      const fields: ManifestField[] = [];
      const reqType = mi.requestType;
      if (reqType && typeof reqType === 'function') {
        try {
          const inst = new reqType();
          for (const key of Object.keys(inst)) {
            const val = inst[key];
            let fieldType = 'str';
            if (typeof val === 'number') fieldType = Number.isInteger(val) ? 'int' : 'float';
            else if (typeof val === 'boolean') fieldType = 'bool';
            else if (Array.isArray(val)) fieldType = 'list';
            else if (val && typeof val === 'object') fieldType = 'dict';
            fields.push({
              name: key,
              type: fieldType,
              required: val === '' || val === 0 || val === false,
            });
          }
        } catch { /* can't instantiate — skip field extraction */ }
      }

      const wireTag = reqType ? (reqType as any)[WIRE_TYPE_KEY] : undefined;

      methods.push({
        name: methodName,
        pattern: patternStr,
        requestType: wireTag ?? '',
        responseType: '',
        timeout: mi.timeout ?? 0,
        idempotent: mi.idempotent ?? false,
        fields,
      });
    }

    return {
      service: info.name,
      version: info.version,
      contractId,
      canonicalEncoding: 'fory-xlang/0.15',
      typeCount: 0,
      typeHashes: [],
      methodCount: methods.length,
      methods,
      serializationModes: ['xlang'],
      scoped: info.scoped === 'stream' ? 'stream' : 'shared',
      deprecated: false,
    };
  }

  /**
   * Start accepting connections. Blocks until close() is called.
   */
  async serve(): Promise<void> {
    if (!this._node || !this._rpcServer) {
      throw new Error('AsterServer.serve() called before start()');
    }

    // Install signal handlers for graceful shutdown
    this._installSignalHandlers();

    // Enable RPC server connection handling
    this._rpcServer.setServing(true);

    // Single accept loop with ALPN routing (matches Python's _accept_loop)
    this._servePromise = this._acceptLoop();
    try {
      await this._servePromise;
    } catch (e) {
      if (!this._closed) throw e;
    }
  }

  /**
   * Stop accepting connections and close the node.
   */
  async close(): Promise<void> {
    if (this._closed) return;
    this._closed = true;
    this._running = false;

    // Remove signal handlers
    for (const cleanup of this._signalHandlers) cleanup();
    this._signalHandlers = [];

    if (this._admissionAbort) {
      this._admissionAbort.abort();
    }
    if (this._rpcServer) {
      await this._rpcServer.close();
    }
    await this.health.stop();
    if (this._node) {
      try { await this._node.close(); } catch { /* ignore */ }
    }
    this.logger.info('AsterServer stopped');
  }

  /** The aster1... connection address for clients. */
  get address(): string {
    if (!this._node) throw new Error('Server not started');
    try {
      const native = loadNativeSync();
      if (native) {
        const info: any = {
          endpointId: this._node.nodeId(),
          relayAddr: undefined,
          directAddrs: [],
        };
        return native.asterTicketToString(info);
      }
    } catch { /* fallback */ }
    return this._node.nodeId();
  }

  /** Hex endpoint ID of this server's node. */
  get endpointId(): string {
    if (!this._node) throw new Error('Server not started');
    return this._node.nodeId();
  }

  /** Whether the server is running. */
  get running(): boolean { return this._running; }

  /** List of services hosted by this server. */
  get services(): ServiceSummary[] { return [...this._serviceSummaries]; }

  /** Create a local in-process transport for testing. */
  localTransport(): LocalTransport {
    return new LocalTransport(this.registry);
  }

  // ── Admission loop ──────────────────────────────────────────────────────

  /**
   * Single accept loop with ALPN-based routing.
   *
   * All aster ALPNs (RPC, consumer admission, delegated admission) are
   * multiplexed through one channel. This loop reads the ALPN tag and
   * dispatches to the correct handler — matching Python's `_accept_loop`.
   */
  private async _acceptLoop(): Promise<void> {
    if (!this._node) return;

    while (this._running && !this._closed) {
      try {
        // Accept next connection — ALPN tag is on the connection
        const conn = await this._node.acceptAster();
        const alpn: string = conn.alpn() ?? '';

        if (alpn === RPC_ALPN) {
          // RPC connection — dispatch to RpcServer
          this._rpcServer!.handleConnection(conn).catch(e => {
            this.logger.error('rpc connection error', { error: String(e) });
          });
        } else if (alpn === ALPN_CONSUMER_ADMISSION) {
          // Consumer admission
          this._handleAdmission(conn).catch(e => {
            this.logger.error('admission error', { error: String(e) });
          });
        } else if (alpn === ALPN_PRODUCER_ADMISSION) {
          // Producer-to-producer mesh admission
          this._handleProducerAdmission(conn).catch(e => {
            this.logger.error('producer admission error', { error: String(e) });
          });
        } else if (alpn === ALPN_DELEGATED_ADMISSION) {
          // Delegated admission
          this._handleDelegatedAdmission(conn).catch(e => {
            this.logger.error('delegated admission error', { error: String(e) });
          });
        } else {
          this.logger.warn(`unknown ALPN: ${alpn}`);
          try { conn.close(400, 'unknown ALPN'); } catch { /* ignore */ }
        }
      } catch (e) {
        if (this._closed) return;
        this.logger.error('accept error', { error: String(e) });
      }
    }
  }

  private async _handleAdmission(conn: any): Promise<void> {
    // Adapt NAPI connection to the interface handleConsumerAdmissionConnection expects
    const adapted = {
      acceptBi: () => conn.acceptBi(),
      remoteId: () => conn.remoteNodeId?.() ?? conn.remoteId?.() ?? 'unknown',
    };
    const opts: ConsumerAdmissionOpts = {
      services: this._serviceSummaries,
      registryNamespace: this._registryNamespace || undefined,
      allowUnenrolled: this._allowAllConsumers,
      logger: this.logger,
    };
    const rootKeyHex = this._resolveRootPubkeyHex();
    await handleConsumerAdmissionConnection(
      adapted,
      rootKeyHex,
      this._hook,
      opts,
    );
  }

  // ── Producer admission ───────────────────────────────────────────────────

  private async _handleProducerAdmission(conn: any): Promise<void> {
    // Producer admission requires root pubkey and mesh state.
    // If not configured (open mode), reject gracefully.
    if (!this.config.rootPubkey && !this.config.rootPubkeyFile) {
      this.logger.warn('producer admission: no root pubkey configured, ignoring');
      return;
    }
    const { handleProducerAdmissionConnection } = await import('./trust/bootstrap.js');
    const rootKeyHex = this._resolveRootPubkeyHex();
    const { MeshState } = await import('./trust/mesh.js');
    const meshState = new MeshState();
    await handleProducerAdmissionConnection(conn, rootKeyHex, meshState);
  }

  private _resolveRootPubkeyHex(): string {
    if (this.config.rootPubkey) return Buffer.from(this.config.rootPubkey).toString('hex');
    if (this.config.rootPubkeyFile) {
      try {
        const { readFileSync } = require('node:fs');
        const expanded = this.config.rootPubkeyFile.replace(/^~/, process.env.HOME ?? '');
        const raw = readFileSync(expanded, 'utf-8').trim();
        if (raw.startsWith('{')) {
          try { return JSON.parse(raw).public_key; } catch { /* fall through */ }
        }
        return raw;
      } catch { return ''; }
    }
    return '';
  }

  // ── Delegated admission ─────────────────────────────────────────────────

  private async _handleDelegatedAdmission(conn: any): Promise<void> {
    const policy = this._delegationPolicies.values().next().value;
    if (!policy) return;
    await handleDelegatedAdmissionConnection(
      conn,
      { policy, hook: this._hook, peerStore: this._peerStore },
    );
  }

  // ── Signal handling ─────────────────────────────────────────────────────

  private _installSignalHandlers(): void {
    if (typeof process === 'undefined') return;

    const shutdown = async () => {
      this.logger.info('Server shutting down...');
      await this.close();
    };

    const handler = () => { shutdown(); };
    process.on('SIGTERM', handler);
    process.on('SIGINT', handler);

    this._signalHandlers.push(
      () => { process.off('SIGTERM', handler); },
      () => { process.off('SIGINT', handler); },
    );
  }

  // ── Banner ──────────────────────────────────────────────────────────────

  private _printBanner(): void {
    if (typeof process !== 'undefined' && !process.stderr?.isTTY) return;

    const C = '\x1b[36m';
    const B = '\x1b[1m';
    const D = '\x1b[2m';
    const G = '\x1b[32m';
    const Y = '\x1b[33m';
    const W = '\x1b[37m';
    const R = '\x1b[0m';

    const w = (s: string) => process.stderr.write(s);

    w(`\n${C}${B}`);
    w(`        _    ____ _____ _____ ____\n`);
    w(`       / \\  / ___|_   _| ____|  _ \\\n`);
    w(`      / _ \\ \\___ \\ | | |  _| | |_) |\n`);
    w(`     / ___ \\ ___) || | | |___|  _ <\n`);
    w(`    /_/   \\_\\____/ |_| |_____|_| \\_\\\n`);
    w(`${R}\n`);
    w(`    ${D}RPC after hostnames.${R}\n\n`);

    // Services table
    if (this._serviceSummaries.length > 0) {
      const maxName = Math.max(...this._serviceSummaries.map(s => s.name.length));
      for (const s of this._serviceSummaries) {
        const name = s.name.padEnd(maxName);
        w(`    ${G}\u25cf${R} ${B}${name}${R}  ${D}v${s.version}${R}\n`);
      }
      w('\n');
    }

    // Endpoint
    try {
      w(`    ${D}endpoint:${R}  ${this.address}\n`);
    } catch { /* not started yet */ }

    // Mode
    const mode = this._allowAllConsumers ? `${Y}open-gate${R}` : `${G}trusted${R}`;
    w(`    ${D}mode:${R}      ${mode}\n`);

    // Log
    const logFormat = this.config.logFormat || 'text';
    const logLevel = this.config.logLevel || 'info';
    w(`    ${D}log:${R}       ASTER_LOG_FORMAT=${W}${logFormat}${R}  ASTER_LOG_LEVEL=${W}${logLevel}${R}\n`);

    // Runtime
    w(`    ${D}runtime:${R}   aster-rpc (typescript)  iroh 0.97\n`);

    // Copyright
    w(`\n    ${D}Copyright \u00a9 2026 Emrul Islam. All rights reserved.${R}\n\n`);
  }
}

// ── AsterClient ──────────────────────────────────────────────────────────────

/** Options for AsterClient. */
export interface AsterClientOptions {
  /** Connection address (aster1... ticket, base64 NodeAddr, or hex EndpointId). */
  address?: string;
  /** @deprecated Use `address` instead. */
  endpointAddr?: string;
  transport?: AsterTransport;
  config?: Partial<AsterConfig>;
  /** Path to .aster-identity file. Overrides config.identityFile. */
  identity?: string;
  /** Peer name for identity file lookup. */
  peer?: string;
  /** Retry configuration for reconnection. */
  retryBackoff?: ExponentialBackoff;
}

/**
 * High-level Aster RPC client.
 *
 * Wraps connection setup, admission, and client stub creation.
 * Supports reconnection with exponential backoff.
 */
export class AsterClientWrapper {
  private transport!: AsterTransport;
  readonly config: AsterConfig;
  private backoff: ExponentialBackoff;
  private _connected = false;
  private _gossipTopic = '';
  private _address: string | undefined;
  private _node: any = null;
  private _services: ServiceSummary[] = [];
  private _registryNamespace = '';

  constructor(opts: AsterClientOptions) {
    this.config = { ...configFromEnv(), ...opts.config } as AsterConfig;
    if (opts.identity) {
      this.config.identityFile = opts.identity;
    }
    this._address = opts.address ?? opts.endpointAddr as string | undefined;
    this.backoff = opts.retryBackoff ?? DEFAULT_BACKOFF;
    if (opts.transport) {
      this.transport = opts.transport;
      this._connected = true;
    }
  }

  /** Whether the client is connected. */
  get connected(): boolean {
    return this._connected;
  }

  /** Services discovered during admission. */
  get services(): ServiceSummary[] { return [...this._services]; }

  /** Registry namespace ID for service discovery (set after admission). */
  get registryNamespace(): string | undefined { return this._registryNamespace || undefined; }

  /** Hex-encoded 32-byte gossip topic ID for the producer mesh. */
  get gossipTopic(): string { return this._gossipTopic; }

  /**
   * Connect to the server via consumer admission, then open an RPC transport.
   *
   * If the client was created with a transport, this is a no-op.
   * If created with an address, it performs the full admission handshake.
   */
  async connect(): Promise<void> {
    if (this._connected) return;
    if (!this._address) {
      throw new Error(
        'AsterClient requires an address or transport. ' +
        'Pass address="aster1..." or transport=new IrohTransport(conn).',
      );
    }

    const native = await loadNative();
    if (!native) {
      throw new Error('Aster native addon not found.');
    }

    // Create an in-memory client node
    this._node = await native.IrohNode.memory();

    // Parse the address to get the endpoint ID and optional address hints.
    //
    // The ticket's `relayAddr` is a SocketAddr (STUN-discovered public IP),
    // NOT an iroh RelayUrl. Treat it as another direct addr — iroh will use
    // any reachable transport address it can.
    let endpointId: string;
    let allDirectAddrs: string[] = [];

    if (this._address.startsWith('aster1')) {
      let parsed: any;
      try {
        parsed = native.asterTicketFromString(this._address);
      } catch {
        parsed = native.asterTicketDecode(Buffer.from(this._address));
      }
      endpointId = parsed.endpointId;
      if (parsed.directAddrs?.length) allDirectAddrs.push(...parsed.directAddrs);
      if (parsed.relayAddr) allDirectAddrs.push(parsed.relayAddr);
    } else {
      // Treat as raw hex endpoint ID
      endpointId = this._address;
    }

    const directAddrs = allDirectAddrs.length ? allDirectAddrs : undefined;

    // Helper: connect using full address info when available, else bare endpoint ID
    const doConnect = (alpn: Buffer) => {
      if (directAddrs) {
        // No relay URL — iroh's connect_node_addr handles direct addresses
        return this._node.connectNodeAddr(endpointId, alpn, directAddrs, undefined);
      }
      return this._node.connect(endpointId, alpn);
    };

    // Consumer admission
    const admissionConn = await doConnect(Buffer.from(ALPN_CONSUMER_ADMISSION));
    const { performAdmission: doAdmission } = await import('./trust/consumer.js');
    // Pass null credential for open-gate mode (sends empty credential_json)
    const admissionResponse = await doAdmission(admissionConn, null as any);

    this._services = admissionResponse.services ?? [];
    this._registryNamespace = admissionResponse.registryNamespace ?? '';
    this._gossipTopic = admissionResponse.gossipTopic ?? '';

    // Open RPC connection and create transport
    const rpcConn = await doConnect(Buffer.from(RPC_ALPN));
    const { IrohTransport: IrohTx } = await import('./transport/iroh.js');
    this.transport = new IrohTx(rpcConn);
    this._connected = true;
  }

  /** Create a typed client proxy for a service class. */
  async client<T extends new (...args: any[]) => any>(serviceClass: T): Promise<ClientProxy<InstanceType<T>>> {
    return createClient(serviceClass, this.transport);
  }

  /** Create a typed client proxy for a service class. */
  service<T extends new (...args: any[]) => any>(serviceClass: T): ClientProxy<InstanceType<T>> {
    return createClient(serviceClass, this.transport);
  }

  /**
   * Create a dynamic proxy client for a service.
   *
   * @example
   * ```ts
   * const mc = client.proxy("MissionControl");
   * const result = await mc.getStatus({ agentId: "edge-1" });
   * console.log(result.status);
   * ```
   */
  proxy(serviceName: string): ProxyClient {
    return new ProxyClient(serviceName, this.transport);
  }

  /** Reconnect with exponential backoff. */
  async reconnect(
    connectFn: () => Promise<AsterTransport>,
    maxAttempts = 5,
  ): Promise<void> {
    let delay = this.backoff.initialMs;
    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      try {
        this.transport = await connectFn();
        this._connected = true;
        return;
      } catch (e) {
        if (attempt === maxAttempts) throw e;
        const jitter = delay * this.backoff.jitter * (Math.random() * 2 - 1);
        const waitMs = Math.min(delay + jitter, this.backoff.maxMs);
        await new Promise(r => setTimeout(r, waitMs));
        delay = Math.min(delay * this.backoff.multiplier, this.backoff.maxMs);
      }
    }
  }

  /** Close the client and underlying transport. */
  async close(): Promise<void> {
    this._connected = false;
    await this.transport.close();
  }
}

// ── ProxyClient ──────────────────────────────────────────────────────────────

/**
 * Dynamic proxy client -- invokes RPC methods without local type definitions.
 *
 * Created via `AsterClientWrapper.proxy("ServiceName")`. Supports all four
 * RPC patterns:
 *
 * ```ts
 * const mc = client.proxy("MissionControl");
 *
 * // Unary
 * const status = await mc.getStatus({ agent_id: "edge-7" });
 *
 * // Client streaming — pass an async iterable
 * const result = await mc.ingestMetrics(asyncGenerator());
 *
 * // Server streaming — use .stream()
 * for await (const entry of mc.tailLogs.stream({ level: "info" })) { ... }
 *
 * // Bidi streaming — use .bidi()
 * const ch = mc.runCommand.bidi();
 * await ch.open();
 * await ch.send({ command: "ls" });
 * for await (const r of ch) { ... }
 * ```
 */
export class ProxyClient {
  constructor(
    private readonly serviceName: string,
    private readonly transport: AsterTransport,
  ) {
    return new Proxy(this, {
      get(target, prop: string) {
        if (prop in target || typeof prop === 'symbol') {
          return (target as any)[prop];
        }
        return _proxyMethod(target.serviceName, prop, target.transport);
      },
    });
  }
}

/** A bound proxy method supporting all RPC patterns. */
function _proxyMethod(serviceName: string, methodName: string, transport: AsterTransport) {
  // The callable: detects async iterables for client streaming, else unary
  const fn = async (payload?: unknown) => {
    // Detect client streaming: payload is an async iterable (but not a plain object)
    if (payload != null && typeof payload === 'object' && Symbol.asyncIterator in payload) {
      return transport.clientStream(
        serviceName,
        methodName,
        payload as AsyncIterable<unknown>,
      );
    }
    // Default: unary
    return transport.unary(serviceName, methodName, payload ?? {});
  };

  // .stream() — server streaming
  fn.stream = (payload?: unknown): AsyncIterable<unknown> => {
    return transport.serverStream(serviceName, methodName, payload ?? {});
  };

  // .bidi() — bidirectional streaming
  fn.bidi = () => {
    return transport.bidiStream(serviceName, methodName);
  };

  return fn;
}

// ── Native addon loader ──────────────────────────────────────────────────────

let _native: any = null;

function loadNativeSync(): any {
  if (_native) return _native;

  const { resolve } = require('node:path');
  const { existsSync } = require('node:fs');

  const platforms = [
    'aster-transport.darwin-arm64.node',
    'aster-transport.darwin-x64.node',
    'aster-transport.linux-x64-gnu.node',
    'aster-transport.linux-arm64-gnu.node',
    'aster-transport.win32-x64-msvc.node',
  ];

  // 1. Try the workspace package
  try {
    _native = require('@aster-rpc/transport');
    return _native;
  } catch { /* next */ }

  // 2. Try ASTER_NATIVE_PATH env var
  const envPath = process.env.ASTER_NATIVE_PATH;
  if (envPath) {
    for (const name of platforms) {
      const full = resolve(envPath, name);
      if (existsSync(full)) { _native = require(full); return _native; }
    }
  }

  // 3. Try common workspace layouts relative to this file
  const searchDirs: string[] = [];
  try {
    const { dirname } = require('node:path');
    // When loaded from packages/aster/src/
    const thisDir = typeof __dirname !== 'undefined'
      ? __dirname
      : dirname(new URL(import.meta.url).pathname);
    searchDirs.push(resolve(thisDir, '../../../native'));    // packages/aster/src -> native
    searchDirs.push(resolve(thisDir, '../../native'));       // packages/aster -> native
    searchDirs.push(resolve(process.cwd(), 'bindings/typescript/native')); // repo root
    searchDirs.push(resolve(process.cwd(), 'node_modules/@aster-rpc/transport')); // linked
  } catch { /* ignore */ }

  for (const dir of searchDirs) {
    for (const name of platforms) {
      const full = resolve(dir, name);
      try {
        if (existsSync(full)) { _native = require(full); return _native; }
      } catch { /* next */ }
    }
  }

  return null;
}

async function loadNative(): Promise<any> {
  return loadNativeSync();
}
