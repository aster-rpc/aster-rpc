/**
 * IrohTransport2 — multiplexed-streams transport (spec §5/§6).
 *
 * Every RPC goes through the per-connection multiplexed-stream pool via
 * the napi `AsterCall` handle. The call handle owns a pooled bi-stream
 * for its lifetime and releases it back to the pool on success (or
 * discards it on error). Streams are never finished per-call; the server
 * reads frames in a loop, so every request's last frame carries
 * `END_STREAM` to tell the dispatcher the request phase is done.
 *
 * `sessionId` selects the pool routing key:
 *   0         = SHARED pool (stateless).
 *   non-zero  = per-session pool. Allocated by `AsterClient2.openSession`
 *               as a monotonic u32 per (peer, rpcAddr).
 *
 * This module mirrors `bindings/python/aster/transport/iroh.py` post-
 * multiplexed-streams migration (commits 30819f3, 25da4e8, f639a31).
 */

import type { Codec } from '../codec.js';
import { JsonCodec } from '../codec.js';
import {
  encodeFrame,
  COMPRESSED,
  END_STREAM,
  HEADER,
  ROW_SCHEMA,
  TRAILER,
} from '../framing.js';
import { StreamHeader, RpcStatus } from '../protocol.js';
import { StatusCode, RpcError } from '../status.js';
import type { AsterTransport, CallOptions, BidiChannel } from './base.js';

// -- Native-shape structural interfaces ---------------------------------------
//
// The native package's `index.d.ts` ships empty — every TS consumer
// defines structural interfaces against the NAPI object shapes it uses.
// These mirror `bindings/typescript/native/src/{net,call}.rs`.

/** Structural shape of the napi `IrohConnection`. */
export interface NativeConnection {
  readonly __iroh_connection?: never; // brand
  close(code: number, reason: string): void;
}

/** Recv-frame result from `AsterCall.recvFrame`. */
export interface AsterCallRecvResult {
  payload: Uint8Array;
  flags: number;
  kind: number; // 0 = OK, 1 = END_OF_STREAM, 2 = TIMEOUT
}

/** Structural shape of the napi `AsterCall` handle. */
export interface NativeAsterCall {
  sendFrame(frameBytes: Uint8Array): Promise<void>;
  recvFrame(timeoutMs?: number): Promise<AsterCallRecvResult>;
  release(): void;
  discard(): void;
}

/** Static factory for `AsterCall` (injected so the transport can be
 *  exercised against a fake in tests). In production, pass the class
 *  imported from `@aster-rpc/transport`. */
export interface AsterCallFactory {
  acquire(conn: NativeConnection, sessionId: number): Promise<NativeAsterCall>;
}

const RECV_OK = 0;
const RECV_END_OF_STREAM = 1;

// -- Error mapping ------------------------------------------------------------

/** Typed error for `StreamAcquireError:*` native failures. Native throws
 *  a message-prefixed string; this wrapper recognises the prefix and
 *  surfaces the reason as a first-class field. */
export class StreamAcquireError extends Error {
  readonly reason: string;
  constructor(reason: string, message: string) {
    super(message);
    this.name = 'StreamAcquireError';
    this.reason = reason;
  }

  /** Parse an error thrown by native `AsterCall.acquire`; returns `null`
   *  if the error isn't a StreamAcquire one so the caller can rethrow. */
  static tryFrom(err: unknown): StreamAcquireError | null {
    if (!(err instanceof Error)) return null;
    const prefix = 'StreamAcquireError:';
    if (!err.message.startsWith(prefix)) return null;
    const rest = err.message.slice(prefix.length);
    const sep = rest.indexOf(':');
    if (sep < 0) return new StreamAcquireError('UNKNOWN', err.message);
    const reason = rest.slice(0, sep);
    const message = rest.slice(sep + 1).trimStart();
    return new StreamAcquireError(reason, message);
  }
}

function acquireErrorToRpcError(err: StreamAcquireError): RpcError {
  // Pool exhaustion / transport failures all map to UNAVAILABLE so
  // callers see a retriable error (spec §6.7).
  return new RpcError(
    StatusCode.UNAVAILABLE,
    `stream acquire failed: ${err.reason}: ${err.message}`,
  );
}

const CONNECTION_LOST_KEYWORDS = [
  'reset',
  'connection closed',
  'connection lost',
  'stream closed',
];

function mapTransportError(err: unknown): RpcError {
  if (err instanceof RpcError) return err;
  const acquire = StreamAcquireError.tryFrom(err);
  if (acquire) return acquireErrorToRpcError(acquire);
  if (err instanceof Error) {
    const msg = err.message.toLowerCase();
    if (CONNECTION_LOST_KEYWORDS.some((kw) => msg.includes(kw))) {
      return new RpcError(StatusCode.UNAVAILABLE, `stream reset: ${err.message}`);
    }
    return new RpcError(StatusCode.UNKNOWN, err.message);
  }
  return new RpcError(StatusCode.UNKNOWN, String(err));
}

// -- Metadata helper ----------------------------------------------------------

function buildMetadata(metadata?: Record<string, string>): [string[], string[]] {
  if (!metadata) return [[], []];
  const keys = Object.keys(metadata);
  if (keys.length === 0) return [[], []];
  return [keys, keys.map((k) => metadata[k]!)];
}

// -- CallDriver (per-call helper) --------------------------------------------

/** Pair with `release()` on success or `discard()` on error. The handle
 *  is valid only between `acquire()` and one of those terminators. */
class CallDriver {
  private terminated = false;

  constructor(
    private readonly call: NativeAsterCall,
    private readonly codec: Codec,
  ) {}

  static async acquire(
    factory: AsterCallFactory,
    conn: NativeConnection,
    sessionId: number,
    codec: Codec,
  ): Promise<CallDriver> {
    let call: NativeAsterCall;
    try {
      call = await factory.acquire(conn, sessionId);
    } catch (e) {
      const acquireErr = StreamAcquireError.tryFrom(e);
      if (acquireErr) throw acquireErrorToRpcError(acquireErr);
      throw e;
    }
    return new CallDriver(call, codec);
  }

  async sendHeader(params: {
    service: string;
    method: string;
    deadlineSecs: number;
    serializationMode: number;
    metadata?: Record<string, string>;
    sessionId: number;
  }): Promise<void> {
    const [keys, values] = buildMetadata(params.metadata);
    const header = new StreamHeader({
      service: params.service,
      method: params.method,
      version: 1,
      callId: 0,
      deadline: params.deadlineSecs,
      serializationMode: params.serializationMode,
      metadataKeys: keys,
      metadataValues: values,
      sessionId: params.sessionId,
    });
    const headerBytes = this.codec.encode(header);
    await this.call.sendFrame(encodeFrame(headerBytes, HEADER));
  }

  async sendRequest(request: unknown, last: boolean): Promise<void> {
    const [payload, compressed] = this.codec.encodeCompressed(request);
    let flags = 0;
    if (compressed) flags |= COMPRESSED;
    if (last) flags |= END_STREAM;
    await this.call.sendFrame(encodeFrame(payload, flags));
  }

  /** Explicit empty `END_STREAM` — used by bidi `close()` when requests
   *  were sent without an inline `last` flag. */
  async sendEndStream(): Promise<void> {
    await this.call.sendFrame(encodeFrame(new Uint8Array(0), END_STREAM));
  }

  /** Pull one frame. Returns `null` on end-of-stream. */
  async recvFrame(): Promise<[payload: Uint8Array, flags: number] | null> {
    const result = await this.call.recvFrame(0);
    if (result.kind === RECV_OK) return [result.payload, result.flags];
    if (result.kind === RECV_END_OF_STREAM) return null;
    // timeoutMs=0 means block indefinitely — reaching here is a bug.
    throw new RpcError(
      StatusCode.UNKNOWN,
      'unexpected recv timeout on multiplexed stream',
    );
  }

  decodeResponse(payload: Uint8Array, flags: number): unknown {
    const compressed = (flags & COMPRESSED) !== 0;
    return this.codec.decodeCompressed(payload, compressed);
  }

  /** Parse a trailer payload and throw `RpcError` if non-OK. Empty
   *  payload = clean OK trailer (core strips empty END_STREAM forwards
   *  on the server side — see Python commit `cdabc02`). */
  checkTrailer(payload: Uint8Array): void {
    if (payload.byteLength === 0) return;
    const status = this.codec.decode(payload, RpcStatus) as RpcStatus;
    if (status.code !== StatusCode.OK) {
      throw RpcError.fromStatus(
        status.code as StatusCode,
        status.message,
        status.details,
      );
    }
  }

  release(): void {
    if (!this.terminated) {
      this.terminated = true;
      this.call.release();
    }
  }

  discard(): void {
    if (!this.terminated) {
      this.terminated = true;
      this.call.discard();
    }
  }
}

// -- Transport ----------------------------------------------------------------

/** Options for constructing an [`IrohTransport2`]. */
export interface IrohTransport2Options {
  connection: NativeConnection;
  asterCall: AsterCallFactory;
  codec?: Codec;
  /** Pool routing key. 0 = SHARED; non-zero = per-session pool. */
  sessionId?: number;
}

/**
 * Multiplexed-streams transport (spec §5/§6). Port of the post-migration
 * Python `IrohTransport`. The v1 `IrohTransport` stays alongside for the
 * migration window; once step 6 lands, existing tests migrate to this
 * class and v1 is deleted in Session 3.
 */
export class IrohTransport2 implements AsterTransport {
  private readonly conn: NativeConnection;
  private readonly factory: AsterCallFactory;
  private readonly codec: Codec;
  private readonly sessionIdValue: number;
  private readonly defaultSerializationMode: number;

  constructor(opts: IrohTransport2Options) {
    this.conn = opts.connection;
    this.factory = opts.asterCall;
    this.codec = opts.codec ?? new JsonCodec();
    this.sessionIdValue = opts.sessionId ?? 0;
    // Auto-pick the default mode from the codec so JSON-only producers
    // don't need to pass `serializationMode: 3` at every call site.
    // Matches Python `IrohTransport._default_serialization_mode`.
    this.defaultSerializationMode = this.codec instanceof JsonCodec ? 3 : 0;
  }

  get sessionId(): number {
    return this.sessionIdValue;
  }

  private resolveMode(override: number | undefined): number {
    return override ?? this.defaultSerializationMode;
  }

  async close(): Promise<void> {
    this.conn.close(0, 'normal close');
  }

  // ── Unary ────────────────────────────────────────────────────────────────

  async unary(
    service: string,
    method: string,
    request: unknown,
    opts?: CallOptions,
  ): Promise<unknown> {
    const driver = await CallDriver.acquire(
      this.factory,
      this.conn,
      this.sessionIdValue,
      this.codec,
    );
    try {
      await driver.sendHeader({
        service,
        method,
        deadlineSecs: opts?.deadlineSecs ?? 0,
        serializationMode: this.resolveMode(opts?.serializationMode),
        metadata: opts?.metadata,
        sessionId: this.sessionIdValue,
      });
      await driver.sendRequest(request, true);

      let responsePayload: Uint8Array | null = null;
      let responseFlags = 0;
      while (true) {
        const frame = await driver.recvFrame();
        if (frame === null) {
          throw new RpcError(StatusCode.UNAVAILABLE, 'stream ended before trailer');
        }
        const [payload, flags] = frame;
        if (flags & TRAILER) {
          driver.checkTrailer(payload);
          if (responsePayload === null) {
            throw new RpcError(
              StatusCode.INTERNAL,
              'unary call received OK trailer with no response frame',
            );
          }
          driver.release();
          return driver.decodeResponse(responsePayload, responseFlags);
        }
        if (flags & ROW_SCHEMA) continue;
        if (responsePayload !== null) {
          throw new RpcError(
            StatusCode.UNKNOWN,
            'unary call received multiple response frames',
          );
        }
        responsePayload = payload;
        responseFlags = flags;
      }
    } catch (e) {
      driver.discard();
      throw mapTransportError(e);
    }
  }

  // ── Server streaming ─────────────────────────────────────────────────────

  async *serverStream(
    service: string,
    method: string,
    request: unknown,
    opts?: CallOptions,
  ): AsyncIterable<unknown> {
    const driver = await CallDriver.acquire(
      this.factory,
      this.conn,
      this.sessionIdValue,
      this.codec,
    );
    let released = false;
    try {
      await driver.sendHeader({
        service,
        method,
        deadlineSecs: opts?.deadlineSecs ?? 0,
        serializationMode: this.resolveMode(opts?.serializationMode),
        metadata: opts?.metadata,
        sessionId: this.sessionIdValue,
      });
      await driver.sendRequest(request, true);

      while (true) {
        const frame = await driver.recvFrame();
        if (frame === null) {
          throw new RpcError(StatusCode.UNAVAILABLE, 'stream ended before trailer');
        }
        const [payload, flags] = frame;
        if (flags & TRAILER) {
          driver.checkTrailer(payload);
          driver.release();
          released = true;
          return;
        }
        if (flags & ROW_SCHEMA) continue;
        yield driver.decodeResponse(payload, flags);
      }
    } catch (e) {
      if (!released) driver.discard();
      throw mapTransportError(e);
    } finally {
      if (!released) driver.discard();
    }
  }

  // ── Client streaming ─────────────────────────────────────────────────────

  async clientStream(
    service: string,
    method: string,
    requests: AsyncIterable<unknown>,
    opts?: CallOptions,
  ): Promise<unknown> {
    const driver = await CallDriver.acquire(
      this.factory,
      this.conn,
      this.sessionIdValue,
      this.codec,
    );
    try {
      await driver.sendHeader({
        service,
        method,
        deadlineSecs: opts?.deadlineSecs ?? 0,
        serializationMode: this.resolveMode(opts?.serializationMode),
        metadata: opts?.metadata,
        sessionId: this.sessionIdValue,
      });

      // Buffer all requests so the last one can carry END_STREAM inline.
      // Matches the Python implementation.
      const buffered: unknown[] = [];
      for await (const req of requests) buffered.push(req);
      if (buffered.length === 0) {
        throw new RpcError(
          StatusCode.INVALID_ARGUMENT,
          'clientStream requires at least one request frame',
        );
      }
      for (let i = 0; i < buffered.length; i++) {
        await driver.sendRequest(buffered[i], i === buffered.length - 1);
      }

      let responsePayload: Uint8Array | null = null;
      let responseFlags = 0;
      while (true) {
        const frame = await driver.recvFrame();
        if (frame === null) {
          throw new RpcError(StatusCode.UNAVAILABLE, 'stream ended before trailer');
        }
        const [payload, flags] = frame;
        if (flags & TRAILER) {
          driver.checkTrailer(payload);
          if (responsePayload === null) {
            throw new RpcError(
              StatusCode.INTERNAL,
              'clientStream got OK trailer with no response frame',
            );
          }
          driver.release();
          return driver.decodeResponse(responsePayload, responseFlags);
        }
        if (flags & ROW_SCHEMA) continue;
        if (responsePayload !== null) {
          throw new RpcError(
            StatusCode.UNKNOWN,
            'clientStream received multiple response frames',
          );
        }
        responsePayload = payload;
        responseFlags = flags;
      }
    } catch (e) {
      driver.discard();
      throw mapTransportError(e);
    }
  }

  // ── Bidi streaming ───────────────────────────────────────────────────────

  bidiStream(service: string, method: string, opts?: CallOptions): BidiChannel {
    return new IrohBidiChannel2({
      factory: this.factory,
      conn: this.conn,
      codec: this.codec,
      service,
      method,
      metadata: opts?.metadata,
      deadlineSecs: opts?.deadlineSecs ?? 0,
      serializationMode: this.resolveMode(opts?.serializationMode),
      sessionId: this.sessionIdValue,
    });
  }
}

// -- Bidi channel -------------------------------------------------------------

interface BidiChannelOptions {
  factory: AsterCallFactory;
  conn: NativeConnection;
  codec: Codec;
  service: string;
  method: string;
  metadata?: Record<string, string>;
  deadlineSecs: number;
  serializationMode: number;
  sessionId: number;
}

class IrohBidiChannel2 implements BidiChannel {
  private driver: CallDriver | null = null;
  private sentEndStream = false;
  private lastTrailer: [StatusCode, string] | null = null;
  // Lazy driver bootstrap guarded by a single in-flight promise so
  // concurrent send()/recv() callers don't race on acquire + sendHeader.
  private acquirePromise: Promise<CallDriver> | null = null;

  constructor(private readonly opts: BidiChannelOptions) {}

  private async ensureDriver(): Promise<CallDriver> {
    if (this.driver !== null) return this.driver;
    if (this.acquirePromise !== null) return this.acquirePromise;

    this.acquirePromise = (async () => {
      const driver = await CallDriver.acquire(
        this.opts.factory,
        this.opts.conn,
        this.opts.sessionId,
        this.opts.codec,
      );
      try {
        await driver.sendHeader({
          service: this.opts.service,
          method: this.opts.method,
          deadlineSecs: this.opts.deadlineSecs,
          serializationMode: this.opts.serializationMode,
          metadata: this.opts.metadata,
          sessionId: this.opts.sessionId,
        });
      } catch (e) {
        driver.discard();
        this.acquirePromise = null;
        throw e;
      }
      this.driver = driver;
      return driver;
    })();

    return this.acquirePromise;
  }

  async send(msg: unknown): Promise<void> {
    if (this.sentEndStream) {
      throw new RpcError(StatusCode.FAILED_PRECONDITION, 'channel is closed for sending');
    }
    const driver = await this.ensureDriver();
    try {
      await driver.sendRequest(msg, false);
    } catch (e) {
      driver.discard();
      this.driver = null;
      throw mapTransportError(e);
    }
  }

  async *[Symbol.asyncIterator](): AsyncIterator<unknown> {
    const driver = await this.ensureDriver();
    while (true) {
      let frame: [Uint8Array, number] | null;
      try {
        frame = await driver.recvFrame();
      } catch (e) {
        driver.discard();
        this.driver = null;
        throw mapTransportError(e);
      }
      if (frame === null) {
        throw new RpcError(StatusCode.UNAVAILABLE, 'stream ended');
      }
      const [payload, flags] = frame;
      if (flags & TRAILER) {
        if (payload.byteLength === 0) {
          this.lastTrailer = [StatusCode.OK, ''];
          driver.release();
          this.driver = null;
          return;
        }
        const status = this.opts.codec.decode(payload, RpcStatus) as RpcStatus;
        this.lastTrailer = [status.code as StatusCode, status.message];
        if (status.code !== StatusCode.OK) {
          driver.discard();
          this.driver = null;
          throw RpcError.fromStatus(
            status.code as StatusCode,
            status.message,
            status.details,
          );
        }
        driver.release();
        this.driver = null;
        return;
      }
      if (flags & ROW_SCHEMA) continue;
      yield driver.decodeResponse(payload, flags);
    }
  }

  async close(): Promise<void> {
    if (this.sentEndStream || this.driver === null) {
      this.sentEndStream = true;
      return;
    }
    this.sentEndStream = true;
    try {
      await this.driver.sendEndStream();
    } catch {
      // Best-effort close: if the stream is already dead, that's fine.
    }
  }

  async waitForTrailer(): Promise<[StatusCode, string]> {
    if (this.lastTrailer !== null) return this.lastTrailer;
    const driver = await this.ensureDriver();
    while (true) {
      let frame: [Uint8Array, number] | null;
      try {
        frame = await driver.recvFrame();
      } catch (e) {
        driver.discard();
        this.driver = null;
        throw mapTransportError(e);
      }
      if (frame === null) {
        throw new RpcError(StatusCode.UNAVAILABLE, 'stream ended before trailer');
      }
      const [payload, flags] = frame;
      if (flags & TRAILER) {
        if (payload.byteLength === 0) {
          this.lastTrailer = [StatusCode.OK, ''];
        } else {
          const status = this.opts.codec.decode(payload, RpcStatus) as RpcStatus;
          this.lastTrailer = [status.code as StatusCode, status.message];
        }
        driver.release();
        this.driver = null;
        return this.lastTrailer;
      }
    }
  }
}
