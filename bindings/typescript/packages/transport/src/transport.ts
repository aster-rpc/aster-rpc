/**
 * Stream protocol abstractions for the framing layer.
 *
 * These interfaces allow framing to work with both real Iroh QUIC streams
 * (from NAPI-RS) and in-memory buffers (for testing). Mirrors the Python
 * SendStream/RecvStream protocols in aster/framing.py.
 */

/** Minimal async send-stream interface (matches IrohSendStream from NAPI). */
export interface SendStream {
  writeAll(data: Uint8Array): Promise<void>;
}

/** Minimal async recv-stream interface (matches IrohRecvStream from NAPI). */
export interface RecvStream {
  readExact(n: number): Promise<Uint8Array>;
}
