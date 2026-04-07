/**
 * Transport interface and BidiChannel.
 *
 * The Transport interface is the abstraction point for different backends:
 * - IrohTransport: NAPI-RS, direct QUIC (Phase 4)
 * - LocalTransport: in-process, for testing (Phase 3)
 * - IrohWasmTransport: wasm-bindgen, relay-only (future Phase 8)
 */

import type { StatusCode } from '../status.js';

/** Options for a single RPC call. */
export interface CallOptions {
  metadata?: Record<string, string>;
  deadlineEpochMs?: number;
  serializationMode?: number;
  callId?: string;
}

/**
 * Aster RPC transport — the wire-level interface that all transports implement.
 *
 * The server and client layers use this interface to send/receive RPC calls.
 * The transport handles framing, stream management, and serialization.
 */
export interface AsterTransport {
  /** Perform a unary RPC call. */
  unary(
    service: string,
    method: string,
    request: unknown,
    opts?: CallOptions,
  ): Promise<unknown>;

  /** Initiate a server-streaming call. Returns an async iterable of responses. */
  serverStream(
    service: string,
    method: string,
    request: unknown,
    opts?: CallOptions,
  ): AsyncIterable<unknown>;

  /** Perform a client-streaming call. Sends items, returns single response. */
  clientStream(
    service: string,
    method: string,
    requests: AsyncIterable<unknown>,
    opts?: CallOptions,
  ): Promise<unknown>;

  /** Initiate a bidirectional-streaming call. */
  bidiStream(
    service: string,
    method: string,
    opts?: CallOptions,
  ): BidiChannel;

  /** Close the transport. */
  close(): Promise<void>;
}

/** Bidirectional streaming channel. */
export interface BidiChannel {
  /** Send a message on the channel. */
  send(msg: unknown): Promise<void>;

  /** Receive messages as an async iterable. */
  [Symbol.asyncIterator](): AsyncIterator<unknown>;

  /** Close the sending side. */
  close(): Promise<void>;

  /** Wait for the trailing status frame. */
  waitForTrailer(): Promise<[code: StatusCode, message: string]>;
}
