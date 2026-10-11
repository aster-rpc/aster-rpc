import type { RawWtCancel, RawWtListenerHandle, RawWtSessionHandle, RawWtStreamHandle } from '@aster-rpc/transport';
import { loadNative } from './native.js';
import { Upcalls } from './upcalls.js';
import type { RpcAuthenticator } from './http-listener.js';

export interface WebTransportOptions {
  certificatePem: Uint8Array; privateKeyPem: Uint8Array;
  bindAddr?: string; path?: string;
  maxSessions?: number; maxStreams?: number; maxDatagrams?: number;
  maxDatagramBytes?: number; maxReadBytes?: number;
  signal?: AbortSignal;
}
export interface WebTransportOperation { signal?: AbortSignal; }
/** Values use JavaScript numbers; use statsJson() to preserve exact large counters. */
export interface WebTransportSessionInfo { connection_id: number; session_id: number; peer_address: string; closed: boolean; principal: string | null; attributes: Record<string, string>; }
export interface WebTransportStreamInfo { stream_id: number; has_send: boolean; has_receive: boolean; }
export interface WebTransportCloseInfo { kind: string; code: number | null; reason: string; }
export interface WebTransportStats {
  rtt_micros: number; cwnd: number; mtu: number; sent_packets: number; lost_packets: number;
  lost_bytes: number; congestion_events: number; udp_tx_bytes: number; udp_tx_packets: number;
  udp_rx_bytes: number; udp_rx_packets: number; ping_tx: number; ack_rx: number;
}
async function operation<T>(options: WebTransportOperation | undefined, work: (token: RawWtCancel) => Promise<T>): Promise<T> {
  options?.signal?.throwIfAborted();
  const token = new (loadNative().RawWtCancel)();
  const cancel = () => token.cancel();
  options?.signal?.addEventListener('abort', cancel, { once: true });
  try { return await work(token); }
  finally { options?.signal?.removeEventListener('abort', cancel); }
}
function uint32(value: number, name: string): number {
  if (!Number.isInteger(value) || value < 0 || value > 0xffffffff) throw new RangeError(`${name} must be a uint32`);
  return value;
}
function config(options: WebTransportOptions) {
  for (const key of ['maxSessions', 'maxStreams', 'maxDatagrams', 'maxDatagramBytes', 'maxReadBytes'] as const) {
    const value = options[key];
    if (value !== undefined && (uint32(value, key) === 0)) throw new RangeError(`${key} must be positive`);
  }
  return { ...options, certificatePem: Buffer.from(options.certificatePem), privateKeyPem: Buffer.from(options.privateKeyPem) };
}
/** Raw media endpoint, separate from framed RPC. Admission authenticates CONNECT before upgrade. */
export class WebTransportListener implements AsyncDisposable {
  private closing: Promise<void> | undefined;
  private constructor(private readonly native: RawWtListenerHandle, private readonly upcalls?: Upcalls, private readonly uninstall?: () => void) {}
  static async start(options: WebTransportOptions, authenticate: RpcAuthenticator): Promise<WebTransportListener> {
    options.signal?.throwIfAborted();
    if (typeof authenticate !== 'function') throw new TypeError('authenticator is required');
    const nativeOptions = config(options);
    const upcalls = new Upcalls();
    const uninstall = upcalls.serveRpcAuthenticator(authenticate);
    let listener: WebTransportListener | undefined;
    try {
      listener = new WebTransportListener(await loadNative().rawWtStart(nativeOptions, upcalls.native), upcalls, uninstall);
      options.signal?.throwIfAborted();
      return listener;
    } catch (error) {
      if (listener) await listener.close(); else { uninstall(); upcalls.close(); }
      throw error;
    }
  }
  static async startPublic(options: WebTransportOptions): Promise<WebTransportListener> {
    options.signal?.throwIfAborted();
    const listener = new WebTransportListener(await loadNative().rawWtStart(config(options)));
    try { options.signal?.throwIfAborted(); return listener; }
    catch (error) { await listener.close(); throw error; }
  }
  get tcpAddr(): string { return this.native.tcpAddr; }
  get quicAddr(): string { return this.native.quicAddr; }
  async accept(options?: WebTransportOperation): Promise<WebTransportSession> {
    return new WebTransportSession(await operation(options, token => this.native.accept(token)));
  }
  stop(graceMs = 0): Promise<void> {
    uint32(graceMs, 'graceMs');
    return this.closing ??= this.native.stop(graceMs).then(() => { this.uninstall?.(); this.upcalls?.close(); })
      .catch(error => { this.closing = undefined; throw error; });
  }
  close(): Promise<void> { return this.stop(); }
  [Symbol.asyncDispose](): Promise<void> { return this.close(); }
}
/** Owns a session. close() closes the entire underlying QUIC connection. */
export class WebTransportSession implements Disposable {
  /** @internal */ constructor(private readonly native: RawWtSessionHandle) {}
  info(): WebTransportSessionInfo { return JSON.parse(this.native.info()); }
  stats(): WebTransportStats { return JSON.parse(this.native.stats()); }
  /** Exact integer JSON, available without IEEE-754 conversion. */
  statsJson(): string { return this.native.stats(); }
  get maxDatagramSize(): number | null { return this.native.maxDatagramSize() ?? null; }
  sendDatagram(data: Uint8Array): void { this.native.sendDatagram(Buffer.from(data.buffer, data.byteOffset, data.byteLength)); }
  receiveDatagram(options?: WebTransportOperation): Promise<Uint8Array> { return operation(options, token => this.native.receiveDatagram(token)); }
  async acceptBi(options?: WebTransportOperation): Promise<WebTransportStream> { return new WebTransportStream(await operation(options, token => this.native.acceptBi(token))); }
  async acceptUni(options?: WebTransportOperation): Promise<WebTransportStream> { return new WebTransportStream(await operation(options, token => this.native.acceptUni(token))); }
  async openBi(options?: WebTransportOperation): Promise<WebTransportStream> { return new WebTransportStream(await operation(options, token => this.native.openBi(token))); }
  async openUni(options?: WebTransportOperation): Promise<WebTransportStream> { return new WebTransportStream(await operation(options, token => this.native.openUni(token))); }
  close(code = 0, reason = new Uint8Array()): void { this.native.close(uint32(code, 'code'), Buffer.from(reason)); }
  async closed(options?: WebTransportOperation): Promise<WebTransportCloseInfo> { return JSON.parse(await operation(options, token => this.native.closed(token))); }
  [Symbol.dispose](): void { this.close(); }
}
/** Keep alive after FIN until transportAcked or explicit reset. Releasing it aborts unfinished directions. */
export class WebTransportStream implements AsyncDisposable {
  /** @internal */ constructor(private readonly native: RawWtStreamHandle) {}
  info(): WebTransportStreamInfo { return JSON.parse(this.native.info()); }
  /** Empty bytes are EOF. Reads preserve unread data when cancelled. */
  read(maximum = 65536, options?: WebTransportOperation): Promise<Uint8Array> { return operation(options, token => this.native.read(uint32(maximum, 'maximum'), token)); }
  /** Cancellation after writing starts resets the send direction; a prefix may have reached the peer. */
  write(data: Uint8Array, options?: WebTransportOperation): Promise<void> { return operation(options, token => this.native.write(Buffer.from(data.buffer, data.byteOffset, data.byteLength), token)); }
  finish(options?: WebTransportOperation): Promise<void> { return operation(options, token => this.native.finish(token)); }
  reset(code = 0): Promise<void> { return this.native.reset(uint32(code, 'code')); }
  stop(code = 0): Promise<void> { return this.native.stop(uint32(code, 'code')); }
  setPriority(priority: number): Promise<void> {
    if (!Number.isInteger(priority) || priority < -2147483648 || priority > 2147483647) throw new RangeError('priority must be an int32');
    return this.native.setPriority(priority);
  }
  async priority(): Promise<number | null> { return await this.native.priority() ?? null; }
  /** Delivery to peer transport, not application receipt. False means reset/loss. */
  transportAcked(options?: WebTransportOperation): Promise<boolean> { return operation(options, token => this.native.transportAcked(token)); }
  async close(): Promise<void> { const info = this.info(); if (info.has_send) await this.reset(); if (info.has_receive) await this.stop(); }
  [Symbol.asyncDispose](): Promise<void> { return this.close(); }
}
