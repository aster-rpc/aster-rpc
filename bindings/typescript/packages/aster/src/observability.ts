/**
 * Observability: the OpenTelemetry pipeline, Rust log records, and `/metrics`.
 *
 * Everything is configured and run in Rust (`aster-otel`); this module starts
 * it and delivers Rust's `tracing` events to a JavaScript log sink.
 *
 * ```ts
 * await initObservability({ serviceName: 'portal', logFilter: 'warn,aster=debug' });
 * setLogSink(pinoSink(logger));             // or winstonSink(...), or your own
 * // ...
 * await shutdownObservability();            // flushes; also done on 'beforeExit'
 * ```
 *
 * **Starting.** `AsterServer.start()` and `AsterClient.connect()` start the
 * pipeline from the environment (`OTEL_*`, `ASTER_*`) when you have not.
 * Call {@link initObservability} before them to configure it in code; a
 * different configuration afterwards is an error (the SDK globals are
 * process-wide).
 *
 * **Log delivery.** The bridge hands batches of records to your sink over a
 * non-blocking, weak native callback (a bounded queue: a stalled event loop
 * drops batches and counts them, see {@link logStats}). Under Bun, where that
 * callback can stall, records are drained by a short unref'd timer instead
 * ({@link LogDeliveryOptions.poll}); the same fallback engages in Node if the
 * native watchdog fires without any callback running.
 *
 * **Exit.** `process.on('beforeExit')` drains pending records and flushes the
 * exporters. `beforeExit` does not fire on an explicit `process.exit()`:
 * `await shutdownObservability()` (or `runtime.close()`) first.
 */

import type { RustLogRecord, LogStats, ObservabilityOptions } from '@aster-rpc/transport';
import { loadNative } from './native.js';

export type { RustLogRecord, LogStats, ObservabilityOptions };

/** Receives each Rust log record. Runs on the JavaScript thread. @group Logging */
export type LogSink = (record: RustLogRecord) => void;

/** Level names by `RustLogRecord.level` (1 error .. 5 trace). @group Logging */
export const LOG_LEVELS = ['', 'error', 'warn', 'info', 'debug', 'trace'] as const;

/** `HAS_TRACE`, `IN_CALL`, `SYNTHETIC` (drop report) and `AUDIT` bits of `RustLogRecord.flags`. @group Logging */
export const LogFlag = { HasTrace: 1, InCall: 2, Synthetic: 4, Audit: 8 } as const;

function fieldsText(r: RustLogRecord): string {
  const parts: string[] = [];
  for (const [k, v] of Object.entries(r.fields)) {
    if (k !== 'thread.name') parts.push(`${k}=${v}`);
  }
  return parts.length ? ` ${parts.join(' ')}` : '';
}

/**
 * The default sink: warn and error go to `console.warn` / `console.error`;
 * everything else is dropped. @group Logging
 */
export const consoleSink: LogSink = (r) => {
  if (r.level > 2) return;
  const line = `[aster] ${LOG_LEVELS[r.level]!.toUpperCase()} ${r.logger}: ${r.message}${fieldsText(r)}`;
  (r.level === 1 ? console.error : console.warn)(line);
};

/** The slice of a pino logger {@link pinoSink} needs. @group Logging */
export interface PinoLike {
  [level: string]: unknown;
}

/** The slice of a winston logger {@link winstonSink} needs. @group Logging */
export interface WinstonLike {
  log(entry: Record<string, unknown>): unknown;
}

function structured(r: RustLogRecord): Record<string, unknown> {
  const obj: Record<string, unknown> = { ...r.fields, logger: r.logger, aster_origin: 'rust' };
  if (r.flags & LogFlag.HasTrace) {
    obj.trace_id = r.traceId;
    obj.span_id = r.spanId;
  }
  return obj;
}

/**
 * Deliver Rust records to a pino logger at the matching level, with the
 * record's fields, `logger`, `aster_origin: "rust"` and, inside a traced call,
 * `trace_id` / `span_id`. Pino filters by its own level. @group Logging
 */
export function pinoSink(logger: PinoLike): LogSink {
  return (r) => {
    const fn = logger[LOG_LEVELS[r.level]!];
    if (typeof fn === 'function') (fn as (o: object, m: string) => void).call(logger, structured(r), r.message);
  };
}

const WINSTON_LEVELS = ['', 'error', 'warn', 'info', 'debug', 'silly'] as const;

/**
 * Deliver Rust records to a winston logger (`trace` maps to `silly`), with the
 * same fields as {@link pinoSink}. @group Logging
 */
export function winstonSink(logger: WinstonLike): LogSink {
  return (r) => {
    logger.log({ level: WINSTON_LEVELS[r.level]!, message: r.message, ...structured(r) });
  };
}

// ── pipeline state ──────────────────────────────────────────────────────────

let sink: LogSink = consoleSink;
let refs = 0;
let pending: Promise<void> | undefined;
let deliveryOptions: LogDeliveryOptions = {};
let pollTimer: ReturnType<typeof setInterval> | undefined;
let monitorTimer: ReturnType<typeof setInterval> | undefined;
let exitHooked = false;
let exitDone = false;

/** Options for {@link setLogDelivery}. @group Logging */
export interface LogDeliveryOptions {
  /**
   * Drain with a timer instead of the native callback. Default: `true` under
   * Bun, `false` elsewhere.
   */
  poll?: boolean;
  /** Poll interval in ms (default 250). */
  pollIntervalMs?: number;
  /** Most records per native batch (default 256). */
  maxBatch?: number;
  /**
   * Queued native calls (of 64) above which small batches are coalesced into
   * full ones instead of sent (default 32). `64` never coalesces: a batch the
   * bounded queue refuses is dropped and counted in `logStats().sinkDropped`.
   */
  holdAbove?: number;
}

function onBun(): boolean {
  return typeof (globalThis as { Bun?: unknown }).Bun !== 'undefined';
}

function dispatch(records: RustLogRecord[]): void {
  for (const r of records) {
    try {
      sink(r);
    } catch {
      // A throwing sink must not take the process (or the TSFN callback) down.
    }
  }
}

/** Drain whatever is queued in Rust into the sink now. Returns the count. @group Logging */
export function pollLogs(max = 256): number {
  const native = loadNative();
  let total = 0;
  for (;;) {
    const batch = native.logPoll(max);
    if (batch.length === 0) return total;
    total += batch.length;
    dispatch(batch);
    if (batch.length < max) return total;
  }
}

function stopTimers(): void {
  if (pollTimer) clearInterval(pollTimer);
  if (monitorTimer) clearInterval(monitorTimer);
  pollTimer = monitorTimer = undefined;
}

function startPolling(): void {
  if (pollTimer) return;
  loadNative().logSetPollMode(true);
  pollTimer = setInterval(() => {
    try {
      pollLogs();
    } catch {
      // The sink stopped under us (pipeline shut down).
    }
  }, deliveryOptions.pollIntervalMs ?? 250);
  pollTimer.unref();
}

/** Start delivering, replacing a stopped sink. No-op while one runs. */
function startDelivery(): void {
  const native = loadNative();
  if (native.logStats().running) return;
  stopTimers();
  const poll = deliveryOptions.poll ?? onBun();
  native.logSinkStart(dispatch, {
    maxBatch: deliveryOptions.maxBatch,
    holdAbove: deliveryOptions.holdAbove,
    poll,
  });
  if (poll) {
    startPolling();
    return;
  }
  // Node: a native watchdog nudge with no callback behind it for a whole
  // window means the TSFN is stalled; fall back to polling.
  let lastRescues = 0;
  let lastCallbacks = 0;
  monitorTimer = setInterval(() => {
    const s = native.logStats();
    if (!s.running) return;
    if (s.rescues > lastRescues && s.callbacks === lastCallbacks) {
      if (monitorTimer) clearInterval(monitorTimer);
      monitorTimer = undefined;
      startPolling();
      return;
    }
    lastRescues = s.rescues;
    lastCallbacks = s.callbacks;
  }, 1000);
  monitorTimer.unref();
}

async function exitHandler(): Promise<void> {
  if (exitDone || refs === 0) return;
  exitDone = true;
  try {
    pollLogs();
  } catch {
    // no sink
  }
  try {
    await flushObservability(2000);
  } catch {
    // best effort at exit
  }
}

/**
 * Start the pipeline, or join the running one. Idempotent calls with the same
 * configuration are fine; a different one fails. Opens the Rust log bridge
 * (unless `logBridge: false`) and starts delivering records to the current
 * sink (the {@link consoleSink} until {@link setLogSink}). Pair every call with
 * {@link shutdownObservability}. @group Observability
 */
export async function initObservability(options: ObservabilityOptions = {}): Promise<void> {
  if (pending) await pending.catch(() => undefined);
  const p = (async () => {
    const native = loadNative();
    await native.observabilityInit(options);
    refs++;
    if (options.logBridge !== false) startDelivery();
    if (!exitHooked) {
      exitHooked = true;
      process.on('beforeExit', () => {
        void exitHandler();
      });
    }
    exitDone = false;
  })();
  pending = p;
  try {
    await p;
  } finally {
    if (pending === p) pending = undefined;
  }
}

/**
 * Start the pipeline from the environment unless something started it
 * (the implicit init of `AsterServer.start()` / `AsterClient.connect()`).
 * @group Observability
 */
export async function ensureObservability(): Promise<void> {
  if (pending) await pending.catch(() => undefined);
  if (refs > 0) return;
  await initObservability({});
}

/**
 * Release one {@link initObservability} reference. The last reference flushes
 * and stops the exporters and closes the log bridge. Resolves `false` when
 * there was nothing to release. @group Observability
 */
export async function shutdownObservability(timeoutMs = 2000): Promise<boolean> {
  if (pending) await pending.catch(() => undefined);
  if (refs === 0) return false;
  const native = loadNative();
  try {
    pollLogs();
  } catch {
    // no sink
  }
  const released = await native.observabilityShutdown(timeoutMs);
  refs--;
  if (refs === 0) stopTimers();
  return released;
}

/** Flush spans, metrics and OTLP logs now (bounded by `timeoutMs`). @group Observability */
export async function flushObservability(timeoutMs = 2000): Promise<void> {
  await loadNative().observabilityFlush(timeoutMs);
}

/**
 * Set where Rust's log records go; `null` restores the {@link consoleSink}.
 * Takes effect for records not yet delivered. @group Logging
 */
export function setLogSink(next: LogSink | null): void {
  sink = next ?? consoleSink;
}

/**
 * Tune delivery before {@link initObservability} (or restart it: if a sink
 * is running it is restarted with the new options). @group Logging
 */
export function setLogDelivery(options: LogDeliveryOptions): void {
  deliveryOptions = { ...options };
  const native = loadNative();
  if (refs > 0 && native.logStats().running) {
    pollLogs();
    native.logSinkStop();
    startDelivery();
  }
}

/**
 * Change which Rust events reach the sink: `EnvFilter` directives such as
 * `warn,aster::rpc=debug`. `aster::audit=info` is always kept. The cost of a
 * disabled event in Rust is one atomic load. @group Logging
 */
export function setLogFilter(directives: string): void {
  loadNative().logSetFilter(directives);
}

/** Delivery counters: records forwarded, dropped by the bounded queue, polled. @group Logging */
export function logStats(): LogStats {
  return loadNative().logStats();
}

/**
 * The OTel Prometheus scrape (what `/metrics` serves), or `null` when no
 * pull reader exists (no `HealthServer` has started, or `OTEL_SDK_DISABLED`).
 * @group Observability
 */
export function prometheusText(): string | null {
  return loadNative().prometheusText() ?? null;
}
