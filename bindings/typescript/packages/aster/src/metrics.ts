/**
 * Custom metrics over the Rust OpenTelemetry pipeline.
 *
 * The built-in RPC, admission, connection and transport metrics are recorded
 * in Rust (`aster.rpc.server.*`, `aster.admission.*`, ...); this module is for
 * the application's own instruments. They live on the same `MeterProvider`
 * (scope `aster.app`), so they reach the OTLP exporter and `/metrics` with the
 * built-ins.
 *
 * ```ts
 * const created = metrics.counter('portal.workspace.created', {
 *   description: 'Workspaces created', unit: '1',
 * });
 * created.add(1, { recipe: 'node' });
 * ```
 *
 * Instruments may be declared before observability starts: a handle that a
 * pipeline shutdown freed is declared again on the next record. Recording
 * with no pipeline is a no-op in Rust.
 */

import { loadNative } from './native.js';

/** Instrument declaration options. @group Metrics */
export interface InstrumentOptions {
  description?: string;
  /** UCUM unit (`'1'`, `'ms'`, `'By'`). */
  unit?: string;
  /**
   * Attribute keys merged in from the current call's extractors. Accepted
   * and stored; merging is not active yet (Rust per-call cache, later step).
   */
  autoAttrs?: string[];
  /** Series limit override; `0`/unset = the SDK default (2000). Not active yet. */
  cardinalityLimit?: number;
}

/** Attribute values are stringified. @group Metrics */
export type Attributes = Record<string, string | number | boolean>;

const KIND_COUNTER = 1;
const KIND_UP_DOWN = 2;
const KIND_HISTOGRAM = 3;
const KIND_GAUGE = 4;

function strings(attrs: Attributes | undefined): Record<string, string> | undefined {
  if (!attrs) return undefined;
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(attrs)) out[k] = String(v);
  return out;
}

function isGone(e: unknown): boolean {
  return e instanceof Error && /no such (instrument|attribute set) handle/.test(e.message);
}

/**
 * An interned attribute set: build it once, pass it to every record on a hot
 * path to skip rebuilding the attribute list. @group Metrics
 */
export class AttributeSet {
  readonly attributes: Readonly<Record<string, string>>;
  #handle = 0;

  constructor(attributes: Attributes) {
    this.attributes = Object.freeze(strings(attributes) ?? {});
  }

  /** @internal The native handle, declared on first use and after a shutdown. */
  handle(fresh = false): number {
    if (this.#handle === 0 || fresh) {
      this.#handle = loadNative().attrsetNew({ ...this.attributes });
    }
    return this.#handle;
  }
}

abstract class Instrument {
  readonly name: string;
  readonly options: InstrumentOptions;
  #handle = 0;

  protected constructor(
    private readonly kind: number,
    name: string,
    options: InstrumentOptions,
  ) {
    this.name = name;
    this.options = options;
    // Declared eagerly so a bad name or a kind clash fails at the call site.
    this.declare();
  }

  private declare(): number {
    this.#handle = loadNative().instrumentNew(
      this.kind,
      this.name,
      this.options.description,
      this.options.unit,
      this.options.autoAttrs,
      this.options.cardinalityLimit,
    );
    return this.#handle;
  }

  /** Run one native record, redeclaring once if a shutdown freed the handles. */
  protected send(
    call: (handle: number, set: number | undefined, attrs: Record<string, string> | undefined) => void,
    attributes: Attributes | AttributeSet | undefined,
  ): void {
    const pick = (fresh: boolean) =>
      attributes instanceof AttributeSet
        ? ([attributes.handle(fresh), undefined] as const)
        : ([undefined, strings(attributes)] as const);
    try {
      const [set, attrs] = pick(false);
      call(this.#handle, set, attrs);
    } catch (e) {
      if (!isGone(e)) throw e;
      const [set, attrs] = pick(true);
      call(this.declare(), set, attrs);
    }
  }
}

/** Monotonic sum. @group Metrics */
export class Counter extends Instrument {
  /** @internal */
  constructor(name: string, options: InstrumentOptions) {
    super(KIND_COUNTER, name, options);
  }

  /** Add a non-negative integer (default 1). */
  add(value = 1, attributes?: Attributes | AttributeSet): void {
    this.send((h, set, attrs) => loadNative().metricAddU64(h, value, set, attrs), attributes);
  }
}

/** Sum that may go down. @group Metrics */
export class UpDownCounter extends Instrument {
  /** @internal */
  constructor(name: string, options: InstrumentOptions) {
    super(KIND_UP_DOWN, name, options);
  }

  /** Add an integer (negative to decrease). */
  add(value: number, attributes?: Attributes | AttributeSet): void {
    this.send((h, set, attrs) => loadNative().metricAddI64(h, value, set, attrs), attributes);
  }
}

/** Distribution of values. @group Metrics */
export class Histogram extends Instrument {
  /** @internal */
  constructor(name: string, options: InstrumentOptions) {
    super(KIND_HISTOGRAM, name, options);
  }

  record(value: number, attributes?: Attributes | AttributeSet): void {
    this.send((h, set, attrs) => loadNative().metricRecordF64(h, value, set, attrs), attributes);
  }
}

/** Last value. @group Metrics */
export class Gauge extends Instrument {
  /** @internal */
  constructor(name: string, options: InstrumentOptions) {
    super(KIND_GAUGE, name, options);
  }

  record(value: number, attributes?: Attributes | AttributeSet): void {
    this.send((h, set, attrs) => loadNative().metricRecordF64(h, value, set, attrs), attributes);
  }
}

/** The metrics namespace of the design: `metrics.counter(...)`. @group Metrics */
export const metrics = {
  counter: (name: string, options: InstrumentOptions = {}) => new Counter(name, options),
  upDownCounter: (name: string, options: InstrumentOptions = {}) => new UpDownCounter(name, options),
  histogram: (name: string, options: InstrumentOptions = {}) => new Histogram(name, options),
  gauge: (name: string, options: InstrumentOptions = {}) => new Gauge(name, options),
  /** Intern an attribute set for a hot path. */
  attributes: (attributes: Attributes) => new AttributeSet(attributes),
};
