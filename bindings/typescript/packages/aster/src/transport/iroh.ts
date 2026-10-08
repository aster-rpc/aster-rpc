/**
 * IrohTransport -- the client over a real `aster/1` QUIC connection.
 *
 * A thin layer over the native `RpcConnection` (`aster::rpc::RpcConnection`,
 * design `dynamic-dispatch-design.md` §3): Rust builds the header, frames,
 * compresses, pools unary streams, enforces the deadline and decodes the
 * trailer. This class only encodes/decodes payloads with the codec, maps a
 * failed call to an `RpcError`, and wires `AbortSignal`s to cancellation.
 *
 * Requires the NAPI-RS addon (`@aster-rpc/transport`).
 */

import { traceMetadata } from '../tracing.js';
import type {
  IrohConnection,
  RpcConnection as NativeRpcConnection,
  ClientCall,
} from '@aster-rpc/transport';
import type { Codec } from '../codec.js';
import { createXlangCodec } from '../xlang.js';
import { StatusCode, RpcError } from '../status.js';
import type { AsterTransport, BidiChannel, CallOptions } from './base.js';
import { loadNative } from '../native.js';

/** The message prefix of a native call failure (JSON status follows). */
const STATUS_PREFIX = 'aster-rpc-status:';

/** Native call failure → `RpcError`; anything else as before T4. */
export function mapTransportError(e: unknown): RpcError {
  if (e instanceof RpcError) return e;
  const msg = e instanceof Error ? e.message : String(e);
  const at = msg.indexOf(STATUS_PREFIX);
  if (at >= 0) {
    try {
      const st = JSON.parse(msg.slice(at + STATUS_PREFIX.length)) as {
        code: number;
        message: string;
        details?: Record<string, string>;
      };
      const details = st.details && Object.keys(st.details).length ? st.details : undefined;
      return RpcError.fromStatus(st.code as StatusCode, st.message, details);
    } catch {
      /* fall through */
    }
  }
  const lower = msg.toLowerCase();
  if (['reset', 'connection closed', 'connection lost'].some((kw) => lower.includes(kw))) {
    return new RpcError(StatusCode.UNAVAILABLE, `stream reset: ${msg}`);
  }
  return new RpcError(StatusCode.UNKNOWN, msg);
}

function metadataList(metadata?: Record<string, string>): string[] | undefined {
  // The current trace context rides along as `traceparent` / `baggage`
  // (Rust parents the client span on it); explicit metadata wins.
  const merged = { ...traceMetadata(), ...metadata };
  let out: string[] | undefined;
  for (const k in merged) {
    (out ??= []).push(k, String(merged[k]));
  }
  return out;
}

function deadlineMs(opts?: CallOptions): number {
  const s = opts?.deadlineSecs;
  return s && s > 0 ? s * 1000 : 0;
}

function cancelledError(): RpcError {
  return RpcError.fromStatus(StatusCode.CANCELLED, 'call cancelled');
}

/** The native `RpcConnection` for `connection` (one per connection). */
const _rpcByConn = new WeakMap<object, NativeRpcConnection>();
function rpcOf(connection: object): NativeRpcConnection {
  let rpc = _rpcByConn.get(connection);
  if (!rpc) {
    rpc = new (loadNative().RpcConnection)(connection as IrohConnection);
    _rpcByConn.set(connection, rpc);
  }
  return rpc;
}

/** Queued and in-flight request item ceiling (including zero-byte items). */
const MAX_PENDING_REQUESTS = 1024;
/** Pause at 8 MiB, allowing the just-encoded legal 16 MiB frame: at most 24 MiB. */
const MAX_RETAINED_REQUEST_BYTES = 8 * 1024 * 1024;

/**
 * Writes a request stream with at most one native write in flight; requests
 * produced meanwhile go out together in the next write (`sendMany`).
 */
class RequestPump {
  private pending: Uint8Array[] = [];
  private inflight: Promise<void> | null = null;
  private pendingBytes = 0;
  private retainedBytes = 0;
  private retainedItems = 0;
  private wakeStopped!: () => void;
  private readonly stopSignal = new Promise<void>((resolve) => { this.wakeStopped = resolve; });
  private error: unknown = undefined;
  private stopped = false;

  constructor(private readonly call: ClientCall) {}

  async push(payload: Uint8Array): Promise<void> {
    if (this.stopped) return;
    if (this.error !== undefined) throw this.error;
    this.pending.push(payload);
    this.pendingBytes += payload.byteLength;
    this.retainedBytes += payload.byteLength;
    this.retainedItems++;
    // Assign the promise before run starts: even a synchronous native validation
    // throw must leave finally's null state intact, rather than a settled write
    // promise that flush would await forever.
    if (!this.inflight) this.inflight = Promise.resolve().then(() => this.run());
    if (this.retainedItems >= MAX_PENDING_REQUESTS || this.retainedBytes >= MAX_RETAINED_REQUEST_BYTES) {
      // Only one producer calls push. Pause before it advances/encodes another item;
      // account for the native batch as well as requests waiting behind it.
      await Promise.race([this.inflight!, this.stopSignal]);
      if (!this.stopped && this.error !== undefined) throw this.error;
    }
  }

  private async run(): Promise<void> {
    try {
      while (!this.stopped && this.pending.length) {
        const batch = this.pending;
        const bytes = this.pendingBytes;
        this.pending = [];
        this.pendingBytes = 0;
        try {
          if (batch.length === 1) await this.call.send(batch[0]!);
          else await this.call.sendMany(batch, false);
        } finally {
          this.retainedBytes -= bytes;
          this.retainedItems -= batch.length;
        }
      }
    } catch (e) {
      this.error = e;
      this.clearPending();
    } finally {
      this.inflight = null;
    }
  }

  private clearPending(): void {
    this.retainedBytes -= this.pendingBytes;
    this.retainedItems -= this.pending.length;
    this.pending = [];
    this.pendingBytes = 0;
  }

  stop(): void {
    this.stopped = true;
    this.clearPending();
    this.wakeStopped();
  }

  async flush(): Promise<void> {
    while (this.inflight && !this.stopped) await Promise.race([this.inflight, this.stopSignal]);
    if (this.error !== undefined) throw this.error;
  }
}

/**
 * Transport over one `aster/1` connection (`IrohConnection`).
 */
export class IrohTransport implements AsterTransport {
  readonly conn: IrohConnection;
  readonly codec: Codec;
  /** Session id stamped on every call (0 = shared). */
  readonly sessionId: number;
  private readonly rpc: NativeRpcConnection;

  constructor(
    connection: IrohConnection | object,
    codec?: Codec,
    options?: { sessionId?: number; rpc?: NativeRpcConnection },
  ) {
    this.conn = connection as IrohConnection;
    this.codec = codec ?? createXlangCodec();
    this.sessionId = options?.sessionId ?? 0;
    this.rpc = options?.rpc ?? rpcOf(connection);
  }

  /** The native pooled client this transport calls through. */
  get native(): NativeRpcConnection {
    return this.rpc;
  }

  /**
   * A transport on the same connection pinned to a session: a fresh id from
   * the connection's shared counter (or `sessionId`). Calls on a
   * session-scoped service through it reach one server-side instance.
   */
  openSession(sessionId?: number): IrohTransport {
    return new IrohTransport(this.conn, this.codec, {
      sessionId: sessionId ?? this.rpc.nextSessionId(),
      rpc: this.rpc,
    });
  }

  async unary(service: string, method: string, request: unknown, opts?: CallOptions): Promise<unknown> {
    const payload = this.codec.encode(request, opts?.hintType);
    const signal = opts?.signal;
    let token: InstanceType<ReturnType<typeof loadNative>['CancelToken']> | undefined;
    let onAbort: (() => void) | undefined;
    if (signal) {
      if (signal.aborted) throw cancelledError();
      token = new (loadNative().CancelToken)();
      const t = token;
      onAbort = () => t.cancel();
      signal.addEventListener('abort', onAbort, { once: true });
    }
    let response: Buffer;
    try {
      response = await this.rpc.unary(
        service,
        method,
        opts?.version ?? 1,
        opts?.sessionId ?? this.sessionId,
        deadlineMs(opts),
        metadataList(opts?.metadata),
        opts?.serializationMode,
        opts?.callId,
        payload,
        token,
      );
    } catch (e) {
      throw mapTransportError(e);
    } finally {
      if (onAbort) signal!.removeEventListener('abort', onAbort);
    }
    // An OK call with no response frame (the handler returned nothing).
    if (response.byteLength === 0) return undefined;
    return this.codec.decode(response);
  }

  async *serverStream(service: string, method: string, request: unknown, opts?: CallOptions): AsyncIterable<unknown> {
    const payload = this.codec.encode(request, opts?.hintType);
    const signal = opts?.signal;
    if (signal?.aborted) throw cancelledError();
    let call: ClientCall;
    try {
      call = await this.rpc.serverStream(
        service,
        method,
        opts?.version ?? 1,
        opts?.sessionId ?? this.sessionId,
        deadlineMs(opts),
        metadataList(opts?.metadata),
        opts?.serializationMode,
        opts?.callId,
        payload,
      );
    } catch (e) {
      throw mapTransportError(e);
    }
    // The signal may have fired while the native call was opening.
    if (signal?.aborted) {
      void call.cancel();
      throw cancelledError();
    }
    const onAbort = () => void call.cancel();
    signal?.addEventListener('abort', onAbort, { once: true });
    let done = false;
    try {
      while (true) {
        let item: Buffer | null;
        try {
          item = await call.next();
        } catch (e) {
          done = true;
          throw mapTransportError(e);
        }
        if (item === null) {
          done = true;
          return;
        }
        yield this.codec.decode(item);
      }
    } finally {
      signal?.removeEventListener('abort', onAbort);
      // The consumer stopped early (break / throw): cancel the call.
      if (!done) void call.cancel();
    }
  }

  async clientStream(
    service: string,
    method: string,
    requests: AsyncIterable<unknown>,
    opts?: CallOptions,
  ): Promise<unknown> {
    const signal = opts?.signal;
    if (signal?.aborted) throw cancelledError();
    let call: ClientCall;
    try {
      call = await this.rpc.clientStream(
        service,
        method,
        opts?.version ?? 1,
        opts?.sessionId ?? this.sessionId,
        deadlineMs(opts),
        metadataList(opts?.metadata),
        opts?.serializationMode,
        opts?.callId,
      );
    } catch (e) {
      throw mapTransportError(e);
    }
    // The signal may have fired while the native call was opening.
    if (signal?.aborted) {
      void call.cancel();
      throw cancelledError();
    }
    const onAbort = () => void call.cancel();
    signal?.addEventListener('abort', onAbort, { once: true });
    const pump = new RequestPump(call);
    let terminal = false;
    let sourceClosed = false;
    let iterator: AsyncIterator<unknown> | Iterator<unknown> | undefined;
    // Neither a pending next() nor an uncooperative return() may delay the RPC.
    const closeRequests = () => {
      terminal = true;
      pump.stop();
      if (sourceClosed) return;
      sourceClosed = true;
      try { void Promise.resolve(iterator?.return?.()).catch(() => {}); }
      catch { /* Best-effort source cleanup. */ }
    };
    const response = Promise.resolve().then(() => call.response()).then(
      (out) => { closeRequests(); return out; },
      (error) => { closeRequests(); throw mapTransportError(error); },
    );
    const producing = (async (): Promise<Buffer> => {
      try {
        const asyncIterator = requests[Symbol.asyncIterator]?.();
        iterator = asyncIterator ?? (requests as unknown as Iterable<unknown>)[Symbol.iterator]();
        while (!terminal) {
          const item = await iterator.next();
          if (terminal) break;
          if (item.done) {
            sourceClosed = true;
            await pump.flush();
            if (!terminal) await call.finish();
            break;
          }
          // for-await unwraps promised values from synchronous iterators only.
          const value = asyncIterator ? item.value : await item.value;
          if (terminal) break;
          await pump.push(this.codec.encode(value, opts?.hintType));
        }
      } catch (error) {
        if (!terminal) {
          const nativeFailure = String((error as Error)?.message ?? error).includes(STATUS_PREFIX)
            ? mapTransportError(error) : undefined;
          const terminalSendCodes: readonly StatusCode[] = [
            StatusCode.CANCELLED,
            StatusCode.INVALID_ARGUMENT,
            StatusCode.DEADLINE_EXCEEDED,
          ];
          if (nativeFailure && terminalSendCodes.includes(nativeFailure.code)) {
            // RequestSink reports completed/cancelled/deadline calls with these
            // codes; the response owns the peer's actual terminal status.
            await response;
            throw nativeFailure;
          }
          // Local framing validation or a failed write need not end the reader.
          // Cancel it and report that original failure without waiting for a reply.
          closeRequests();
          void Promise.resolve(call.cancel()).catch(() => {});
          throw nativeFailure ?? error;
        }
      }
      // Finishing the request half does not finish the response half.
      return await response;
    })();
    try {
      const out = await Promise.race([response, producing]);
      return out.byteLength ? this.codec.decode(out) : undefined;
    } finally {
      closeRequests();
      signal?.removeEventListener('abort', onAbort);
    }
  }

  bidiStream(service: string, method: string, opts?: CallOptions): BidiChannel {
    const codec = this.codec;
    const signal = opts?.signal;
    const ready: Promise<ClientCall> = this.rpc
      .bidi(
        service,
        method,
        opts?.version ?? 1,
        opts?.sessionId ?? this.sessionId,
        deadlineMs(opts),
        metadataList(opts?.metadata),
        opts?.serializationMode,
        opts?.callId,
      )
      .then((call) => {
        if (signal) {
          if (signal.aborted) void call.cancel();
          else signal.addEventListener('abort', () => void call.cancel(), { once: true });
        }
        return call;
      });
    ready.catch(() => {});
    const open = async (): Promise<ClientCall> => {
      try {
        return await ready;
      } catch (e) {
        throw mapTransportError(e);
      }
    };
    let trailer: [StatusCode, string] | null = null;
    let finished = false;

    const next = async (): Promise<IteratorResult<unknown>> => {
      if (trailer) return { done: true, value: undefined };
      try {
        const call = await open();
        const item = await call.next();
        if (item === null) {
          trailer = [StatusCode.OK, ''];
          return { done: true, value: undefined };
        }
        return { done: false, value: codec.decode(item) };
      } catch (e) {
        const err = mapTransportError(e);
        trailer = [err.code, err.message];
        throw err;
      }
    };

    return {
      async send(msg: unknown): Promise<void> {
        const call = await open();
        try {
          await call.send(codec.encode(msg, opts?.hintType));
        } catch (e) {
          throw mapTransportError(e);
        }
      },
      [Symbol.asyncIterator](): AsyncIterator<unknown> {
        return {
          next,
          async return(): Promise<IteratorResult<unknown>> {
            // The consumer stopped reading before the end: cancel.
            if (!trailer) {
              trailer = [StatusCode.CANCELLED, 'call cancelled'];
              void (await open()).cancel();
            }
            return { done: true, value: undefined };
          },
        };
      },
      async close(): Promise<void> {
        if (finished) return;
        finished = true;
        const call = await open();
        try {
          await call.finish();
        } catch (e) {
          throw mapTransportError(e);
        }
      },
      async waitForTrailer(): Promise<[StatusCode, string]> {
        try {
          while (!(await next()).done) { /* drain */ }
        } catch {
          /* recorded in `trailer` */
        }
        return trailer ?? [StatusCode.OK, ''];
      },
      async cancel(): Promise<void> {
        trailer ??= [StatusCode.CANCELLED, 'call cancelled'];
        await (await open()).cancel();
      },
    };
  }

  async close(): Promise<void> {
    try {
      this.conn.close(0, 'normal close');
    } catch {
      /* already closed */
    }
  }
}
