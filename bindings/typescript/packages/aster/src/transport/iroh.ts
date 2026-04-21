/**
 * IrohTransport — Transport implementation over real QUIC streams.
 *
 * Each RPC call opens a bidirectional QUIC stream, sends the StreamHeader
 * frame, then performs the appropriate read/write sequence for the pattern.
 *
 * Requires NAPI-RS native bindings (@aster-rpc/transport).
 */

import type { Codec } from '../codec.js';
import { JsonCodec } from '../codec.js';
import {
  writeFrame,
  readFrame,
  encodeFrame,
  HEADER,
  TRAILER,
  COMPRESSED,
  END_STREAM,
} from '../framing.js';
import { StreamHeader, RpcStatus } from '../protocol.js';
import { StatusCode, RpcError } from '../status.js';
import type { AsterTransport, CallOptions } from './base.js';
import type { BidiChannel } from './base.js';

// Lazy handle to the napi AsterCall class. Cached on first access so
// the unary hot path doesn't re-resolve the native addon every call.
// Falls back to `null` if the addon isn't loadable — the caller keeps
// the old openBi path as a safety net.
let _nativeAsterCall: any | null | undefined = undefined;
function getNativeAsterCall(): any | null {
  if (_nativeAsterCall !== undefined) return _nativeAsterCall;
  try {
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const native = require('@aster-rpc/transport');
    _nativeAsterCall = native?.AsterCall ?? null;
  } catch {
    _nativeAsterCall = null;
  }
  return _nativeAsterCall;
}

/** QUIC connection interface (matches NAPI IrohConnection). */
interface QuicConnection {
  openBi(): Promise<{ takeSend(): QuicSendStream; takeRecv(): QuicRecvStream }>;
  close(code: number, reason: string): void;
}

/** QUIC send stream interface (matches NAPI IrohSendStream). */
interface QuicSendStream {
  writeAll(data: Uint8Array): Promise<void>;
  finish(): Promise<void>;
}

/** QUIC recv stream interface (matches NAPI IrohRecvStream). */
interface QuicRecvStream {
  readExact(n: number): Promise<Uint8Array>;
}

function buildMetadata(metadata?: Record<string, string>): [string[], string[]] {
  if (!metadata || Object.keys(metadata).length === 0) return [[], []];
  const keys = Object.keys(metadata);
  return [keys, keys.map(k => metadata[k]!)];
}

/**
 * Transport implementation using Iroh QUIC streams.
 */
export class IrohTransport implements AsterTransport {
  private conn: QuicConnection;
  private codec: Codec;
  private readonly sessionId: number;

  constructor(
    connection: QuicConnection,
    codec?: Codec,
    options?: { sessionId?: number },
  ) {
    this.conn = connection;
    this.codec = codec ?? new JsonCodec();
    this.sessionId = options?.sessionId ?? 0;
  }

  async unary(
    service: string,
    method: string,
    request: unknown,
    opts?: CallOptions,
  ): Promise<unknown> {
    // Fast path: one napi round-trip through `AsterCall.unaryFastPath`
    // (mirrors `ffi::aster_call_unary`). Replaces the ~10-hop
    // openBi → 2x writeFrame → finish → 2x readFrame sequence with a
    // single native call that does acquire + write_all + recv-loop +
    // release on the Rust side. Falls back to the legacy path if the
    // native addon isn't loadable.
    const fast = getNativeAsterCall();
    if (fast && typeof fast.unaryFastPath === 'function') {
      try {
        // Build the header and request frames separately, then
        // concatenate into a single buffer so Quinn sees one logical
        // write on the server side. The request carries
        // `FLAG_END_STREAM` so the core dispatch loop knows the
        // request phase is done and can start responding.
        const headerBytes = this.codec.encode(
          new StreamHeader({
            service,
            method,
            version: 1,
            callId: opts?.callId ?? 0,
            deadline: opts?.deadlineSecs ?? 0,
            serializationMode:
              opts?.serializationMode ?? (this.codec instanceof JsonCodec ? 3 : 0),
            metadataKeys: buildMetadata(opts?.metadata)[0],
            metadataValues: buildMetadata(opts?.metadata)[1],
            sessionId: this.sessionId,
          }),
        );
        const headerFrame = encodeFrame(headerBytes, HEADER);
        const [reqPayload, compressed] = this.codec.encodeCompressed(request, opts?.hintType);
        const requestFrame = encodeFrame(
          reqPayload,
          (compressed ? COMPRESSED : 0) | END_STREAM,
        );
        const requestPair = new Uint8Array(
          headerFrame.byteLength + requestFrame.byteLength,
        );
        requestPair.set(headerFrame, 0);
        requestPair.set(requestFrame, headerFrame.byteLength);

        const result = await fast.unaryFastPath(
          this.conn,
          this.sessionId,
          requestPair,
        );

        // Trailer: empty means clean OK; non-empty carries an RpcStatus.
        if (result.trailer && result.trailer.byteLength > 0) {
          this.checkTrailer(result.trailer);
        }
        // Response body: empty means the dispatcher only sent a
        // trailer (error case). For OK-with-empty-response, checkTrailer
        // already passed and we return undefined.
        if (!result.response || result.response.byteLength === 0) {
          return undefined;
        }
        return this.codec.decode(result.response);
      } catch (e) {
        throw mapTransportError(e);
      }
    }

    // Legacy fallback path — kept only for environments where the
    // native addon isn't resolvable. Functionally identical to the
    // pre-fast-path implementation.
    const bi = await this.conn.openBi();
    const send = bi.takeSend();
    const recv = bi.takeRecv();

    try {
      // Write StreamHeader
      await this.writeHeader(send, service, method, opts);

      // Write request
      const [payload, compressed] = this.codec.encodeCompressed(request, opts?.hintType);
      await writeFrame(send, payload, compressed ? COMPRESSED : 0);
      await send.finish();

      // Read response frames
      let response: unknown = undefined;
      while (true) {
        const frame = await readFrame(recv, 0);
        if (!frame) throw new RpcError(StatusCode.UNAVAILABLE, 'stream ended before response');
        const [data, flags] = frame;

        if (flags & TRAILER) {
          this.checkTrailer(data);
          break;
        }

        response = this.codec.decode(data);
      }

      return response;
    } catch (e) {
      throw mapTransportError(e);
    }
  }

  async *serverStream(
    service: string,
    method: string,
    request: unknown,
    opts?: CallOptions,
  ): AsyncIterable<unknown> {
    const bi = await this.conn.openBi();
    const send = bi.takeSend();
    const recv = bi.takeRecv();

    try {
      await this.writeHeader(send, service, method, opts);

      const [payload, compressed] = this.codec.encodeCompressed(request, opts?.hintType);
      await writeFrame(send, payload, compressed ? COMPRESSED : 0);
      await send.finish();

      while (true) {
        const frame = await readFrame(recv, 0);
        if (!frame) throw new RpcError(StatusCode.UNAVAILABLE, 'stream ended before trailer');
        const [data, flags] = frame;

        if (flags & TRAILER) {
          this.checkTrailer(data);
          break;
        }

        yield this.codec.decode(data);
      }
    } catch (e) {
      throw mapTransportError(e);
    }
  }

  async clientStream(
    service: string,
    method: string,
    requests: AsyncIterable<unknown>,
    opts?: CallOptions,
  ): Promise<unknown> {
    const bi = await this.conn.openBi();
    const send = bi.takeSend();
    const recv = bi.takeRecv();

    try {
      await this.writeHeader(send, service, method, opts);

      for await (const req of requests) {
        const [payload, compressed] = this.codec.encodeCompressed(req, opts?.hintType);
        await writeFrame(send, payload, compressed ? COMPRESSED : 0);
      }
      await send.finish();

      // Read single response + trailer
      let response: unknown = undefined;
      while (true) {
        const frame = await readFrame(recv, 0);
        if (!frame) throw new RpcError(StatusCode.UNAVAILABLE, 'stream ended before response');
        const [data, flags] = frame;

        if (flags & TRAILER) {
          this.checkTrailer(data);
          break;
        }

        response = this.codec.decode(data);
      }

      return response;
    } catch (e) {
      throw mapTransportError(e);
    }
  }

  bidiStream(
    service: string,
    method: string,
    opts?: CallOptions,
  ): BidiChannel {
    const codec = this.codec;
    const conn = this.conn;
    const writeHeader = this.writeHeader.bind(this);
    const checkTrailer = this.checkTrailer.bind(this);

    // Open the stream eagerly but return a channel object
    let sendStream: QuicSendStream | undefined;
    let recvStream: QuicRecvStream | undefined;
    let streamReady: Promise<void>;
    let receiveDone = false;

    streamReady = (async () => {
      const bi = await conn.openBi();
      sendStream = bi.takeSend();
      recvStream = bi.takeRecv();
      await writeHeader(sendStream, service, method, opts);
    })();

    const channel: BidiChannel = {
      async send(msg: unknown): Promise<void> {
        await streamReady;
        const [payload, compressed] = codec.encodeCompressed(msg, opts?.hintType);
        await writeFrame(sendStream!, payload, compressed ? COMPRESSED : 0);
      },

      async *[Symbol.asyncIterator](): AsyncIterator<unknown> {
        await streamReady;
        while (!receiveDone) {
          try {
            const frame = await readFrame(recvStream!, 0);
            if (!frame) { receiveDone = true; return; }
            const [data, flags] = frame;
            if (flags & TRAILER) {
              checkTrailer(data);
              receiveDone = true;
              return;
            }
            yield codec.decode(data);
          } catch (e) {
            receiveDone = true;
            throw mapTransportError(e);
          }
        }
      },

      async close(): Promise<void> {
        await streamReady;
        await sendStream!.finish();
      },

      async waitForTrailer(): Promise<[StatusCode, string]> {
        await streamReady;
        // Drain remaining frames until trailer
        while (!receiveDone) {
          const frame = await readFrame(recvStream!, 0);
          if (!frame) return [StatusCode.OK, ''];
          const [data, flags] = frame;
          if (flags & TRAILER) {
            if (data.length === 0) return [StatusCode.OK, ''];
            const status = codec.decode(data) as RpcStatus;
            return [status.code as StatusCode, status.message];
          }
        }
        return [StatusCode.OK, ''];
      },
    };

    return channel;
  }

  async close(): Promise<void> {
    this.conn.close(0, 'normal close');
  }

  // -- Helpers ----------------------------------------------------------------

  private async writeHeader(
    send: QuicSendStream,
    service: string,
    method: string,
    opts?: CallOptions,
  ): Promise<void> {
    const [keys, values] = buildMetadata(opts?.metadata);
    const header = new StreamHeader({
      service,
      method,
      version: 1,
      callId: opts?.callId ?? 0,
      deadline: opts?.deadlineSecs ?? 0,
      serializationMode: opts?.serializationMode ?? (this.codec instanceof JsonCodec ? 3 : 0),
      metadataKeys: keys,
      metadataValues: values,
      sessionId: this.sessionId,
    });
    const headerBytes = this.codec.encode(header);
    await writeFrame(send, headerBytes, HEADER);
  }

  private checkTrailer(data: Uint8Array): void {
    if (data.length === 0) return; // empty trailer = OK
    const status = this.codec.decode(data) as RpcStatus;
    if (status.code !== StatusCode.OK) {
      throw RpcError.fromStatus(
        status.code as any,
        status.message,
        status.details,
      );
    }
  }
}

const RESET_KEYWORDS = ['reset', 'connection closed', 'connection lost'];

function mapTransportError(e: unknown): RpcError {
  if (e instanceof RpcError) return e;
  if (e instanceof Error) {
    const msg = e.message.toLowerCase();
    if (RESET_KEYWORDS.some(kw => msg.includes(kw))) {
      return new RpcError(StatusCode.UNAVAILABLE, `stream reset: ${e.message}`);
    }
  }
  if (e instanceof Error) return new RpcError(StatusCode.UNKNOWN, e.message);
  return new RpcError(StatusCode.UNKNOWN, String(e));
}
