/**
 * Structured logging for Aster services.
 *
 * Provides JSON and text format logging with request correlation,
 * sensitive field masking, and standard log fields.
 */

import { AsyncLocalStorage } from 'node:async_hooks';
import { currentTraceContext, currentTraceparent } from './tracing.js';
import { loadNative } from './native.js';

/** Request context stored via AsyncLocalStorage for correlation. */
export interface RequestContext {
  service: string;
  method: string;
  requestId: string;
  peer?: string;
}

const requestContext = new AsyncLocalStorage<RequestContext>();

/** Run a callback with request context attached to all logs within. */
export function withRequestContext<T>(ctx: RequestContext, fn: () => T): T {
  return requestContext.run(ctx, fn);
}

/** Get the current request context (if any). */
export function getRequestContext(): RequestContext | undefined {
  return requestContext.getStore();
}

type LogLevel = 'debug' | 'info' | 'warning' | 'error';

const LEVEL_ORDER: Record<LogLevel, number> = {
  debug: 0,
  info: 1,
  warning: 2,
  error: 3,
};

/** Sensitive fields that are masked in log output. */
const SENSITIVE_FIELDS = new Set([
  'secret_key', 'private_key', 'signing_key', 'signature', 'credential_json',
]);

/** Identifier fields that are truncated. */
const TRUNCATE_FIELDS = new Set([
  'root_pubkey', 'endpoint_id', 'node_id', 'contract_id', 'nonce',
]);

function maskValue(key: string, value: unknown, mask: boolean): unknown {
  if (!mask) return value;
  if (SENSITIVE_FIELDS.has(key)) return '***';
  if (TRUNCATE_FIELDS.has(key) && typeof value === 'string' && value.length > 12) {
    return `${value.slice(0, 7)}...${value.slice(-4)}`;
  }
  return value;
}

export interface LoggerOptions {
  format?: 'json' | 'text';
  level?: LogLevel;
  mask?: boolean;
  /**
   * Also export every record this logger prints through the Rust OTel logs
   * SDK (path B, see {@link emitLog}), with the current trace ids. Off by
   * default; needs `initObservability({ otlpLogs: true })` to export.
   */
  otlp?: boolean;
  /** Logger name on exported records. Default `app`. */
  name?: string;
}

/** Severity names {@link emitLog} accepts, mapped to `aster_log_level_t`. */
export type EmitLevel = 'error' | 'warning' | 'warn' | 'info' | 'debug' | 'trace';

const EMIT_LEVELS: Record<EmitLevel, number> = { error: 1, warning: 2, warn: 2, info: 3, debug: 4, trace: 5 };

/**
 * Path B of the observability design (3.4): write an application log record
 * into the Rust OpenTelemetry logs SDK, so it is exported over OTLP next to
 * the Rust records. A quiet no-op unless the pipeline exports logs
 * (`initObservability({ otlpLogs: true })` or `OTEL_LOGS_EXPORTER`). It never
 * blocks, and cannot loop back into the Rust log bridge (it emits no Rust
 * event). The record carries the current trace context unless `traceparent`
 * is given. Field values are rendered with `String()`. @group Logging
 */
export function emitLog(
  level: EmitLevel,
  message: string,
  fields?: Record<string, unknown>,
  options: { logger?: string; timestampMs?: number; traceparent?: string } = {},
): void {
  const code = EMIT_LEVELS[level];
  if (code === undefined) throw new RangeError(`unknown log level ${String(level)}`);
  let flat: Record<string, string> | undefined;
  if (fields) {
    flat = {};
    for (const [k, v] of Object.entries(fields)) flat[k] = typeof v === 'string' ? v : String(v);
  }
  loadNative().logEmit(
    code,
    options.logger ?? 'app',
    message,
    flat,
    options.timestampMs === undefined ? 0 : options.timestampMs * 1e6,
    options.traceparent ?? currentTraceparent(),
  );
}

/** Simple structured logger. For production, swap with pino. */
export class AsterLogger {
  private format: 'json' | 'text';
  private minLevel: number;
  private mask: boolean;
  private otlp: boolean;
  private name: string;

  constructor(opts?: LoggerOptions) {
    this.format = opts?.format ?? 'text';
    this.minLevel = LEVEL_ORDER[opts?.level ?? 'info'];
    this.mask = opts?.mask ?? true;
    this.otlp = opts?.otlp ?? false;
    this.name = opts?.name ?? 'app';
  }

  debug(msg: string, fields?: Record<string, unknown>): void { this.log('debug', msg, fields); }
  info(msg: string, fields?: Record<string, unknown>): void { this.log('info', msg, fields); }
  warning(msg: string, fields?: Record<string, unknown>): void { this.log('warning', msg, fields); }
  warn(msg: string, fields?: Record<string, unknown>): void { this.log('warning', msg, fields); }
  error(msg: string, fields?: Record<string, unknown>): void { this.log('error', msg, fields); }

  private log(level: LogLevel, msg: string, fields?: Record<string, unknown>): void {
    if (LEVEL_ORDER[level] < this.minLevel) return;

    if (this.otlp) {
      try {
        const masked: Record<string, unknown> = {};
        for (const [k, v] of Object.entries(fields ?? {})) masked[k] = maskValue(k, v, this.mask);
        emitLog(level, msg, masked, { logger: this.name });
      } catch {
        // exporting must never break the caller
      }
    }

    const ctx = getRequestContext();
    const entry: Record<string, unknown> = {
      ts: new Date().toISOString(),
      level,
      msg,
    };

    if (ctx) {
      entry.service = ctx.service;
      entry.method = ctx.method;
      entry.request_id = ctx.requestId;
      if (ctx.peer) entry.peer = maskValue('endpoint_id', ctx.peer, this.mask);
    }

    const trace = currentTraceContext();
    if (trace) {
      entry.trace_id = trace.traceId;
      entry.span_id = trace.spanId;
    }

    if (fields) {
      for (const [k, v] of Object.entries(fields)) {
        entry[k] = maskValue(k, v, this.mask);
      }
    }

    if (this.format === 'json') {
      console.log(JSON.stringify(entry));
    } else {
      const time = new Date().toLocaleTimeString('en-US', { hour12: false });
      const ctxStr = ctx ? ` [svc=${ctx.service} method=${ctx.method} req=${ctx.requestId.slice(0, 8)}]` : '';
      const fieldStr = fields ? ' ' + Object.entries(fields).map(([k, v]) => `${k}=${maskValue(k, v, this.mask)}`).join(' ') : '';
      console.log(`${time} ${level.toUpperCase().padEnd(7)} ${msg}${ctxStr}${fieldStr}`);
    }
  }
}

/** Create a logger from AsterConfig values. */
export function createLogger(opts?: LoggerOptions): AsterLogger {
  return new AsterLogger(opts);
}
