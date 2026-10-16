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
  /**
   * Make this call on behalf of a user (gateway identity): the user's JWT,
   * either a provider's token forwarded unchanged or one this node minted
   * (`AsterClientWrapper.mintUserToken`). Sent as the `aster-on-behalf-of`
   * metadata entry, replacing any in `metadata`. The calling node needs
   * `aster.gateway`; the server authorises the user, with the token's roles.
   */
  onBehalfOf?: string;
  /** Total call budget in seconds (fractions allowed); enforced client-side, sent rounded up. */
  deadlineSecs?: number;
  serializationMode?: number;
  callId?: number;
  /** Service version (`StreamHeader.version`). Typed clients set it from `@Service`; default 1. */
  version?: number;
  /** Session id for session-scoped services (0 = shared). Overrides the transport's. */
  sessionId?: number;
  /** Abort the call: the client cancels it on the wire and the call rejects with `CANCELLED`. */
  signal?: AbortSignal;
  /** Hint type for serialization: the request class constructor (e.g. StatusRequest).
   *  Used by ForyCodec to convert plain object literals to typed instances before
   *  encoding, enabling cross-language wire compatibility. */
  hintType?: unknown;
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

  /** Cancel the call (optional; transports over real streams implement it). */
  cancel?(): Promise<void>;
}
