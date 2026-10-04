/**
 * Wire-level frame read/write.
 *
 * Spec reference: Aster-SPEC §6.1 (stream framing), §5.6 (compression)
 *
 * Frame layout (on a QUIC stream):
 *
 *     +----------+-------+----------+
 *     | Length   | Flags | Payload  |
 *     | 4 bytes  |1 byte | variable |
 *     | LE u32   |       |          |
 *     +----------+-------+----------+
 *
 * Frame encode/decode and zstd payload compression are Rust core
 * (`core/src/framing.rs`) via the NAPI addon -- one implementation for every
 * binding, on Node and Bun alike. The RPC paths no longer frame in JS: since
 * T4 the server is `aster::rpc::Server` and the client `RpcConnection`, so the
 * stream I/O helpers are gone. What is left is the pure codec, kept for
 * tooling and the conformance vectors.
 */

import { loadNative } from './native.js';

// -- Flag constants (values pinned by Aster-SPEC §6.1; equal core's) ----------

/** Bit 0 -- payload is zstd-compressed. */
export const COMPRESSED = 0x01;
/** Bit 1 -- trailing status frame. */
export const TRAILER = 0x02;
/** Bit 2 -- stream header (first frame). */
export const HEADER = 0x04;
/** Bit 3 -- Fory row schema frame. */
export const ROW_SCHEMA = 0x08;
/** Bit 4 -- per-call header in a session stream. */
export const CALL = 0x10;
/** Bit 5 -- cancel current call in a session stream. */
export const CANCEL = 0x20;
/** Bit 6 -- last request frame for this call on a multiplexed stream. */
export const END_STREAM = 0x40;

// -- Errors -------------------------------------------------------------------

/** Raised when a framing violation is detected. */
export class FramingError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'FramingError';
  }
}

function framing<T>(fn: () => T): T {
  try {
    return fn();
  } catch (e) {
    throw new FramingError(e instanceof Error ? e.message : String(e));
  }
}

function bytes(b: Uint8Array): Uint8Array {
  return new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
}


// -- Byte-level encode/decode (Rust core) ---------------------------------------

/**
 * Encode a frame to raw wire bytes, payload unchanged. Use for envelope and
 * control frames; data frames go through {@link encodePayloadFrame}.
 *
 * @throws FramingError for an empty payload without TRAILER/CANCEL, or a
 *   frame larger than MAX_FRAME_SIZE.
 */
export function encodeFrame(payload: Uint8Array, flags = 0): Uint8Array {
  return framing(() => bytes(loadNative().encodeFrameNative(toBuffer(payload), flags)));
}

/**
 * Encode a data frame, zstd-compressing the payload (and setting COMPRESSED)
 * when it is larger than `threshold` bytes (default 4096) and compression
 * actually shrinks it (Aster-SPEC §5.6).
 */
export function encodePayloadFrame(
  payload: Uint8Array,
  flags = 0,
  threshold?: number,
): Uint8Array {
  return framing(() => bytes(loadNative().encodePayloadFrame(payload, flags, threshold)));
}

/** Result of reading a frame: [payload, flags]. */
export type FrameResult = [payload: Uint8Array, flags: number];

/** Decode complete wire bytes into `[payload, flags]` (payload as sent). */
export function decodeFrame(wire: Uint8Array): FrameResult {
  return framing(() => {
    const d = loadNative().decodeFrameNative(toBuffer(wire));
    return [bytes(d.payload), d.flags] as FrameResult;
  });
}

/**
 * The logical payload of a received frame: zstd-decompressed (refusing more
 * than 16 MiB of output) when `flags` carries COMPRESSED, else `payload`.
 */
export function payloadOf(payload: Uint8Array, flags: number): Uint8Array {
  if (!(flags & COMPRESSED)) return payload;
  return framing(() => bytes(loadNative().payloadOf(payload, flags)));
}

function toBuffer(b: Uint8Array): Buffer {
  return Buffer.isBuffer(b) ? b : Buffer.from(b.buffer, b.byteOffset, b.byteLength);
}
