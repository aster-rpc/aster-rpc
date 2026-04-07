/**
 * Wire-protocol types.
 *
 * Spec reference: S6.2 (StreamHeader), S6.4 (RpcStatus/trailer)
 *
 * These types are always serialized with Fory XLANG, regardless of the
 * service's negotiated serialization mode.
 */

/** First frame on every QUIC stream (HEADER flag). */
export class StreamHeader {
  /** Wire type tag for Fory XLANG registration. */
  static readonly wireType = '_aster/StreamHeader';

  service = '';
  method = '';
  version = 0;
  contractId = '';
  callId = '';
  deadlineEpochMs = 0;
  serializationMode = 0;
  metadataKeys: string[] = [];
  metadataValues: string[] = [];

  constructor(init?: Partial<StreamHeader>) {
    if (init) Object.assign(this, init);
  }
}

/** Per-call header within a session stream (CALL flag). */
export class CallHeader {
  static readonly wireType = '_aster/CallHeader';

  method = '';
  callId = '';
  deadlineEpochMs = 0;
  metadataKeys: string[] = [];
  metadataValues: string[] = [];

  constructor(init?: Partial<CallHeader>) {
    if (init) Object.assign(this, init);
  }
}

/** Trailing status frame (TRAILER flag). */
export class RpcStatus {
  static readonly wireType = '_aster/RpcStatus';

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
