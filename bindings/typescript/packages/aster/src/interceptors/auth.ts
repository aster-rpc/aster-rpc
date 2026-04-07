/**
 * Auth interceptor — injects or validates auth tokens in metadata.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import { RpcError, StatusCode } from '../status.js';

export class AuthInterceptor implements Interceptor {
  constructor(
    private readonly tokenProvider?: () => string | Promise<string>,
    private readonly tokenValidator?: (token: string) => boolean | Promise<boolean>,
    private readonly headerKey = 'authorization',
  ) {}

  async onRequest(ctx: CallContext, request: unknown): Promise<unknown> {
    // Client-side: inject token
    if (this.tokenProvider) {
      const token = await this.tokenProvider();
      ctx.metadata[this.headerKey] = token;
    }

    // Server-side: validate token
    if (this.tokenValidator) {
      const token = ctx.metadata[this.headerKey];
      if (!token) {
        throw new RpcError(StatusCode.UNAUTHENTICATED, 'missing auth token');
      }
      const valid = await this.tokenValidator(token);
      if (!valid) {
        throw new RpcError(StatusCode.UNAUTHENTICATED, 'invalid auth token');
      }
    }

    return request;
  }
}
