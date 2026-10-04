/**
 * Expose — local services over Aster (Rust `aster::expose`).
 *
 * Three target shapes on a node's {@link Expose} registry:
 *
 * ```ts
 * const host = await Node.start({ alpns: ['aster-expose/demo'] });
 * const expose = host.expose();
 *
 * // 1. A local TCP socket (raw L4 splice; your own server, any protocol).
 * expose.exposeSocket('web', '127.0.0.1:8080', async ({ peer }) => allowed.has(peer));
 *
 * // 2. A JavaScript HTTP handler, one upcall per request.
 * expose.exposeHttp('app', async (req) => {
 *   const body = await req.readBody();
 *   return { status: 201, headers: { 'content-type': 'text/plain' }, body: Buffer.concat([Buffer.from('hi '), body]) };
 * }, { authorize: async ({ peer }) => allowed.has(peer) });
 *
 * // 3. Act as an edge: peers register routes, a JavaScript policy decides.
 * const router = new EdgeRouter();
 * expose.serveRoutes(router, ({ specs }) => specs.filter((s) => s.host.endsWith('.example')));
 *
 * const server = host.serveExpose(expose, ['aster-expose/demo']);
 * ```
 *
 * Consumer side, on a connection to that ALPN:
 *
 * ```ts
 * const conn = await client.connect(host.addr, 'aster-expose/demo');
 * const stream = await conn.openExposeStream('web');                        // socket target
 * const resp = await relayHttp(conn, 'app', { method: 'POST', body: 'you' }); // HTTP target
 * const reg = await requestRoute(conn, [{ host: 'a.example', port: 443, serviceId: 'app' }]);
 * ```
 *
 * **Authorization** (kind `expose_authorize`) runs once per `(connection,
 * service)`: an error or timeout applies the kind's failure mode
 * (fail-closed by default).
 *
 * **HTTP handlers** (kind `expose_http`). `request.body` is a `Buffer` when
 * the request declared a `content-length` of at most `inlineRequestMax`
 * (64 KiB), else an {@link HttpBodyStream} to `await read()` or `for await`
 * over (`await request.readBody()` covers both). Return an
 * {@link HttpResponse} (or a `string` / `Uint8Array` for a `200`) whose
 * `body` is a string / bytes (sent inline with the head) or a sync / async
 * iterable of chunks (streamed after the head; each write awaits the queue,
 * so a slow consumer is backpressure on your iterator), finished with
 * `trailers`. The kind's timeout (`policies.expose_http`, default 5 s)
 * bounds the time to the response *head* only; raise it for long-polling
 * handlers. A handler that throws answers `500` (the message is logged,
 * never sent), a timeout `504`, a missing handler `501`, an unusable value
 * `502`; an error while streaming aborts the response (the consumer sees a
 * failed response, never a truncated body). When the consumer goes away
 * before the head, `ctx.signal` aborts; after it, the next `write` rejects
 * with a `cancelled` {@link HttpBodyError} and your iterator is closed.
 * `HEAD` responses keep their headers and drop the body. Trailers do not
 * cross the relay hop yet (`aster-expose` V2).
 *
 * **Route policies** (kind `expose_route_policy`): the callback returns the
 * granted {@link RouteSpec}s or their indexes (`true`: all, `false`: none).
 * No usable answer applies the failure mode: fail-closed grants nothing,
 * fail-open grants every requested route.
 * @module
 */

import type {
  Expose as NativeExpose,
  HttpBodyHandle as NativeHttpBodyHandle,
  IrohConnection,
  ExposeHeader as NativeHeader,
  ExposeRouteSpec as NativeRouteSpec,
  RouteRegistration as NativeRouteRegistration,
  EdgeRouter as NativeEdgeRouter,
  UpcallExposeHttpRequest as NativeHttpRequest,
  UpcallExposeHttpResponse as NativeHttpResponse,
  UpcallExposeRoutePolicyRequest as NativeRoutePolicyRequest,
} from '@aster-rpc/transport';
import { loadNative } from './native.js';
import type { NodeId } from './node.js';
import {
  AFTER_RESPOND,
  adapterTable,
  type AfterRespond,
  type UpcallContext,
  type Upcalls,
} from './upcalls.js';

/** Who is asking for an exposed service. @group Expose */
export interface ExposeAuthorizeRequest {
  serviceId: string;
  /** The authenticated peer. */
  peer: NodeId;
  /** Metadata the peer attached to the connection. */
  metadata: Uint8Array;
}

/** Admit (`true`) or refuse a peer for one service. @group Expose */
export type ExposeAuthorizer = (request: ExposeAuthorizeRequest, ctx: UpcallContext) => Promise<boolean> | boolean;

// -- HTTP types ---------------------------------------------------------------------

/** One header (or trailer): lowercase `name`, raw `value` bytes. @group Expose */
export interface HttpHeader {
  readonly name: string;
  readonly value: Buffer;
}

/**
 * Headers to send: an object (`string`, bytes, or an array of them per
 * name) or `[name, value]` pairs. Names are lowercased.
 * @group Expose
 */
export type HttpHeadersInit =
  | Record<string, string | Uint8Array | ReadonlyArray<string | Uint8Array>>
  | Iterable<readonly [string, string | Uint8Array]>;

/** HTTP version of a request. @group Expose */
export type HttpVersion = 'HTTP/0.9' | 'HTTP/1.0' | 'HTTP/1.1' | 'HTTP/2' | 'HTTP/3';

/** A body chunk. @group Expose */
export type HttpBodyChunk = string | Uint8Array;

/** A response body: inline bytes / text, or a sync / async iterable of chunks. @group Expose */
export type HttpResponseBody = HttpBodyChunk | Iterable<HttpBodyChunk> | AsyncIterable<HttpBodyChunk> | null | undefined;

/** A body operation failed. @group Expose */
export class HttpBodyError extends Error {
  override readonly name = 'HttpBodyError';
  constructor(
    message: string,
    /**
     * `unknown_stream`, `cancelled` (the consumer went away), `too_large`,
     * `chunk_too_large`, `finished`, `timeout`, `read`, `invalid_trailers`
     * or `blocking_on_runtime`.
     */
    readonly kind: string,
    /** The stable ABI code (1-9). */
    readonly code: number,
  ) {
    super(message);
  }
}

const BODY_TAG = /^\[body:([a-z_]+):(\d+)\] ([\s\S]*)$/;

function bodyError(e: unknown): unknown {
  if (e instanceof Error) {
    const m = BODY_TAG.exec(e.message);
    if (m) return new HttpBodyError(m[3], m[1], Number(m[2]));
  }
  return e;
}

function toBuffer(chunk: HttpBodyChunk): Buffer {
  if (typeof chunk === 'string') return Buffer.from(chunk);
  if (chunk instanceof Uint8Array) return Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  throw new TypeError(`a body chunk must be a string or Uint8Array, not ${typeof chunk}`);
}

function toHeaders(init: HttpHeadersInit | undefined, what: string): NativeHeader[] {
  if (init == null) return [];
  const out: NativeHeader[] = [];
  const add = (name: string, value: string | Uint8Array) => {
    if (typeof name !== 'string' || name === '') throw new TypeError(`${what}: empty header name`);
    out.push({ name: name.toLowerCase(), value: toBuffer(value) });
  };
  if (Symbol.iterator in init) {
    for (const [name, value] of init as Iterable<readonly [string, string | Uint8Array]>) add(name, value);
  } else {
    for (const [name, value] of Object.entries(init)) {
      if (value === undefined) continue;
      if (Array.isArray(value)) for (const v of value) add(name, v);
      else add(name, value as string | Uint8Array);
    }
  }
  return out;
}

/**
 * One exchange's body handle (`request.handle`; also `request.body` when
 * the request body streams). Read the request body with `await read()` or
 * `for await (const chunk of stream)`. The runtime writes a streamed
 * response through it after the head; `write` / `finish` / `abort` are
 * there for handlers that drive it themselves. A rejected call throws an
 * {@link HttpBodyError}.
 * @group Expose
 */
export class HttpBodyStream implements AsyncIterable<Buffer> {
  /** @internal */
  constructor(private readonly handle: NativeHttpBodyHandle) {}

  get streamId(): number {
    return this.handle.streamId;
  }

  /** The next request-body chunk (at most `max` bytes), or `null` at the end. */
  async read(max?: number): Promise<Buffer | null> {
    try {
      return await this.handle.read(max);
    } catch (e) {
      throw bodyError(e);
    }
  }

  async *[Symbol.asyncIterator](): AsyncGenerator<Buffer, void, undefined> {
    for (;;) {
      const chunk = await this.read();
      if (chunk === null) return;
      yield chunk;
    }
  }

  /**
   * Queue a response-body chunk, resolving once it is queued (waits while
   * the queue is full: backpressure). Write after the response head.
   */
  async write(chunk: HttpBodyChunk): Promise<void> {
    try {
      await this.handle.write(toBuffer(chunk));
    } catch (e) {
      throw bodyError(e);
    }
  }

  /** Queue a chunk without waiting: `false` when the queue is full (nothing queued). */
  tryWrite(chunk: HttpBodyChunk): boolean {
    try {
      return this.handle.tryWrite(toBuffer(chunk));
    } catch (e) {
      throw bodyError(e);
    }
  }

  /** End the response body, optionally with trailers. */
  finish(trailers?: HttpHeadersInit): void {
    try {
      this.handle.finish(toHeaders(trailers, 'trailers'));
    } catch (e) {
      throw bodyError(e);
    }
  }

  /** Abort the response: the consumer sees a failed response, not a truncated one. */
  abort(message = 'aborted'): void {
    try {
      this.handle.abort(message);
    } catch (e) {
      throw bodyError(e);
    }
  }
}

/** One HTTP request for an {@link HttpHandler}. @group Expose */
export class HttpRequest {
  /** @internal */
  constructor(
    readonly serviceId: string,
    /**
     * The transport-authenticated node (`null` off the relay: refuse it if
     * you attribute requests).
     */
    readonly peer: NodeId | null,
    readonly streamId: number,
    readonly version: HttpVersion,
    readonly method: string,
    readonly uri: string,
    readonly headers: readonly HttpHeader[],
    /** `Buffer` (inline) or the {@link HttpBodyStream} to read it from. */
    readonly body: Buffer | HttpBodyStream,
    /** The exchange's body handle either way. */
    readonly handle: HttpBodyStream,
  ) {}

  /** The first value of header `name` (case-insensitive) as text, or `undefined`. */
  header(name: string): string | undefined {
    const n = name.toLowerCase();
    return this.headers.find((h) => h.name === n)?.value.toString('utf8');
  }

  /** The whole request body; `limit` rejects with `too_large` past that many bytes. */
  async readBody(limit?: number): Promise<Buffer> {
    if (Buffer.isBuffer(this.body)) {
      if (limit != null && this.body.length > limit) throw tooLarge(limit);
      return this.body;
    }
    const chunks: Buffer[] = [];
    let n = 0;
    for await (const chunk of this.body) {
      n += chunk.length;
      if (limit != null && n > limit) throw tooLarge(limit);
      chunks.push(chunk);
    }
    return Buffer.concat(chunks);
  }
}

function tooLarge(limit: number): HttpBodyError {
  return new HttpBodyError(`body exceeds ${limit} bytes`, 'too_large', 3);
}

/** A handler's answer. @group Expose */
export interface HttpResponse {
  /** 100-999 (default 200). */
  status?: number;
  headers?: HttpHeadersInit;
  /** Inline bytes / text, or an iterable of chunks (streamed). Default empty. */
  body?: HttpResponseBody;
  /** Sent after a streamed body. */
  trailers?: HttpHeadersInit;
}

/**
 * A JavaScript HTTP service for {@link Expose.exposeHttp}: answer one
 * request. A `string` / `Uint8Array` is a `200`; throwing is a `500`.
 * `ctx.signal` aborts when the consumer goes away before the head or the
 * kind's timeout passes.
 * @group Expose
 */
export type HttpHandler = (
  request: HttpRequest,
  ctx: UpcallContext,
) => Promise<HttpResponse | HttpBodyChunk> | HttpResponse | HttpBodyChunk;

/** Options of {@link Expose.exposeHttp}. @group Expose */
export interface ExposeHttpOptions {
  /** Per-`(connection, service)` admission; omitted admits every peer that reaches the ALPN. */
  authorize?: ExposeAuthorizer;
  /** A declared body of at most this many bytes arrives inline (default 65 536). */
  inlineRequestMax?: number;
  /** Cap on the request body: a declared length over it is a `413` without calling the handler. */
  maxRequestBody?: number;
  /** Cap on the response body: inline over it is a `502`, streamed is aborted. */
  maxResponseBody?: number;
  /** Largest single body chunk (default 1 MiB, > 0): reads return at most this many bytes, writes split. */
  maxChunk?: number;
  /** Response chunks queued before a write waits (default 8). */
  responseBuffer?: number;
}

// -- routes -----------------------------------------------------------------------------

/**
 * A route an edge binds for a peer: `host` (`""` = any) and `port` (`0` =
 * the listener's default) to match, carried as `protocol` to the origin's
 * exposed `serviceId`.
 * @group Expose
 */
export interface RouteSpec {
  host: string;
  port?: number;
  protocol?: 'http' | 'tcp';
  serviceId: string;
}

/** What an edge's {@link RoutePolicy} is asked. @group Expose */
export interface RoutePolicyRequest {
  /** The authenticated registering peer. */
  peer: NodeId;
  /** Metadata the peer attached to its registration. */
  metadata: Uint8Array;
  specs: readonly Required<RouteSpec>[];
}

/**
 * Which requested routes to grant: the specs (or indexes into
 * `request.specs`); `true` grants all, `false` none.
 * @group Expose
 */
export type RoutePolicy = (
  request: RoutePolicyRequest,
  ctx: UpcallContext,
) => Promise<boolean | Iterable<Required<RouteSpec> | number>> | boolean | Iterable<Required<RouteSpec> | number>;

/** A live route of an {@link EdgeRouter}. @group Expose */
export interface EdgeRoute {
  serviceId: string;
  protocol: 'http' | 'tcp';
  /** The origin node. */
  node: NodeId;
}

/**
 * An edge's `(host, port) -> origin route` table, filled by
 * {@link Expose.serveRoutes} as peers register routes.
 * @group Expose
 */
export class EdgeRouter {
  /** @internal */
  readonly native: NativeEdgeRouter = new (loadNative().EdgeRouter)();

  /** The live route for `(host, port)` (then `(host, 0)`), or `null`. */
  lookup(host: string, port = 0): EdgeRoute | null {
    const r = this.native.lookup(host, port);
    return r ? { serviceId: r.serviceId, protocol: r.protocol as 'http' | 'tcp', node: r.nodeId as NodeId } : null;
  }

  /** Drop the route for `(host, port)`. */
  unregister(host: string, port = 0): void {
    this.native.unregister(host, port);
  }
}

function normalizeSpec(s: RouteSpec): Required<RouteSpec> {
  return { host: s.host, port: s.port ?? 0, protocol: s.protocol ?? 'http', serviceId: s.serviceId };
}

function nativeSpec(s: RouteSpec): NativeRouteSpec {
  const n = normalizeSpec(s);
  return { host: n.host, port: n.port, protocol: n.protocol, serviceId: n.serviceId };
}

function specFrom(s: NativeRouteSpec): Required<RouteSpec> {
  return { host: s.host, port: s.port, protocol: s.protocol as 'http' | 'tcp', serviceId: s.serviceId };
}

const sameSpec = (a: Required<RouteSpec>, b: Required<RouteSpec>) =>
  a.host === b.host && a.port === b.port && a.protocol === b.protocol && a.serviceId === b.serviceId;

/**
 * A live route registration ({@link requestRoute}): the edge keeps the
 * routes bound while it is open. `close()` ends it.
 * @group Expose
 */
export class RouteRegistration implements AsyncDisposable {
  /** The requested routes the edge granted. */
  readonly granted: readonly Required<RouteSpec>[];

  /** @internal */
  constructor(private readonly native: NativeRouteRegistration) {
    this.granted = native.granted.map(specFrom);
  }

  get closed(): boolean {
    return this.native.closed;
  }

  /** End the registration (the edge evicts the routes). Idempotent. */
  async close(): Promise<void> {
    await this.native.close();
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }
}

// -- consumer side ------------------------------------------------------------------------

/** A buffered request for {@link relayHttp}. @group Expose */
export interface RelayHttpRequest {
  /** Default `GET`. */
  method?: string;
  /** Default `/`. */
  uri?: string;
  headers?: HttpHeadersInit;
  body?: HttpBodyChunk;
}

/** The collected response of {@link relayHttp}. @group Expose */
export interface RelayedHttpResponse {
  status: number;
  headers: readonly HttpHeader[];
  body: Buffer;
  /** The first value of header `name` (case-insensitive) as text, or `undefined`. */
  header(name: string): string | undefined;
}

/**
 * Relay one buffered HTTP request to the HTTP service `serviceId` on
 * `conn` and collect the response. Rejects when the response fails
 * mid-body (never a truncated body) or the service refuses. Trailers do not
 * cross the relay hop yet.
 * @group Expose
 */
export async function relayHttp(
  conn: IrohConnection,
  serviceId: string,
  request: RelayHttpRequest = {},
): Promise<RelayedHttpResponse> {
  const r = await conn.relayHttp(serviceId, {
    method: request.method,
    uri: request.uri,
    headers: toHeaders(request.headers, 'headers'),
    body: request.body === undefined ? undefined : toBuffer(request.body),
  });
  return {
    status: r.status,
    headers: r.headers,
    body: r.body,
    header(name: string) {
      const n = name.toLowerCase();
      return r.headers.find((h) => h.name === n)?.value.toString('utf8');
    },
  };
}

/**
 * Ask the edge at the other end of `conn` to route `specs` to this node,
 * attaching `metadata` for its route policy. Hold the registration for the
 * routes to stay live.
 * @group Expose
 */
export async function requestRoute(
  conn: IrohConnection,
  specs: readonly RouteSpec[],
  metadata?: Uint8Array,
): Promise<RouteRegistration> {
  const reg = await conn.requestRoute(
    specs.map(nativeSpec),
    metadata === undefined ? undefined : toBuffer(metadata),
  );
  return new RouteRegistration(reg);
}

// -- Expose ------------------------------------------------------------------------------------

interface HttpService {
  handler: HttpHandler;
  maxChunk: number;
}

const DEFAULT_MAX_CHUNK = 1 << 20;

/**
 * A node-wide exposure registry — from `node.expose()`. Serve it with
 * `node.serveExpose(expose, alpns)`.
 *
 * Each registry answers its authorizations, HTTP requests and route
 * policies on one {@link Upcalls} table (a private one unless you pass it to
 * `node.expose({ upcalls })`): raise the HTTP timeout for long-polling
 * handlers with `new Upcalls({ policies: { expose_http: { timeoutMs } } })`.
 * @group Expose
 */
export class Expose implements Disposable {
  private readonly authorizers = new Map<string, ExposeAuthorizer>();
  private readonly httpServices = new Map<string, HttpService>();
  private routePolicy: RoutePolicy | null = null;
  private readonly table: Upcalls;
  private readonly owned: boolean;
  private readonly removers: Array<() => void> = [];

  /** @internal */
  constructor(
    /** @internal */
    readonly native: NativeExpose,
    upcalls?: Upcalls,
  ) {
    const t = adapterTable(upcalls);
    this.table = t.upcalls;
    this.owned = t.owned;
    try {
      this.removers.push(
        this.table.handle('expose_authorize', (async (
          req: { serviceId: string; peerId: string; metadata: Buffer },
          ctx: UpcallContext,
        ) => {
          const authorize = this.authorizers.get(req.serviceId);
          if (!authorize) throw new Error(`service ${req.serviceId} is not exposed`);
          const allowed = await authorize(
            { serviceId: req.serviceId, peer: req.peerId as NodeId, metadata: new Uint8Array(req.metadata) },
            ctx,
          );
          return { allowed: allowed === true };
        }) as never),
      );
    } catch (e) {
      if (this.owned) this.table.close();
      throw e;
    }
  }

  /** The table authorizations are answered on. */
  get upcalls(): Upcalls {
    return this.table;
  }

  /**
   * Expose the local TCP service at `addr` (`"127.0.0.1:8080"`) as
   * `serviceId`, admitting the peers `authorize` accepts (asked once per
   * connection and service).
   */
  exposeSocket(serviceId: string, addr: string, authorize: ExposeAuthorizer): void {
    this.native.exposeSocket(serviceId, addr, this.table.native);
    this.authorizers.set(serviceId, authorize);
  }

  /**
   * Expose a JavaScript HTTP handler as `serviceId` (kind `expose_http`):
   * `handler` is asked once per request (see the module docs).
   * `options.authorize` admits peers once per `(connection, service)`;
   * without it every peer that reaches the ALPN is admitted.
   */
  exposeHttp(serviceId: string, handler: HttpHandler, options: ExposeHttpOptions = {}): void {
    if (typeof handler !== 'function') throw new TypeError('exposeHttp needs a handler function');
    const maxChunk = options.maxChunk ?? DEFAULT_MAX_CHUNK;
    if (!(maxChunk > 0)) throw new RangeError('maxChunk must be > 0');
    this.installHttp();
    const previousAuth = this.authorizers.get(serviceId);
    const previousHttp = this.httpServices.get(serviceId);
    this.httpServices.set(serviceId, { handler, maxChunk });
    if (options.authorize) this.authorizers.set(serviceId, options.authorize);
    else this.authorizers.delete(serviceId);
    try {
      this.native.exposeHttp(serviceId, this.table.native, options.authorize != null, {
        inlineRequestMax: options.inlineRequestMax,
        maxRequestBody: options.maxRequestBody,
        maxResponseBody: options.maxResponseBody,
        maxChunk: options.maxChunk,
        responseBuffer: options.responseBuffer,
      });
    } catch (e) {
      if (previousHttp) this.httpServices.set(serviceId, previousHttp);
      else this.httpServices.delete(serviceId);
      if (previousAuth) this.authorizers.set(serviceId, previousAuth);
      else this.authorizers.delete(serviceId);
      throw e;
    }
  }

  /**
   * Act as an edge: accept route registrations from peers into `router`,
   * granting what `policy` allows (kind `expose_route_policy`; no usable
   * answer applies its failure mode). Without a `policy`, every requested
   * route is granted. A second call replaces the policy.
   */
  serveRoutes(router: EdgeRouter, policy?: RoutePolicy): void {
    if (policy !== undefined && typeof policy !== 'function') throw new TypeError('policy must be a function');
    if (policy) this.installRoutePolicy();
    this.routePolicy = policy ?? null;
    this.native.serveRoutes(router.native, this.table.native, policy != null);
  }

  /** Stop exposing `serviceId`. */
  unexpose(serviceId: string): void {
    this.native.unexpose(serviceId);
    this.authorizers.delete(serviceId);
    this.httpServices.delete(serviceId);
  }

  /** Remove the handlers (and close a private table). */
  close(): void {
    for (const remove of this.removers.splice(0)) remove();
    if (this.owned) this.table.close();
  }

  [Symbol.dispose](): void {
    this.close();
  }

  private httpInstalled = false;
  private installHttp(): void {
    if (this.httpInstalled) return;
    this.removers.push(
      this.table.handle('expose_http', ((req: NativeHttpRequest, ctx: UpcallContext) =>
        this.serveHttp(req, ctx)) as never),
    );
    this.httpInstalled = true;
  }

  private routesInstalled = false;
  private installRoutePolicy(): void {
    if (this.routesInstalled) return;
    this.removers.push(
      this.table.handle('expose_route_policy', ((req: NativeRoutePolicyRequest, ctx: UpcallContext) =>
        this.grantRoutes(req, ctx)) as never),
    );
    this.routesInstalled = true;
  }

  private async serveHttp(req: NativeHttpRequest, ctx: UpcallContext): Promise<NativeHttpResponse> {
    const service = this.httpServices.get(req.serviceId);
    if (!service) throw new Error(`HTTP service ${req.serviceId} is not exposed`);
    const handle = new HttpBodyStream(this.table.native.httpBody(req.streamId, service.maxChunk));
    const request = new HttpRequest(
      req.serviceId,
      (req.peerId ?? null) as NodeId | null,
      req.streamId,
      req.version as HttpVersion,
      req.method,
      req.uri,
      req.headers,
      req.body ?? handle,
      handle,
    );
    const answer = await service.handler(request, ctx);
    return toNativeResponse(answer, handle, req.serviceId);
  }

  private async grantRoutes(
    req: NativeRoutePolicyRequest,
    ctx: UpcallContext,
  ): Promise<{ granted: number[] }> {
    const policy = this.routePolicy;
    if (!policy) throw new Error('no route policy');
    const specs = req.specs.map(specFrom);
    const answer = await policy({ peer: req.peerId as NodeId, metadata: new Uint8Array(req.metadata), specs }, ctx);
    if (answer === true) return { granted: specs.map((_, i) => i) };
    if (answer === false || answer == null) return { granted: [] };
    const granted = new Set<number>();
    for (const g of answer) {
      if (typeof g === 'number') {
        if (!Number.isInteger(g) || g < 0 || g >= specs.length) throw new RangeError(`granted route index ${g} is out of range`);
        granted.add(g);
      } else {
        const i = specs.findIndex((s) => sameSpec(s, normalizeSpec(g)));
        if (i < 0) throw new Error(`granted route ${g.host}:${g.port ?? 0} was not requested`);
        granted.add(i);
      }
    }
    return { granted: [...granted].sort((a, b) => a - b) };
  }
}

// -- response conversion and the streamed-body pump -----------------------------------------

type Streamed = NativeHttpResponse & { [AFTER_RESPOND]?: AfterRespond };

function isChunk(v: unknown): v is HttpBodyChunk {
  return typeof v === 'string' || v instanceof Uint8Array;
}

const RESPONSE_KEYS = new Set(['status', 'headers', 'body', 'trailers']);

function toNativeResponse(answer: unknown, handle: HttpBodyStream, serviceId: string): Streamed {
  const response: HttpResponse = isChunk(answer) ? { body: answer } : (answer as HttpResponse);
  if (response === null || typeof response !== 'object') {
    throw new TypeError(
      `an HTTP handler must return an HttpResponse, string or Uint8Array, not ${response === null ? 'null' : typeof response}`,
    );
  }
  const unknown = Object.keys(response).filter((k) => !RESPONSE_KEYS.has(k));
  if (unknown.length > 0) {
    throw new TypeError(`an HTTP response has no ${unknown.map((k) => `'${k}'`).join(', ')} (status, headers, body, trailers)`);
  }
  const status = response.status ?? 200;
  if (!Number.isInteger(status)) throw new TypeError('HTTP status must be an integer');
  const headers = toHeaders(response.headers, 'headers');
  const body = response.body;
  if (body == null) return { status, headers, body: Buffer.alloc(0) };
  if (isChunk(body)) return { status, headers, body: toBuffer(body) };
  if (Symbol.asyncIterator in body || Symbol.iterator in body) {
    const trailers = response.trailers ?? undefined;
    toHeaders(trailers, 'trailers'); // refuse bad trailers before the head goes out
    const out: Streamed = { status, headers, body: undefined };
    out[AFTER_RESPOND] = (delivered) =>
      delivered
        ? pumpBody(body as Iterable<HttpBodyChunk> | AsyncIterable<HttpBodyChunk>, handle, trailers, serviceId)
        : discardBody(body as { return?: unknown });
    return out;
  }
  throw new TypeError('an HTTP response body must be a string, Uint8Array or an iterable of chunks');
}

/** The response was never sent: release a body generator that never ran. */
async function discardBody(body: { return?: unknown }): Promise<void> {
  // An iterable that is also its own iterator (generators) can be closed.
  try {
    const ret = (body as { return?: () => unknown }).return;
    if (typeof ret === 'function') await ret.call(body);
  } catch {
    /* best effort */
  }
}

const GONE = new Set(['cancelled', 'unknown_stream', 'finished']);

/**
 * After a streamed response head went out: write `body` through the
 * exchange's handle (each write awaits the queue), then finish, or abort
 * when the body fails. A consumer that went away ends it quietly; the
 * iterator is closed either way (`for await` calls `return()`).
 */
async function pumpBody(
  body: Iterable<HttpBodyChunk> | AsyncIterable<HttpBodyChunk>,
  handle: HttpBodyStream,
  trailers: HttpHeadersInit | undefined,
  serviceId: string,
): Promise<void> {
  try {
    for await (const chunk of body) await handle.write(chunk);
    handle.finish(trailers);
  } catch (e) {
    if (e instanceof HttpBodyError && GONE.has(e.kind)) return;
    console.warn(`aster.expose: HTTP handler for ${serviceId} failed mid-body: ${e instanceof Error ? e.message : String(e)}`);
    try {
      handle.abort(e instanceof Error ? `${e.name}: ${e.message}` : String(e));
    } catch {
      /* the exchange is already over */
    }
  }
}
