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
 * binding, on Node and Bun alike. Only the async stream I/O (reading the
 * length prefix and body off a QUIC stream) lives here, until RPC moves
 * onto native dispatch (plan T4).
 */

import { MAX_FRAME_SIZE, DEFAULT_FRAME_READ_TIMEOUT_S } from './limits.js';
import { loadNative } from './native.js';

// -- Stream protocol abstractions ---------------------------------------------

/** Minimal async send-stream interface (matches IrohSendStream from NAPI). */
export interface SendStream {
  writeAll(data: Uint8Array): Promise<void>;
}

/** Minimal async recv-stream interface (matches IrohRecvStream from NAPI). */
export interface RecvStream {
  readExact(n: number): Promise<Uint8Array>;
}

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

const LENGTH_SIZE = 4;

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

// -- Stream I/O ----------------------------------------------------------------

/**
 * Write one frame with the payload unchanged (envelope / control frames).
 */
export async function writeFrame(
  stream: SendStream,
  payload: Uint8Array,
  flags = 0,
): Promise<void> {
  await stream.writeAll(encodeFrame(payload, flags));
}

/**
 * Write one data frame, compressing the payload per Aster-SPEC §5.6.
 */
export async function writePayloadFrame(
  stream: SendStream,
  payload: Uint8Array,
  flags = 0,
): Promise<void> {
  await stream.writeAll(encodePayloadFrame(payload, flags));
}

/**
 * Read a single frame from a receive stream. The payload is returned as
 * sent; pass it through {@link payloadOf} to undo compression.
 *
 * @param stream - The QUIC receive stream.
 * @param timeoutS - Read timeout in seconds. Defaults to
 *   DEFAULT_FRAME_READ_TIMEOUT_S; pass 0 to disable.
 * @returns A [payload, flags] tuple, or null if the stream has ended cleanly.
 * @throws FramingError on wire-format violations (zero length, oversized frame).
 */
export async function readFrame(
  stream: RecvStream,
  timeoutS?: number,
): Promise<FrameResult | null> {
  const timeout = timeoutS ?? DEFAULT_FRAME_READ_TIMEOUT_S;
  const effectiveTimeout = timeout > 0 ? timeout * 1000 : undefined;

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
  if (lengthBytes.byteLength < LENGTH_SIZE) return null;

  const view = new DataView(lengthBytes.buffer, lengthBytes.byteOffset, lengthBytes.byteLength);
  const frameBodyLen = view.getUint32(0, true);
  if (frameBodyLen === 0) {
    throw new FramingError('received zero-length frame');
  }
  if (frameBodyLen > MAX_FRAME_SIZE) {
    throw new FramingError(`frame size ${frameBodyLen} exceeds maximum ${MAX_FRAME_SIZE}`);
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
    throw new FramingError(`incomplete frame: expected ${frameBodyLen} bytes`);
  }
  if (body.byteLength < frameBodyLen) {
    throw new FramingError(
      `incomplete frame: expected ${frameBodyLen} bytes, got ${body.byteLength}`,
    );
  }
  return [body.subarray(1), body[0]!];
}

/** Wrap a promise with a timeout. */
async function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number | undefined,
  message: string,
): Promise<T> {
  if (timeoutMs === undefined) return promise;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new FramingError(message)), timeoutMs);
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}
