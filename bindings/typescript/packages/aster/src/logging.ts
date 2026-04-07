/**
 * Structured logging for Aster services.
 *
 * Provides JSON and text format logging with request correlation,
 * sensitive field masking, and standard log fields.
 */

import { AsyncLocalStorage } from 'node:async_hooks';

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
}

/** Simple structured logger. For production, swap with pino. */
export class AsterLogger {
  private format: 'json' | 'text';
  private minLevel: number;
  private mask: boolean;

  constructor(opts?: LoggerOptions) {
    this.format = opts?.format ?? 'text';
    this.minLevel = LEVEL_ORDER[opts?.level ?? 'info'];
    this.mask = opts?.mask ?? true;
  }

  debug(msg: string, fields?: Record<string, unknown>): void { this.log('debug', msg, fields); }
  info(msg: string, fields?: Record<string, unknown>): void { this.log('info', msg, fields); }
  warning(msg: string, fields?: Record<string, unknown>): void { this.log('warning', msg, fields); }
  error(msg: string, fields?: Record<string, unknown>): void { this.log('error', msg, fields); }

  private log(level: LogLevel, msg: string, fields?: Record<string, unknown>): void {
    if (LEVEL_ORDER[level] < this.minLevel) return;

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
