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

// ── Constants ────────────────────────────────────────────────────────────────

const ALPN_CONSUMER_ADMISSION = 'aster.consumer_admission';
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
  private _signalHandlers: (() => void)[] = [];
  private _serviceSummaries: ServiceSummary[] = [];
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

    // Create node with RPC + admission ALPNs
    const alpns = [
      Buffer.from(RPC_ALPN),
      Buffer.from(ALPN_CONSUMER_ADMISSION),
      Buffer.from(ALPN_DELEGATED_ADMISSION),
    ];
    this._node = await native.IrohNode.memoryWithAlpns(alpns);

    // Build service summaries for admission response
    this._serviceSummaries = [];
    for (const info of this.registry.getAllServices()) {
      this._serviceSummaries.push({
        name: info.name,
        version: info.version,
        contractId: '',
        pattern: info.scoped ?? 'shared',
        methods: Object.keys(info.methods),
      });
    }

    // Create the RPC server (uses JsonCodec for cross-language compat)
    this._rpcServer = new RpcServer({
      registry: this.registry,
      codec: new JsonCodec(),
      logger: this.logger,
    });

    await this.health.start();
    this._running = true;
    this._printBanner();
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

    // Start admission handler in background
    this._admissionAbort = new AbortController();
    const admissionPromise = this._admissionLoop();

    // Start delegated admission loop if policies exist
    const delegatedPromise = this._delegationPolicies.size > 0
      ? this._delegatedAdmissionLoop()
      : Promise.resolve();

    // Start RPC server
    const rpcPromise = this._rpcServer.serve(this._node);

    this._servePromise = Promise.all([admissionPromise, delegatedPromise, rpcPromise]).then(() => {});
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

  private async _admissionLoop(): Promise<void> {
    if (!this._node) return;

    while (this._running && !this._closed) {
      try {
        // Accept connections on the consumer admission ALPN
        const conn = await this._node.acceptAster();
        this._handleAdmission(conn).catch(e => {
          this.logger.error('admission error', { error: String(e) });
        });
      } catch (e) {
        if (this._closed) return;
        this.logger.error('admission accept error', { error: String(e) });
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
      allowUnenrolled: this._allowAllConsumers,
      logger: this.logger,
    };
    // Resolve root pubkey from config (either raw bytes or from file)
    const rootKeyHex = (() => {
      if (this.config.rootPubkey) return Buffer.from(this.config.rootPubkey).toString('hex');
      if (this.config.rootPubkeyFile) {
        try {
          const { readFileSync } = require('node:fs');
          const expanded = this.config.rootPubkeyFile.replace(/^~/, process.env.HOME ?? '');
          const hex = readFileSync(expanded, 'utf-8').trim();
          return hex;
        } catch (e: any) { this.logger.warn(`failed to read rootPubkeyFile: ${e.message}`); return ''; }
      }
      return '';
    })();
    await handleConsumerAdmissionConnection(
      adapted,
      rootKeyHex,
      this._hook,
      opts,
    );
  }

  // ── Delegated admission loop ─────────────────────────────────────────────

  private async _delegatedAdmissionLoop(): Promise<void> {
    if (!this._node) return;

    while (this._running && !this._closed) {
      try {
        const conn = await this._node.acceptAster();
        const policy = this._delegationPolicies.values().next().value;
        if (!policy) continue;
        handleDelegatedAdmissionConnection(
          conn,
          { policy, hook: this._hook, peerStore: this._peerStore },
        ).catch(e => {
          this.logger.error('delegated admission error', { error: String(e) });
        });
      } catch (e) {
        if (this._closed) return;
        this.logger.error('delegated admission accept error', { error: String(e) });
      }
    }
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

    // Parse the address to get the endpoint ID
    let endpointId: string;
    if (this._address.startsWith('aster1')) {
      try {
        const parsed = native.asterTicketFromString(this._address);
        endpointId = parsed.endpointId;
      } catch {
        // Ticket contains only nodeId -- decode it
        const decoded = native.asterTicketDecode(Buffer.from(this._address));
        endpointId = decoded.endpointId;
      }
    } else {
      // Treat as raw hex endpoint ID
      endpointId = this._address;
    }

    // Consumer admission
    const admissionConn = await this._node.connect(endpointId, Buffer.from(ALPN_CONSUMER_ADMISSION));
    const { performAdmission: doAdmission } = await import('./trust/consumer.js');
    const admissionResponse = await doAdmission(admissionConn, {} as any);

    this._services = admissionResponse.services ?? [];
    this._registryNamespace = admissionResponse.registryNamespace ?? '';
    this._gossipTopic = admissionResponse.gossipTopic ?? '';

    // Open RPC connection and create transport
    const rpcConn = await this._node.connect(endpointId, Buffer.from(RPC_ALPN));
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
 * Created via `AsterClientWrapper.proxy("ServiceName")`. Methods are
 * dispatched dynamically -- any method name called on the proxy becomes
 * a unary RPC call.
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
        return async (payload?: unknown) => {
          const result = await target.transport.unary(
            target.serviceName,
            prop,
            payload ?? {},
          );
          return result;
        };
      },
    });
  }
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
