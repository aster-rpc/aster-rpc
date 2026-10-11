import type { HttpListener as NativeHttpListener, DispatchServer } from '@aster-rpc/transport';
import { Upcalls, type UpcallContext } from './upcalls.js';

/** Credentials and routing information for one HTTP/WebTransport RPC. */
export interface RpcAuthRequest {
  peerId: string;
  service: string;
  method: string;
  sessionId: number;
  metadata: Record<string, string>;
}

/** A nonempty principal and attributes local to this call. */
export interface RpcAuthIdentity {
  principal: string;
  attributes?: Record<string, string>;
}

/** Return null for UNAUTHENTICATED. Errors/timeouts fail closed with UNAVAILABLE. */
export type RpcAuthenticator = (
  request: RpcAuthRequest, context: UpcallContext,
) => RpcAuthIdentity | null | Promise<RpcAuthIdentity | null>;

export interface HttpListenerOptions {
  /** IP socket address; defaults to 127.0.0.1:0. */
  bindAddr?: string;
  certificatePem: Uint8Array;
  privateKeyPem: Uint8Array;
  /** Bind QUIC on the selected TCP port too. */
  webTransport?: boolean;
  /** Zero/absent selects 256 pending authentications / 64 KiB payloads. */
  maxPendingAuth?: number;
  maxAuthPayloadBytes?: number;
  /** Checked before and after native bind; cancellation joins cleanup. */
  signal?: AbortSignal;
}

/** Startup failed after binding and cleanup also failed. The listener remains owned for retry. */
export class HttpListenerStartError extends Error {
  constructor(
    readonly listener: HttpListener,
    cause: unknown,
    readonly cleanupError: unknown,
  ) {
    super('listener startup failed and cleanup failed; close error.listener to retry', { cause });
    this.name = 'HttpListenerStartError';
  }
}

/** Owns a listener and, for authenticated mode, its private callback table. */
export class HttpListener implements AsyncDisposable {
  private closing: Promise<void> | undefined;
  private constructor(
    private readonly native: NativeHttpListener,
    private readonly upcalls: Upcalls | undefined,
    private readonly uninstall: () => void,
    private onClosed?: (listener: HttpListener) => void,
  ) {}

  /** @internal */
  static async start(server: DispatchServer, options: HttpListenerOptions, authenticate: RpcAuthenticator, onClosed?: (listener: HttpListener) => void): Promise<HttpListener> {
    options.signal?.throwIfAborted();
    if (typeof authenticate !== 'function') throw new TypeError('an RPC authenticator is required');
    for (const [name, value] of Object.entries({ maxPendingAuth: options.maxPendingAuth, maxAuthPayloadBytes: options.maxAuthPayloadBytes })) {
      if (value !== undefined && (!Number.isInteger(value) || value < 0 || value > 0xffffffff)) {
        throw new RangeError(`${name} must be an integer between 0 and 4294967295`);
      }
    }
    const upcalls = new Upcalls();
    const uninstall = upcalls.serveRpcAuthenticator(authenticate);
    let listener: HttpListener | undefined;
    try {
      const native = await server.startHttp({
        ...options,
        certificatePem: Buffer.from(options.certificatePem),
        privateKeyPem: Buffer.from(options.privateKeyPem),
      }, upcalls.native);
      listener = new HttpListener(native, upcalls, uninstall, onClosed);
      options.signal?.throwIfAborted();
      return listener;
    } catch (error) {
      if (listener) {
        try { await listener.close(); }
        catch (cleanupError) { throw new HttpListenerStartError(listener, error, cleanupError); }
      }
      else { uninstall(); upcalls.close(); }
      throw error;
    }
  }

  /** @internal Explicit anonymous mode; no authentication callback table is allocated. */
  static async startPublic(server: DispatchServer, options: HttpListenerOptions, onClosed?: (listener: HttpListener) => void): Promise<HttpListener> {
    options.signal?.throwIfAborted();
    if ((options.maxPendingAuth ?? 0) !== 0 || (options.maxAuthPayloadBytes ?? 0) !== 0) {
      throw new RangeError('public listeners do not accept authentication limits');
    }
    let listener: HttpListener | undefined;
    try {
      const native = await server.startPublicHttp({ ...options,
        certificatePem: Buffer.from(options.certificatePem), privateKeyPem: Buffer.from(options.privateKeyPem) });
      listener = new HttpListener(native, undefined, () => {}, onClosed);
      options.signal?.throwIfAborted();
      return listener;
    } catch (error) {
      if (listener) {
        try { await listener.close(); }
        catch (cleanupError) { throw new HttpListenerStartError(listener, error, cleanupError); }
      }
      throw error;
    }
  }

  get tcpAddr(): string { return this.native.tcpAddr; }
  get quicAddr(): string | null { return this.native.quicAddr ?? null; }

  /** Application grace in ms. QUIC protocol/socket drain can extend beyond it. */
  stop(graceMs = 0): Promise<void> {
    if (!Number.isFinite(graceMs) || graceMs < 0 || graceMs > 0xffffffff) {
      return Promise.reject(new RangeError('graceMs must be between 0 and 4294967295'));
    }
    if (!this.closing) {
      this.closing = this.native.stop(graceMs).then(() => {
        this.uninstall();
        this.upcalls?.close();
        this.onClosed?.(this);
        this.onClosed = undefined;
      }).catch((error) => { this.closing = undefined; throw error; });
    }
    return this.closing;
  }
  close(): Promise<void> { return this.stop(0); }
  [Symbol.asyncDispose](): Promise<void> { return this.close(); }
}
