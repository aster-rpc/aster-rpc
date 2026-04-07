/**
 * Token-bucket rate limit interceptor.
 *
 * Enforces rate limits at global, per-service, per-method, and per-peer granularity.
 */

import type { Interceptor } from './base.js';
import { CallContext } from './base.js';
import { RpcError, StatusCode } from '../status.js';

class TokenBucket {
  private tokens: number;
  private lastRefill: number;

  constructor(
    private readonly rps: number,
    private readonly maxBurst: number,
  ) {
    this.tokens = maxBurst;
    this.lastRefill = performance.now();
  }

  tryConsume(): boolean {
    this.refill();
    if (this.tokens >= 1) {
      this.tokens -= 1;
      return true;
    }
    return false;
  }

  private refill(): void {
    const now = performance.now();
    const elapsed = (now - this.lastRefill) / 1000;
    this.tokens = Math.min(this.maxBurst, this.tokens + elapsed * this.rps);
    this.lastRefill = now;
  }
}

export interface RateLimitOptions {
  globalRps?: number;
  perServiceRps?: number;
  perMethodRps?: number;
  perPeerRps?: number;
}

export class RateLimitInterceptor implements Interceptor {
  private global: TokenBucket | undefined;
  private perService = new Map<string, TokenBucket>();
  private perMethod = new Map<string, TokenBucket>();
  private perPeer = new Map<string, TokenBucket>();
  private opts: Required<RateLimitOptions>;

  constructor(opts: RateLimitOptions = {}) {
    this.opts = {
      globalRps: opts.globalRps ?? 0,
      perServiceRps: opts.perServiceRps ?? 0,
      perMethodRps: opts.perMethodRps ?? 0,
      perPeerRps: opts.perPeerRps ?? 0,
    };
    if (this.opts.globalRps > 0) {
      this.global = new TokenBucket(this.opts.globalRps, this.opts.globalRps * 2);
    }
  }

  async onRequest(ctx: CallContext, request: unknown): Promise<unknown> {
    if (this.global && !this.global.tryConsume()) {
      throw new RpcError(StatusCode.RESOURCE_EXHAUSTED, 'global rate limit exceeded');
    }

    if (this.opts.perServiceRps > 0) {
      const bucket = this.getOrCreate(this.perService, ctx.service, this.opts.perServiceRps);
      if (!bucket.tryConsume()) {
        throw new RpcError(StatusCode.RESOURCE_EXHAUSTED, `rate limit exceeded for service ${ctx.service}`);
      }
    }

    if (this.opts.perMethodRps > 0) {
      const key = `${ctx.service}/${ctx.method}`;
      const bucket = this.getOrCreate(this.perMethod, key, this.opts.perMethodRps);
      if (!bucket.tryConsume()) {
        throw new RpcError(StatusCode.RESOURCE_EXHAUSTED, `rate limit exceeded for ${key}`);
      }
    }

    if (this.opts.perPeerRps > 0 && ctx.peer) {
      const bucket = this.getOrCreate(this.perPeer, ctx.peer, this.opts.perPeerRps);
      if (!bucket.tryConsume()) {
        throw new RpcError(StatusCode.RESOURCE_EXHAUSTED, `rate limit exceeded for peer ${ctx.peer}`);
      }
    }

    return request;
  }

  private getOrCreate(map: Map<string, TokenBucket>, key: string, rps: number): TokenBucket {
    let bucket = map.get(key);
    if (!bucket) {
      bucket = new TokenBucket(rps, rps * 2);
      map.set(key, bucket);
    }
    return bucket;
  }
}
