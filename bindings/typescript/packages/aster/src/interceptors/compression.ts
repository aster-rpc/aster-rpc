/**
 * Compression interceptor — manages payload compression negotiation.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';

export class CompressionInterceptor implements Interceptor {
  readonly threshold: number;
  constructor(threshold = 4096) { this.threshold = threshold; }

  async onRequest(ctx: CallContext, request: unknown): Promise<unknown> {
    ctx.metadata['accept-encoding'] = 'zstd';
    return request;
  }
}
