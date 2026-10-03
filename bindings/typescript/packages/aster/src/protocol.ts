/**
 * RPC envelope types and their wire codec.
 *
 * Spec reference: Aster-SPEC §6.2 (StreamHeader), §6.4 (RpcStatus),
 * §6.9 (AEv1 envelope format).
 *
 * The envelope is **not** Fory-encoded. `StreamHeader`, `CallHeader` and
 * `RpcStatus` use the fixed AEv1 format implemented once in Rust core
 * (`core/src/envelope.rs`); the functions below call it through the NAPI
 * addon, so TypeScript, Python, Rust and the FFI bindings produce the same
 * bytes. Only call payloads go through Fory.
 */

import { loadNative } from './native.js';

/** First frame on every QUIC stream (HEADER flag). */
export class StreamHeader {
  service = '';
  method = '';
  version = 0;
  callId = 0;               // int32 sequence number
  deadline = 0;             // int16 relative seconds, 0 = none
  serializationMode = 0;    // int8: XLANG=0 (NATIVE=1, ROW=2 reserved)
  metadataKeys: string[] = [];
  metadataValues: string[] = [];
  /**
   * Session identifier (multiplexed streams, spec §6). 0 = stateless
   * SHARED pool stream; non-zero = stream belongs to the session with
   * this id on this (peer, connection).
   */
  sessionId = 0;

  constructor(init?: Partial<StreamHeader>) {
    if (init) Object.assign(this, init);
  }
}

/** Per-call header within a session stream (CALL flag). */
export class CallHeader {
  method = '';
  callId = 0;               // int32 sequence number
  deadline = 0;             // int16 relative seconds, 0 = none
  metadataKeys: string[] = [];
  metadataValues: string[] = [];

  constructor(init?: Partial<CallHeader>) {
    if (init) Object.assign(this, init);
  }
}

/** Trailing status frame (TRAILER flag). */
export class RpcStatus {
  code = 0;
  message = '';
  detailKeys: string[] = [];
  detailValues: string[] = [];

  constructor(init?: Partial<RpcStatus>) {
    if (init) Object.assign(this, init);
  }

  /** Convert detail arrays to a key-value record. */
  get details(): Record<string, string> {
    const result: Record<string, string> = {};
    for (let i = 0; i < this.detailKeys.length; i++) {
      result[this.detailKeys[i]!] = this.detailValues[i] ?? '';
    }
    return result;
  }
}

// -- AEv1 codec (Rust core via NAPI) -------------------------------------------

function bytes(b: Uint8Array): Uint8Array {
  return new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
}

/** Encode a {@link StreamHeader} to AEv1 bytes (payload of a HEADER frame). */
export function encodeStreamHeader(h: Partial<StreamHeader>): Uint8Array {
  return bytes(loadNative().encodeStreamHeader({
    service: h.service ?? '',
    method: h.method ?? '',
    version: h.version ?? 0,
    callId: h.callId ?? 0,
    deadline: h.deadline ?? 0,
    serializationMode: h.serializationMode ?? 0,
    metadataKeys: h.metadataKeys ?? [],
    metadataValues: h.metadataValues ?? [],
    sessionId: h.sessionId ?? 0,
  }));
}

/** Decode AEv1 bytes to a {@link StreamHeader}. Throws on malformed input. */
export function decodeStreamHeader(data: Uint8Array): StreamHeader {
  return new StreamHeader(loadNative().decodeStreamHeader(data));
}

/** Encode a {@link CallHeader} to AEv1 bytes (payload of a CALL frame). */
export function encodeCallHeader(h: Partial<CallHeader>): Uint8Array {
  return bytes(loadNative().encodeCallHeader({
    method: h.method ?? '',
    callId: h.callId ?? 0,
    deadline: h.deadline ?? 0,
    metadataKeys: h.metadataKeys ?? [],
    metadataValues: h.metadataValues ?? [],
  }));
}

/** Decode AEv1 bytes to a {@link CallHeader}. Throws on malformed input. */
export function decodeCallHeader(data: Uint8Array): CallHeader {
  return new CallHeader(loadNative().decodeCallHeader(data));
}

/** Encode an {@link RpcStatus} to AEv1 bytes (payload of a TRAILER frame). */
export function encodeRpcStatus(s: Partial<RpcStatus>): Uint8Array {
  return bytes(loadNative().encodeRpcStatus({
    code: s.code ?? 0,
    message: s.message ?? '',
    detailKeys: s.detailKeys ?? [],
    detailValues: s.detailValues ?? [],
  }));
}

/** Decode AEv1 bytes to an {@link RpcStatus}. Throws on malformed input. */
export function decodeRpcStatus(data: Uint8Array): RpcStatus {
  return new RpcStatus(loadNative().decodeRpcStatus(data));
}
