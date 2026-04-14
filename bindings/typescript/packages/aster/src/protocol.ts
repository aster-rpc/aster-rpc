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
  callId = 0;               // int32 sequence number
  deadline = 0;             // int16 relative seconds, 0 = none
  serializationMode = 0;    // int8: XLANG=0, NATIVE=1, ROW=2, JSON=3
  metadataKeys: string[] = [];
  metadataValues: string[] = [];
  // Session identifier (multiplexed-streams, spec §6). 0 = stateless
  // SHARED pool stream; non-zero = stream belongs to the session with
  // this id on this (peer, connection). Client allocates monotonically
  // per connection. On the wire this is an int32, matching pyfory.
  sessionId = 0;

  constructor(init?: Partial<StreamHeader>) {
    if (init) Object.assign(this, init);
  }
}

/** Per-call header within a session stream (CALL flag). */
export class CallHeader {
  static readonly wireType = '_aster/CallHeader';

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
