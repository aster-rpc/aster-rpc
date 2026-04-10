/**
 * Base interceptor primitives and helpers.
 *
 * Interceptors form a middleware chain for RPC calls:
 * - Request path: interceptor1 -> interceptor2 -> ... -> handler
 * - Response path: same order
 * - Error path: reverse order (LIFO)
 */

import { RpcError, StatusCode } from '../status.js';
import type { RpcPattern } from '../types.js';

/** Context describing a single RPC invocation. */
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
}

/** Base interceptor interface. */
export interface Interceptor {
  onRequest?(ctx: CallContext, request: unknown): Promise<unknown>;
  onResponse?(ctx: CallContext, response: unknown): Promise<unknown>;
  onError?(ctx: CallContext, error: RpcError): Promise<RpcError | null>;
}

/** Convert deadline epoch ms to Unix seconds, or undefined. */
export function deadlineFromEpochMs(ms: number): number | undefined {
  return ms > 0 ? ms / 1000 : undefined;
}

/** Build a CallContext from common parameters. */
export function buildCallContext(opts: {
  service: string;
  method: string;
  metadata?: Record<string, string>;
  deadlineEpochMs?: number;
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
    deadline: deadlineFromEpochMs(opts.deadlineEpochMs ?? 0),
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
