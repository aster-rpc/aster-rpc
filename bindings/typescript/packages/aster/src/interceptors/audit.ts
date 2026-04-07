/**
 * Audit log interceptor — logs RPC calls for compliance/debugging.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import { RpcError, statusName } from '../status.js';

export type AuditLogFn = (entry: AuditEntry) => void;

export interface AuditEntry {
  timestamp: string;
  service: string;
  method: string;
  callId: string;
  peer: string | undefined;
  status: 'started' | 'completed' | 'failed';
  errorCode?: string;
  errorMessage?: string;
}

export class AuditLogInterceptor implements Interceptor {
  private log: AuditLogFn;

  constructor(logFn?: AuditLogFn) {
    this.log = logFn ?? ((entry) => console.log(JSON.stringify(entry)));
  }

  async onRequest(ctx: CallContext, request: unknown): Promise<unknown> {
    this.log({
      timestamp: new Date().toISOString(),
      service: ctx.service,
      method: ctx.method,
      callId: ctx.callId,
      peer: ctx.peer,
      status: 'started',
    });
    return request;
  }

  async onResponse(ctx: CallContext, response: unknown): Promise<unknown> {
    this.log({
      timestamp: new Date().toISOString(),
      service: ctx.service,
      method: ctx.method,
      callId: ctx.callId,
      peer: ctx.peer,
      status: 'completed',
    });
    return response;
  }

  async onError(ctx: CallContext, error: RpcError): Promise<RpcError> {
    this.log({
      timestamp: new Date().toISOString(),
      service: ctx.service,
      method: ctx.method,
      callId: ctx.callId,
      peer: ctx.peer,
      status: 'failed',
      errorCode: statusName(error.code),
      errorMessage: error.message,
    });
    return error;
  }
}
