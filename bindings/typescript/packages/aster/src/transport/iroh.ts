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
  HEADER,
  TRAILER,
  COMPRESSED,
} from '../framing.js';
import { StreamHeader, RpcStatus } from '../protocol.js';
import { StatusCode, RpcError } from '../status.js';
import type { AsterTransport, CallOptions, BidiChannel } from './base.js';

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

  constructor(connection: QuicConnection, codec?: Codec) {
    this.conn = connection;
    this.codec = codec ?? new JsonCodec();
  }

  async unary(
    service: string,
    method: string,
    request: unknown,
    opts?: CallOptions,
  ): Promise<unknown> {
    const bi = await this.conn.openBi();
    const send = bi.takeSend();
    const recv = bi.takeRecv();

    try {
      // Write StreamHeader
      await this.writeHeader(send, service, method, opts);

      // Write request
      const [payload, compressed] = this.codec.encodeCompressed(request);
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

      const [payload, compressed] = this.codec.encodeCompressed(request);
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
        const [payload, compressed] = this.codec.encodeCompressed(req);
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
    _service: string,
    _method: string,
    _opts?: CallOptions,
  ): BidiChannel {
    // Bidi is complex — defer to future implementation
    throw new RpcError(StatusCode.UNIMPLEMENTED, 'bidi_stream over IrohTransport not yet implemented');
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
      contractId: opts?.contractId ?? '',
      callId: opts?.callId ?? crypto.randomUUID(),
      deadlineEpochMs: opts?.deadlineEpochMs ?? 0,
      serializationMode: opts?.serializationMode ?? 0,
      metadataKeys: keys,
      metadataValues: values,
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
