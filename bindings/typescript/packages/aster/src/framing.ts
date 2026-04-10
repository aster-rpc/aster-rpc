/**
 * Wire-level frame read/write.
 *
 * Spec reference: S6.1 (stream framing)
 *
 * Frame layout (on a QUIC stream):
 *
 *     +----------+-------+----------+
 *     | Length   | Flags | Payload  |
 *     | 4 bytes  |1 byte | variable |
 *     | LE u32   |       |          |
 *     +----------+-------+----------+
 *
 * - Length is the total size of Flags + Payload (i.e. payload.length + 1).
 *   Maximum 16 MiB per frame. A Length of 0 is invalid.
 * - Flags is a 1-byte bitfield (see constants below).
 * - Payload is the serialized bytes.
 */

import { MAX_FRAME_SIZE, DEFAULT_FRAME_READ_TIMEOUT_S } from './limits.js';

// -- Stream protocol abstractions ---------------------------------------------
//
// Minimal interfaces that let framing work with both real Iroh QUIC streams
// (from @aster-rpc/transport) and in-memory buffers (for testing). Mirrors
// the SendStream/RecvStream protocols in bindings/python/aster/framing.py.

/** Minimal async send-stream interface (matches IrohSendStream from NAPI). */
export interface SendStream {
  writeAll(data: Uint8Array): Promise<void>;
}

/** Minimal async recv-stream interface (matches IrohRecvStream from NAPI). */
export interface RecvStream {
  readExact(n: number): Promise<Uint8Array>;
}

// -- Flag constants -----------------------------------------------------------

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

// -- Errors -------------------------------------------------------------------

/** Raised when a framing violation is detected. */
export class FramingError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'FramingError';
  }
}

// -- Internal constants -------------------------------------------------------

const LENGTH_SIZE = 4;
const FLAGS_SIZE = 1;

// -- writeFrame ---------------------------------------------------------------

/**
 * Write a single frame to a send stream.
 *
 * @param stream - An async send stream with a writeAll method.
 * @param payload - The serialized payload bytes.
 * @param flags - The 1-byte flag bitfield (default 0).
 * @throws FramingError if the frame exceeds MAX_FRAME_SIZE or has an invalid empty payload.
 */
export async function writeFrame(
  stream: SendStream,
  payload: Uint8Array,
  flags = 0,
): Promise<void> {
  const frameBodyLen = FLAGS_SIZE + payload.byteLength;

  // Zero-length payloads are permitted only for TRAILER and CANCEL control frames
  if (frameBodyLen === FLAGS_SIZE && !(flags & (TRAILER | CANCEL))) {
    throw new FramingError('zero-length payload is not permitted');
  }

  if (frameBodyLen > MAX_FRAME_SIZE) {
    throw new FramingError(
      `frame size ${frameBodyLen} exceeds maximum ${MAX_FRAME_SIZE}`,
    );
  }

  const buf = new Uint8Array(LENGTH_SIZE + FLAGS_SIZE + payload.byteLength);
  const view = new DataView(buf.buffer);
  view.setUint32(0, frameBodyLen, true); // little-endian
  buf[LENGTH_SIZE] = flags;
  buf.set(payload, LENGTH_SIZE + FLAGS_SIZE);

  await stream.writeAll(buf);
}

// -- readFrame ----------------------------------------------------------------

/** Result of reading a frame: [payload, flags]. */
export type FrameResult = [payload: Uint8Array, flags: number];

/**
 * Read a single frame from a receive stream.
 *
 * @param stream - The QUIC receive stream.
 * @param timeoutS - Optional read timeout in seconds. Defaults to DEFAULT_FRAME_READ_TIMEOUT_S.
 *                    Pass 0 to disable.
 * @returns A [payload, flags] tuple, or null if the stream has ended cleanly.
 * @throws FramingError on wire-format violations (zero length, oversized frame).
 */
export async function readFrame(
  stream: RecvStream,
  timeoutS?: number,
): Promise<FrameResult | null> {
  const timeout = timeoutS ?? DEFAULT_FRAME_READ_TIMEOUT_S;
  const effectiveTimeout = timeout > 0 ? timeout * 1000 : undefined;

  // Read the 4-byte length prefix
  let lengthBytes: Uint8Array;
  try {
    lengthBytes = await withTimeout(
      stream.readExact(LENGTH_SIZE),
      effectiveTimeout,
      'frame read timed out waiting for length header',
    );
  } catch (e) {
    if (e instanceof FramingError) throw e;
    // Stream ended or was reset -- treat as clean EOF
    return null;
  }

  if (lengthBytes.byteLength < LENGTH_SIZE) {
    return null;
  }

  const view = new DataView(
    lengthBytes.buffer,
    lengthBytes.byteOffset,
    lengthBytes.byteLength,
  );
  const frameBodyLen = view.getUint32(0, true);

  if (frameBodyLen === 0) {
    throw new FramingError('received zero-length frame');
  }

  if (frameBodyLen > MAX_FRAME_SIZE) {
    throw new FramingError(
      `frame size ${frameBodyLen} exceeds maximum ${MAX_FRAME_SIZE}`,
    );
  }

  let body: Uint8Array;
  try {
    body = await withTimeout(
      stream.readExact(frameBodyLen),
      effectiveTimeout,
      `frame read timed out waiting for ${frameBodyLen} bytes of body`,
    );
  } catch (e) {
    if (e instanceof FramingError) throw e;
    throw new FramingError(
      `incomplete frame: expected ${frameBodyLen} bytes`,
    );
  }

  if (body.byteLength < frameBodyLen) {
    throw new FramingError(
      `incomplete frame: expected ${frameBodyLen} bytes, got ${body.byteLength}`,
    );
  }

  const flags = body[0]!;
  const payload = body.subarray(1);
  return [payload, flags];
}

// -- Helpers ------------------------------------------------------------------

/**
 * Encode a frame to raw bytes (for testing and conformance vectors).
 * Does not write to a stream -- returns the complete wire bytes.
 */
export function encodeFrame(payload: Uint8Array, flags = 0): Uint8Array {
  const frameBodyLen = FLAGS_SIZE + payload.byteLength;
  const buf = new Uint8Array(LENGTH_SIZE + frameBodyLen);
  const view = new DataView(buf.buffer);
  view.setUint32(0, frameBodyLen, true);
  buf[LENGTH_SIZE] = flags;
  buf.set(payload, LENGTH_SIZE + FLAGS_SIZE);
  return buf;
}

/**
 * Decode raw wire bytes into [payload, flags] (for testing and conformance vectors).
 * Does not read from a stream -- parses the complete wire bytes.
 */
export function decodeFrame(wire: Uint8Array): FrameResult {
  if (wire.byteLength < LENGTH_SIZE) {
    throw new FramingError('wire bytes too short');
  }
  const view = new DataView(wire.buffer, wire.byteOffset, wire.byteLength);
  const frameBodyLen = view.getUint32(0, true);

  if (frameBodyLen === 0) {
    throw new FramingError('received zero-length frame');
  }

  if (wire.byteLength < LENGTH_SIZE + frameBodyLen) {
    throw new FramingError(
      `incomplete frame: expected ${frameBodyLen} bytes, got ${wire.byteLength - LENGTH_SIZE}`,
    );
  }

  const flags = wire[LENGTH_SIZE]!;
  const payload = wire.subarray(LENGTH_SIZE + FLAGS_SIZE, LENGTH_SIZE + frameBodyLen);
  return [payload, flags];
}

/** Wrap a promise with a timeout. */
async function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number | undefined,
  message: string,
): Promise<T> {
  if (timeoutMs === undefined) return promise;

  return Promise.race([
    promise,
    new Promise<never>((_, reject) =>
      setTimeout(() => reject(new FramingError(message)), timeoutMs),
    ),
  ]);
}
