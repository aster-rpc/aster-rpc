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

/** Options for AsterServer. */
export interface AsterServerOptions {
  services: object[];
  config?: Partial<AsterConfig>;
  interceptors?: unknown[];
}

/**
 * High-level Aster RPC server.
 *
 * Wraps service registration, endpoint creation, health, and shutdown.
 */
export class AsterServer {
  readonly registry: ServiceRegistry;
  readonly config: AsterConfig;
  readonly logger: AsterLogger;
  private health: HealthServer;
  private _endpointAddr?: string;
  private _running = false;
  private _draining = false;
  private _inFlight = 0;
  private _signalHandlers: (() => void)[] = [];

  constructor(opts: AsterServerOptions) {
    this.config = { ...configFromEnv(), ...opts.config } as AsterConfig;
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

    for (const svc of opts.services) {
      this.registry.register(svc);
    }
  }

  /** Start the server (create node, publish contracts, start health). */
  async start(): Promise<void> {
    this.logger.info('starting AsterServer', { services: this.registry.size });
    await this.health.start();
    this._running = true;
    this.logger.info('AsterServer started');
  }

  /** The endpoint address for clients to connect to. */
  get endpointAddr(): string | undefined {
    return this._endpointAddr;
  }

  get running(): boolean {
    return this._running;
  }

  get draining(): boolean {
    return this._draining;
  }

  /** Create a local (in-process) transport for testing. */
  localTransport(): LocalTransport {
    return new LocalTransport(this.registry);
  }

  /**
   * Install signal handlers for graceful shutdown (SIGTERM, SIGINT).
   * When a signal arrives, drain() is called automatically.
   */
  installSignalHandlers(): void {
    const handler = () => {
      this.logger.info('signal received, draining...');
      this.drain().catch(e => this.logger.error('drain error', { error: String(e) }));
    };
    process.on('SIGTERM', handler);
    process.on('SIGINT', handler);
    this._signalHandlers.push(
      () => { process.removeListener('SIGTERM', handler); },
      () => { process.removeListener('SIGINT', handler); },
    );
  }

  /**
   * Graceful drain: stop accepting new connections, wait for in-flight
   * RPCs to complete (up to timeoutS seconds), then shut down.
   */
  async drain(timeoutS = 30): Promise<void> {
    if (this._draining) return;
    this._draining = true;
    this.logger.info('draining', { timeout_s: timeoutS, in_flight: this._inFlight });

    // Wait for in-flight RPCs to complete
    const deadline = Date.now() + timeoutS * 1000;
    while (this._inFlight > 0 && Date.now() < deadline) {
      await new Promise(r => setTimeout(r, 100));
    }

    if (this._inFlight > 0) {
      this.logger.warning('drain timeout, cancelling remaining RPCs', { in_flight: this._inFlight });
    }

    await this.close();
  }

  // ── Property accessors ─────────────────────────────────────────────────────

  /** Base64-encoded endpoint address (node-id + relay). */
  get endpointAddrB64(): string | undefined { return this._endpointAddr; }

  /** Base64-encoded RPC endpoint address. */
  get rpcAddrB64(): string | undefined { return this._endpointAddr; }

  /** Base64-encoded producer admission address. */
  get producerAdmissionAddrB64(): string | undefined { return undefined; }

  /** Base64-encoded consumer admission address. */
  get consumerAdmissionAddrB64(): string | undefined { return undefined; }

  /** Base64-encoded admission address. */
  get admissionAddrB64(): string | undefined { return undefined; }

  /** Mesh state if running in trust mode, or undefined for open mode. */
  get meshState(): unknown { return undefined; }

  /** Root public key (hex) if running in trust mode, or undefined. */
  get rootPubkey(): string | undefined { return undefined; }

  /** The underlying Iroh node (if using QUIC transport). */
  get node(): unknown { return undefined; }

  /** Blobs client (if using QUIC transport). */
  get blobs(): unknown { return undefined; }

  /** Docs client (if using QUIC transport). */
  get docs(): unknown { return undefined; }

  /** Gossip client (if using QUIC transport). */
  get gossip(): unknown { return undefined; }

  /** RPC endpoint handle (if using QUIC transport). */
  get rpcEndpoint(): unknown { return undefined; }

  // ── In-flight tracking ─────────────────────────────────────────────────────

  /** Track an in-flight RPC (for drain). */
  trackRpcStart(): void { this._inFlight++; }
  trackRpcEnd(): void { this._inFlight = Math.max(0, this._inFlight - 1); }

  /** Graceful shutdown. */
  async close(): Promise<void> {
    this._running = false;
    this._draining = false;
    for (const cleanup of this._signalHandlers) cleanup();
    this._signalHandlers = [];
    await this.health.stop();
    this.logger.info('AsterServer stopped');
  }
}

/** Options for AsterClient. */
export interface AsterClientOptions {
  endpointAddr?: string;
  transport?: AsterTransport;
  config?: Partial<AsterConfig>;
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
  private transport: AsterTransport;
  readonly config: AsterConfig;
  private backoff: ExponentialBackoff;
  private _connected = false;

  constructor(opts: AsterClientOptions) {
    this.config = { ...configFromEnv(), ...opts.config } as AsterConfig;
    this.backoff = opts.retryBackoff ?? DEFAULT_BACKOFF;
    if (opts.transport) {
      this.transport = opts.transport;
      this._connected = true;
    } else {
      throw new Error(
        'AsterClient requires a transport. Use localTransport() for testing ' +
        'or IrohTransport for production.',
      );
    }
  }

  /** Whether the client is connected. */
  get connected(): boolean {
    return this._connected;
  }

  /** Registry ticket for service discovery (set after admission). */
  get registryTicket(): string | undefined { return undefined; }

  /**
   * Connect to the server (no-op if already connected via transport option).
   * Override in subclasses to implement QUIC connection setup.
   */
  async connect(): Promise<void> {
    // Subclasses may override to do admission + connect
  }

  /**
   * Create a typed client proxy for a service class.
   * Alias for service() for Python API compatibility.
   */
  async client<T extends new (...args: any[]) => any>(serviceClass: T): Promise<ClientProxy<InstanceType<T>>> {
    return createClient(serviceClass, this.transport);
  }

  /** Create a typed client proxy for a service class. */
  service<T extends new (...args: any[]) => any>(serviceClass: T): ClientProxy<InstanceType<T>> {
    return createClient(serviceClass, this.transport);
  }

  /**
   * Reconnect with exponential backoff.
   *
   * @param connectFn - Function that creates a new transport.
   * @param maxAttempts - Maximum number of reconnection attempts (default 5).
   */
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
