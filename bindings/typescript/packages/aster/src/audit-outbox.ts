/** Explicit durable audit submission; storage and delivery run in shared Rust. */
import type { NativeAuditOutbox } from '@aster-rpc/transport';
import { loadNative } from './native.js';

export type AuditOutboxErrorKind =
  | 'invalid_config' | 'invalid_event' | 'event_too_large' | 'storage_full'
  | 'storage' | 'commit_uncertain' | 'queue_overloaded' | 'maintenance'
  | 'closed' | 'worker_failed' | 'timeout' | 'delivery_stalled'
  | 'disposition' | 'not_found' | 'destination_mismatch' | 'integrity_conflict'
  | 'stale_delivery';

export class AuditOutboxError extends Error {
  constructor(readonly kind: AuditOutboxErrorKind, message: string) {
    super(message);
    this.name = 'AuditOutboxError';
  }
}

export interface AuditDestination {
  endpoint: string;
  receiverIdentity?: string;
  authorization?: string;
}

export interface AuditOutboxConfig {
  directory: string;
  destination: AuditDestination;
  /** Default 256 MiB. Exact unsigned 64-bit integer. */
  maxRetainedBytes?: bigint;
  /** Default 64 KiB. */
  maxEventBytes?: number;
}

export type AuditJson = null | boolean | number | string | AuditJson[] | { [key: string]: AuditJson };
export interface AuditEvent {
  timestampMs: bigint;
  kind: string;
  payload: { [key: string]: AuditJson };
}

export interface AuditDeliveryError {
  kind: 'transport' | 'protocol' | 'credentials' | 'rejected' | 'integrity' | 'configuration';
  httpStatus: bigint | null;
  sequence: bigint;
  immediateFlushFailure: boolean;
  message: string;
}

export interface AuditOutboxStatus {
  outboxId: string;
  storageRecovered: boolean;
  committedSequence: bigint;
  pendingRecords: bigint;
  pendingBytes: bigint;
  deadLetterRecords: bigint;
  deadLetterBytes: bigint;
  dispositionReceipts: bigint;
  dispositionBytes: bigint;
  retainedBytes: bigint;
  maxRetainedBytes: bigint;
  queuedRecords: bigint;
  queuedBytes: bigint;
  oldestAgeMs: bigint | null;
  accepting: boolean;
  ready: boolean;
  deliveryState: string;
  lastDeliveryError: AuditDeliveryError | null;
  storageWorkerAlive: boolean;
  deliveryWorkerAlive: boolean;
  maintenance: boolean;
  maintenanceDurationMs: bigint | null;
  closed: boolean;
  durableCommits: bigint;
  lastGroupSize: bigint;
}

export interface AuditDispositionReceipt {
  eventId: string;
  action: string;
  timestampMs: bigint;
  reason: string;
  operatorReference: string;
  destinationFingerprint: string;
  rejection: AuditDeliveryError | null;
}

export interface AuditQuarantineExport {
  eventId: string;
  envelopeBase64: string;
  pending: boolean;
  receipts: AuditDispositionReceipt[];
}

/**
 * One event the receiver acknowledged as durably stored. The outbox writes it
 * in the same local transaction that deletes the event, so an event is either
 * still pending or in this log (until pruned by its count / age bounds).
 * `cursor` increases with each acknowledgement and is never reused: pass the
 * last one seen to continue reading.
 */
export interface AuditAcknowledgement {
  cursor: bigint;
  eventId: string;
  sequence: bigint;
  /** The batch digest the receiver echoed in its durable acknowledgement. */
  batchId: string;
  /** Local Unix milliseconds when the acknowledgement committed. */
  acknowledgedAtMs: bigint;
}

/** The largest page one acknowledgement read returns. */
export const AUDIT_ACKNOWLEDGEMENT_PAGE_MAX = 1000;
/** The longest wait {@link AuditOutbox.waitAcknowledgements} accepts (one hour). */
const ACK_WAIT_MAX_MS = 3_600_000;
/** How long each read of {@link AuditOutbox.watchAcknowledgements} waits. */
const WATCH_WAIT_MS = 30_000;

function pageLimit(limit: number): number {
  if (!Number.isInteger(limit) || limit < 1 || limit > AUDIT_ACKNOWLEDGEMENT_PAGE_MAX) {
    throw new AuditOutboxError('invalid_event', 'acknowledgement limit must be between 1 and 1000');
  }
  return limit;
}

function aborted(signal: AbortSignal): Promise<never> {
  return new Promise((_, reject) => {
    const fail = () => reject(signal.reason ?? new Error('aborted'));
    if (signal.aborted) fail();
    else signal.addEventListener('abort', fail, { once: true });
  });
}

function unsigned(value: bigint, kind: AuditOutboxErrorKind = 'invalid_event'): string {
  if (typeof value !== 'bigint' || value < 0n || value > 0xffff_ffff_ffff_ffffn) {
    throw new AuditOutboxError(kind, 'expected an unsigned 64-bit bigint');
  }
  return value.toString();
}

function encode(value: unknown): string {
  try {
    const result = JSON.stringify(value, (_key, v: unknown) => {
      if (typeof v === 'number' && (!Number.isFinite(v) || (Number.isInteger(v) && !Number.isSafeInteger(v)))) {
        throw new Error('invalid JSON number');
      }
      if (v === undefined || typeof v === 'function' || typeof v === 'symbol') {
        throw new Error('invalid JSON value');
      }
      return v;
    });
    if (result === undefined) throw new Error('invalid JSON value');
    return result;
  } catch {
    throw new AuditOutboxError('invalid_event', 'audit arguments must contain valid JSON values');
  }
}

function destination(value: AuditDestination) {
  return {
    endpoint: value.endpoint,
    ...(value.receiverIdentity !== undefined ? { receiver_identity: value.receiverIdentity } : {}),
    ...(value.authorization !== undefined ? { authorization: value.authorization } : {}),
  };
}

// Bridge metadata only. The original envelope stays opaque base64.
const INTEGER_FIELDS = new Set([
  'committed_sequence', 'pending_records', 'pending_bytes', 'dead_letter_records',
  'dead_letter_bytes', 'disposition_receipts', 'disposition_bytes', 'retained_bytes',
  'max_retained_bytes', 'queued_records', 'queued_bytes', 'oldest_age_ms',
  'maintenance_duration_ms', 'durable_commits', 'last_group_size', 'http_status',
  'sequence', 'timestamp_ms', 'cursor', 'acknowledged_at_ms',
]);
function metadata(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(metadata);
  if (value === null || typeof value !== 'object') return value;
  return Object.fromEntries(Object.entries(value).map(([key, v]) => [
    key.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase()),
    INTEGER_FIELDS.has(key) && v !== null ? BigInt(v as string) : metadata(v),
  ]));
}

async function call<T>(operation: () => Promise<T>): Promise<T> {
  try { return await operation(); } catch (error) {
    if (error instanceof AuditOutboxError) throw error;
    const message = error instanceof Error ? error.message : String(error);
    const tagged = /^\[audit_outbox:([a-z_]+)\] ([\s\S]*)$/.exec(message);
    if (tagged) throw new AuditOutboxError(tagged[1] as AuditOutboxErrorKind, tagged[2]);
    throw error;
  }
}

/**
 * Submission success means durable local acceptance. Delivery is at least once.
 * Discarding a submission promise does not retract accepted work. Close keeps
 * pending records and does not require the receiver to be available.
 */
export class AuditOutbox implements AsyncDisposable {
  private constructor(private readonly native: NativeAuditOutbox) {}

  static async open(config: AuditOutboxConfig): Promise<AuditOutbox> {
    if (config.maxEventBytes !== undefined && (!Number.isSafeInteger(config.maxEventBytes) || config.maxEventBytes < 0)) {
      throw new AuditOutboxError('invalid_config', 'maxEventBytes must be a nonnegative safe integer');
    }
    const json = encode({
      directory: config.directory,
      destination: destination(config.destination),
      ...(config.maxRetainedBytes !== undefined ? { max_retained_bytes: unsigned(config.maxRetainedBytes, 'invalid_config') } : {}),
      ...(config.maxEventBytes !== undefined ? { max_event_bytes: config.maxEventBytes } : {}),
    });
    return new AuditOutbox(await call(() => loadNative().NativeAuditOutbox.open(json)));
  }

  /** Share workers, directory lock and capacity with this handle. */
  clone(): AuditOutbox { return new AuditOutbox(this.native.cloneHandle()); }

  async submit(event: AuditEvent): Promise<string> {
    const json = encode({ timestamp_ms: unsigned(event.timestampMs), kind: event.kind, payload: event.payload });
    return call(() => this.native.submit(json));
  }

  async status(): Promise<AuditOutboxStatus> {
    return metadata(JSON.parse(await call(() => this.native.status()))) as AuditOutboxStatus;
  }

  /** Wait for the committed prefix; timeoutMs must be finite and nonnegative. */
  async flush(timeoutMs: number): Promise<void> {
    if (!Number.isFinite(timeoutMs) || timeoutMs < 0) throw new AuditOutboxError('invalid_event', 'invalid flush timeout');
    await call(() => this.native.flush(timeoutMs));
  }

  async close(): Promise<void> { await call(() => this.native.close()); }
  async reconfigure(value: AuditDestination): Promise<void> {
    const json = encode(destination(value));
    await call(() => this.native.reconfigure(json));
  }
  async quarantine(eventId: string, reason: string, operatorReference: string): Promise<void> {
    await call(() => this.native.quarantine(eventId, reason, operatorReference));
  }
  async export(eventId: string): Promise<AuditQuarantineExport> {
    return metadata(JSON.parse(await call(() => this.native.export(eventId)))) as AuditQuarantineExport;
  }
  async replay(eventId: string): Promise<void> { await call(() => this.native.replay(eventId)); }

  /**
   * Up to `limit` (1..1000) logged acknowledgements with a cursor greater
   * than `after`, oldest first. Pass `0n` to start at the oldest retained
   * entry. Pruning by the configured bounds shows as a gap in cursors, never
   * a reordering.
   */
  async acknowledgements(after: bigint = 0n, limit: number = AUDIT_ACKNOWLEDGEMENT_PAGE_MAX): Promise<AuditAcknowledgement[]> {
    const from = unsigned(after);
    const max = pageLimit(limit);
    return metadata(JSON.parse(await call(() => this.native.acknowledgements(from, max)))) as AuditAcknowledgement[];
  }

  /**
   * {@link acknowledgements}, but when nothing follows `after` yet it waits
   * up to `waitMs` (at most one hour) for the next acknowledgement. Resolves
   * with an empty page when the wait ends first.
   */
  async waitAcknowledgements(after: bigint, limit: number, waitMs: number): Promise<AuditAcknowledgement[]> {
    const from = unsigned(after);
    const max = pageLimit(limit);
    if (!Number.isFinite(waitMs) || waitMs < 0 || waitMs > ACK_WAIT_MAX_MS) {
      throw new AuditOutboxError('invalid_event', 'acknowledgement wait must be between 0 and one hour');
    }
    return metadata(JSON.parse(await call(() => this.native.acknowledgements(from, max, waitMs)))) as AuditAcknowledgement[];
  }

  /**
   * Every acknowledgement with a cursor greater than `after`, as they commit,
   * until `signal` aborts (the iteration then ends), the loop breaks, or the
   * outbox fails (the iteration throws). Lossless while entries are
   * retained: record the last `cursor` to resume after a restart.
   *
   * ```ts
   * for await (const ack of outbox.watchAcknowledgements(lastCursor, { signal })) {
   *   markDelivered(ack.eventId);
   *   lastCursor = ack.cursor;
   * }
   * ```
   */
  async *watchAcknowledgements(after: bigint = 0n, options: { signal?: AbortSignal } = {}): AsyncGenerator<AuditAcknowledgement, void, undefined> {
    unsigned(after);
    // Breaking out of the loop aborts this too, so a pending wait stops
    // holding the generator (the native wait itself ends on its timeout).
    const stop = new AbortController();
    const onAbort = () => stop.abort(options.signal?.reason);
    options.signal?.addEventListener('abort', onAbort, { once: true });
    if (options.signal?.aborted) stop.abort(options.signal.reason);
    const halted = aborted(stop.signal);
    halted.catch(() => {});
    let cursor = after;
    try {
      while (!stop.signal.aborted) {
        let page: AuditAcknowledgement[];
        try {
          page = await Promise.race([
            this.waitAcknowledgements(cursor, AUDIT_ACKNOWLEDGEMENT_PAGE_MAX, WATCH_WAIT_MS),
            halted,
          ]);
        } catch (error) {
          if (stop.signal.aborted) return;
          throw error;
        }
        for (const ack of page) {
          if (stop.signal.aborted) return;
          yield ack;
          cursor = ack.cursor;
        }
      }
    } finally {
      options.signal?.removeEventListener('abort', onAbort);
      stop.abort();
    }
  }
  async [Symbol.asyncDispose](): Promise<void> { await this.close(); }
}
