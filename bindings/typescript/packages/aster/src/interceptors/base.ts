/**
 * Base interceptor primitives and helpers.
 *
 * Interceptors form a middleware chain for RPC calls:
 * - Request path: interceptor1 -> interceptor2 -> ... -> handler
 * - Response path: same order
 * - Error path: reverse order (LIFO)
 */

import { AsyncLocalStorage } from 'node:async_hooks';

import { RpcError, StatusCode } from '../status.js';
import type { RpcPattern } from '../types.js';

/**
 * Async-local storage holding the CallContext for the currently-dispatching
 * RPC. Set by the server dispatcher before invoking a handler; used by
 * {@link CallContext.current} to provide implicit access from within
 * handlers and downstream async code.
 */
const _callContextStorage = new AsyncLocalStorage<CallContext>();

/**
 * Context describing a single RPC invocation.
 * @group Interceptors
 */
export class CallContext {
  service: string;
  method: string;
  callId: string;
  sessionId: string | undefined;
  peer: string | undefined;
  metadata: Record<string, string>;
  attributes: Record<string, string>;
  deadline: number | undefined; // Unix seconds
  isStreaming: boolean;
  pattern: RpcPattern | undefined;
  idempotent: boolean;
  attempt: number;

  constructor(init: {
    service: string;
    method: string;
    callId?: string;
    sessionId?: string;
    peer?: string;
    metadata?: Record<string, string>;
    attributes?: Record<string, string>;
    deadline?: number;
    isStreaming?: boolean;
    pattern?: RpcPattern;
    idempotent?: boolean;
    attempt?: number;
  }) {
    this.service = init.service;
    this.method = init.method;
    this.callId = init.callId ?? crypto.randomUUID();
    this.sessionId = init.sessionId;
    this.peer = init.peer;
    this.metadata = init.metadata ?? {};
    this.attributes = init.attributes ?? {};
    this.deadline = init.deadline;
    this.isStreaming = init.isStreaming ?? false;
    this.pattern = init.pattern;
    this.idempotent = init.idempotent ?? false;
    this.attempt = init.attempt ?? 1;
  }

  /** Seconds until deadline, or undefined if no deadline set. */
  get remainingSeconds(): number | undefined {
    if (this.deadline === undefined) return undefined;
    return Math.max(0, this.deadline - Date.now() / 1000);
  }

  /** True if deadline has passed. */
  get expired(): boolean {
    const remaining = this.remainingSeconds;
    return remaining !== undefined && remaining <= 0;
  }

  /**
   * Return the CallContext for the RPC currently being dispatched, or
   * ``undefined`` when called outside a handler invocation.
   */
  static current(): CallContext | undefined {
    return _callContextStorage.getStore();
  }

  /**
   * Run ``fn`` with this CallContext installed as the current async-local
   * context. Used by the server dispatcher; application code normally uses
   * {@link CallContext.current} to read the context.
   */
  static runWith<R>(ctx: CallContext, fn: () => R): R {
    return _callContextStorage.run(ctx, fn);
  }
}

/**
 * Return true if ``handler`` declares more than one positional parameter,
 * which we take as a signal that it expects a ``CallContext`` as the
 * second argument. TypeScript erases types at runtime, so we cannot
 * inspect the exact parameter type; ``Function.length`` is the only
 * signal available.
 */
export function handlerAcceptsCtx(handler: Function): boolean {
  return (handler as Function).length > 1;
}

/**
 * Base interceptor interface.
 * @group Interceptors
 */
export interface Interceptor {
  onRequest?(ctx: CallContext, request: unknown): Promise<unknown>;
  onResponse?(ctx: CallContext, response: unknown): Promise<unknown>;
  onError?(ctx: CallContext, error: RpcError): Promise<RpcError | null>;
}

/** Convert relative deadline (seconds) to absolute Unix seconds, or undefined. */
export function deadlineFromRelativeSecs(secs: number): number | undefined {
  return secs > 0 ? Date.now() / 1000 + secs : undefined;
}

/** Build a CallContext from common parameters. */
export function buildCallContext(opts: {
  service: string;
  method: string;
  metadata?: Record<string, string>;
  deadlineSecs?: number;
  peer?: string;
  isStreaming?: boolean;
  pattern?: RpcPattern;
  idempotent?: boolean;
  callId?: string;
  sessionId?: string;
  attributes?: Record<string, string>;
}): CallContext {
  return new CallContext({
    ...opts,
    deadline: deadlineFromRelativeSecs(opts.deadlineSecs ?? 0),
  });
}

/** Apply request interceptors in order. */
export async function applyRequestInterceptors(
  interceptors: Interceptor[],
  ctx: CallContext,
  request: unknown,
): Promise<unknown> {
  let current = request;
  for (const i of interceptors) {
    if (i.onRequest) current = await i.onRequest(ctx, current);
  }
  return current;
}

/** Apply response interceptors in order. */
export async function applyResponseInterceptors(
  interceptors: Interceptor[],
  ctx: CallContext,
  response: unknown,
): Promise<unknown> {
  let current = response;
  for (const i of interceptors) {
    if (i.onResponse) current = await i.onResponse(ctx, current);
  }
  return current;
}

/** Apply error interceptors in reverse order (LIFO). */
export async function applyErrorInterceptors(
  interceptors: Interceptor[],
  ctx: CallContext,
  error: RpcError,
): Promise<RpcError | null> {
  let current: RpcError | null = error;
  for (let idx = interceptors.length - 1; idx >= 0; idx--) {
    if (current === null) return null;
    const i = interceptors[idx]!;
    if (i.onError) current = await i.onError(ctx, current);
  }
  return current;
}

/** Normalize any error into an RpcError. */
export function normalizeError(error: unknown): RpcError {
  if (error instanceof RpcError) return error;
  if (error instanceof Error && error.name === 'TimeoutError') {
    return new RpcError(StatusCode.DEADLINE_EXCEEDED, 'deadline exceeded');
  }
  if (error instanceof Error) {
    return new RpcError(StatusCode.UNKNOWN, error.message);
  }
  return new RpcError(StatusCode.UNKNOWN, String(error));
}
