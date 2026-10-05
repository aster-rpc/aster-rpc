/**
 * Tracing: binding spans and the trace-context carrier.
 *
 * Spans live in the Rust SDK (`aster-otel`), so they are exported with the
 * core `rpc` spans, share the resource and follow the same sampling decision.
 * This module is a thin layer over the native `span*` calls plus an
 * `AsyncLocalStorage` carrier that holds the current span context:
 *
 * - an inbound call runs with the carrier set to the call's server span
 *   (`DispatchEvent.traceparent`), so binding spans and logs parent on it;
 * - {@link tracing.withSpan} makes its span the current one for the callback;
 * - an outbound call made inside the carrier sends it as `traceparent` (and
 *   `baggage`) metadata, which Rust parents the client span on.
 *
 * The four primitives of the design are {@link currentTraceId},
 * {@link currentSpanId}, {@link currentBaggage} and {@link withTraceContext};
 * {@link tracingMixin} (pino) and {@link winstonTracingFormat} wrap them.
 *
 * Without an initialised pipeline (or with `OTEL_SDK_DISABLED`) spans are
 * no-ops: callbacks still run and the carrier is left as it was.
 */

import { AsyncLocalStorage } from 'node:async_hooks';
import { loadNative, requireCjs } from './native.js';

/** The current span context. @group Tracing */
export interface TraceContext {
  /** 32 lowercase hex characters. */
  traceId: string;
  /** 16 lowercase hex characters. */
  spanId: string;
  /** W3C trace flags; bit 0 = sampled. */
  traceFlags: number;
  /** W3C baggage members visible to this context (never put secrets here). */
  baggage?: Readonly<Record<string, string>>;
}

const carrier = new AsyncLocalStorage<TraceContext>();

const TRACEPARENT_RE = /^00-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$/;

/** Parse a W3C `traceparent` (version 00). `undefined` when malformed. @group Tracing */
export function parseTraceparent(
  value: string | null | undefined,
  baggage?: Readonly<Record<string, string>>,
): TraceContext | undefined {
  if (!value) return undefined;
  const m = TRACEPARENT_RE.exec(value);
  if (!m) return undefined;
  if (/^0+$/.test(m[1]!) || /^0+$/.test(m[2]!)) return undefined;
  const ctx: TraceContext = { traceId: m[1]!, spanId: m[2]!, traceFlags: parseInt(m[3]!, 16) };
  if (baggage) ctx.baggage = baggage;
  return ctx;
}

/** Format a context as a W3C `traceparent`. @group Tracing */
export function formatTraceparent(ctx: Pick<TraceContext, 'traceId' | 'spanId' | 'traceFlags'>): string {
  return `00-${ctx.traceId}-${ctx.spanId}-${(ctx.traceFlags & 0xff).toString(16).padStart(2, '0')}`;
}

/** The context of the running call or span, if any. @group Tracing */
export function currentTraceContext(): TraceContext | undefined {
  return carrier.getStore();
}

/** Trace id of the current context (32 hex characters), if any. @group Tracing */
export function currentTraceId(): string | undefined {
  return carrier.getStore()?.traceId;
}

/** Span id of the current context (16 hex characters), if any. @group Tracing */
export function currentSpanId(): string | undefined {
  return carrier.getStore()?.spanId;
}

/** Baggage of the current context (empty when none). @group Tracing */
export function currentBaggage(): Readonly<Record<string, string>> {
  return carrier.getStore()?.baggage ?? {};
}

/** The current context as a `traceparent`, if any. @group Tracing */
export function currentTraceparent(): string | undefined {
  const c = carrier.getStore();
  return c ? formatTraceparent(c) : undefined;
}

/**
 * Run `fn` with `ctx` as the current trace context: binding spans started
 * inside parent on it, logs carry its ids, outbound calls propagate it.
 * `ctx` is a {@link TraceContext} or a `traceparent` string (with optional
 * `baggage`). A malformed string runs `fn` unchanged. @group Tracing
 */
export function withTraceContext<T>(
  ctx: TraceContext | string,
  fn: () => T,
  baggage?: Readonly<Record<string, string>>,
): T {
  const resolved = typeof ctx === 'string' ? parseTraceparent(ctx, baggage) : baggage ? { ...ctx, baggage } : ctx;
  return resolved ? carrier.run(resolved, fn) : fn();
}

/**
 * Metadata keys that carry the current context to a callee (W3C
 * `traceparent` and `baggage`), or `{}` when there is none. Spread into an
 * outbound call's metadata; the client does it for Aster calls.
 * @group Tracing
 */
export function traceMetadata(): Record<string, string> {
  const c = carrier.getStore();
  if (!c) return {};
  const out: Record<string, string> = { traceparent: formatTraceparent(c) };
  if (c.baggage) {
    const members = Object.entries(c.baggage).map(
      ([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`,
    );
    if (members.length) out.baggage = members.join(',');
  }
  return out;
}

/** Span kinds. @group Tracing */
export type SpanKind = 'internal' | 'server' | 'client' | 'producer' | 'consumer';

const KIND_CODE: Record<SpanKind, number> = { internal: 0, server: 1, client: 2, producer: 3, consumer: 4 };

/** Span attribute values are stringified. @group Tracing */
export type SpanAttributes = Record<string, string | number | boolean>;

function stringify(attrs: SpanAttributes | undefined): Record<string, string> | undefined {
  if (!attrs) return undefined;
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(attrs)) out[k] = String(v);
  return out;
}

/** Options for {@link tracing.span}. @group Tracing */
export interface SpanOptions {
  attributes?: SpanAttributes;
  kind?: SpanKind;
}

/** A live binding span. End it exactly once (more calls are ignored). @group Tracing */
export class Span {
  #handle: number;
  #ended = false;
  /** This span's context; `undefined` for a no-op span (no pipeline). */
  readonly context: TraceContext | undefined;

  /** @internal */
  constructor(handle: number, context: TraceContext | undefined) {
    this.#handle = handle;
    this.context = context;
  }

  setAttribute(key: string, value: string | number | boolean): this {
    return this.setAttributes({ [key]: value });
  }

  setAttributes(attrs: SpanAttributes): this {
    if (!this.#ended) loadNative().spanSetAttributes(this.#handle, stringify(attrs)!);
    return this;
  }

  addEvent(name: string, attrs?: SpanAttributes): this {
    if (!this.#ended) loadNative().spanAddEvent(this.#handle, name, stringify(attrs));
    return this;
  }

  /** `ok`, or `error` with a message. */
  setStatus(code: 'unset' | 'ok' | 'error', message?: string): this {
    if (!this.#ended) {
      loadNative().spanSetStatus(this.#handle, code === 'unset' ? 0 : code === 'ok' ? 1 : 2, message);
    }
    return this;
  }

  /** An `exception` event plus status error. */
  recordError(error: unknown): this {
    if (this.#ended) return this;
    const e = error instanceof Error ? error : new Error(String(error));
    loadNative().spanRecordError(this.#handle, e.name, e.message);
    return this;
  }

  /** End the span and free its native handle. Idempotent. */
  end(): void {
    if (this.#ended) return;
    this.#ended = true;
    try {
      loadNative().spanEnd(this.#handle);
    } catch {
      // The pipeline shut down first: its teardown already ended the span.
    }
  }

  [Symbol.dispose](): void {
    this.end();
  }
}

/**
 * Start a span under the current context (a root when there is none). It does
 * **not** become the current context: use {@link tracing.withSpan} for that,
 * or run code in `withTraceContext(span.context, …)`. The caller ends it.
 * @group Tracing
 */
export function span(name: string, options: SpanOptions = {}): Span {
  const native = loadNative();
  const handle = native.spanStart(
    name,
    KIND_CODE[options.kind ?? 'internal'],
    currentTraceparent() ?? null,
    stringify(options.attributes),
  );
  const cur = carrier.getStore();
  const tp = native.spanContext(handle);
  const parsed = tp ? parseTraceparent(tp, cur?.baggage) : undefined;
  return new Span(handle, parsed);
}

function runInSpan<T>(s: Span, fn: (span: Span) => T): T {
  const call = () => fn(s);
  let result: T;
  try {
    result = s.context ? carrier.run(s.context, call) : call();
  } catch (e) {
    s.recordError(e);
    s.end();
    throw e;
  }
  if (result && typeof (result as { then?: unknown }).then === 'function') {
    return (result as unknown as Promise<unknown>).then(
      (v) => {
        s.end();
        return v;
      },
      (e) => {
        s.recordError(e);
        s.end();
        throw e;
      },
    ) as unknown as T;
  }
  s.end();
  return result;
}

type SpanFn<T> = (span: Span) => T;

/**
 * Run `fn` inside a span that is the current context for the callback; the
 * span ends when `fn` returns (or, for a promise, settles) and records a
 * thrown / rejected error. @group Tracing
 */
function withSpan<T>(name: string, fn: SpanFn<T>): T;
function withSpan<T>(name: string, attributes: SpanAttributes | SpanOptions | undefined, fn: SpanFn<T>): T;
function withSpan<T>(name: string, a: unknown, b?: SpanFn<T>): T {
  const fn = (typeof a === 'function' ? a : b) as SpanFn<T>;
  const opts = typeof a === 'function' ? undefined : (a as SpanAttributes | SpanOptions | undefined);
  return runInSpan(span(name, toOptions(opts)), fn);
}

/** {@link tracing.withSpan} for an async callback. @group Tracing */
async function withSpanAsync<T>(name: string, fn: SpanFn<Promise<T>>): Promise<T>;
async function withSpanAsync<T>(
  name: string,
  attributes: SpanAttributes | SpanOptions | undefined,
  fn: SpanFn<Promise<T>>,
): Promise<T>;
async function withSpanAsync<T>(name: string, a: unknown, b?: SpanFn<Promise<T>>): Promise<T> {
  const fn = (typeof a === 'function' ? a : b) as SpanFn<Promise<T>>;
  const opts = typeof a === 'function' ? undefined : (a as SpanAttributes | SpanOptions | undefined);
  return runInSpan(span(name, toOptions(opts)), fn);
}

/** `{ kind?, attributes? }` is options; anything else is an attribute map. */
function toOptions(v: SpanAttributes | SpanOptions | undefined): SpanOptions {
  if (!v) return {};
  const keys = Object.keys(v);
  const looksLikeOptions =
    keys.length > 0 &&
    keys.every((k) => k === 'kind' || k === 'attributes') &&
    (typeof (v as SpanOptions).attributes === 'object' || typeof (v as SpanOptions).kind === 'string');
  return looksLikeOptions ? (v as SpanOptions) : { attributes: v as SpanAttributes };
}

/** The tracing namespace of the design: `tracing.withSpan(...)`. @group Tracing */
export const tracing = {
  span,
  withSpan,
  withSpanAsync,
  withTraceContext,
  currentTraceId,
  currentSpanId,
  currentBaggage,
  currentTraceContext,
  traceMetadata,
};

// ── logging-framework helpers ───────────────────────────────────────────────

/**
 * pino `mixin`: adds `trace_id` and `span_id` of the current context to every
 * line. `const log = pino({ mixin: tracingMixin })`. @group Logging
 */
export function tracingMixin(): Record<string, string> {
  const c = carrier.getStore();
  return c ? { trace_id: c.traceId, span_id: c.spanId } : {};
}

/** The slice of winston's `format` factory this helper needs. @group Logging */
export interface WinstonFormatFactory {
  (transform: (info: Record<string, unknown>) => Record<string, unknown> | false): unknown;
}

/**
 * winston format adding `trace_id` and `span_id`:
 * `winston.format.combine(winstonTracingFormat(), winston.format.json())`.
 * winston is an optional peer: it is resolved when this is called, or pass
 * its `format` factory explicitly. @group Logging
 */
export function winstonTracingFormat(format?: WinstonFormatFactory): unknown {
  const factory: WinstonFormatFactory = format ?? requireCjs('winston').format;
  return factory((info) => {
    const c = carrier.getStore();
    if (c) {
      info.trace_id = c.traceId;
      info.span_id = c.spanId;
    }
    return info;
  });
}
