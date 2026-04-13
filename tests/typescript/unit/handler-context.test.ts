import { describe, it, expect } from 'vitest';
import {
  Service,
  Rpc,
  ServiceRegistry,
  LocalTransport,
  CallContext,
  getServiceInfo,
} from '@aster-rpc/aster';

class Req {
  value = '';
  constructor(init?: Partial<Req>) { if (init) Object.assign(this, init); }
}

class Resp {
  service = '';
  method = '';
  viaCurrent = false;
  constructor(init?: Partial<Resp>) { if (init) Object.assign(this, init); }
}

@Service({ name: 'HandlerCtxService', version: 1 })
class HandlerCtxService {
  @Rpc()
  async withExplicitCtx(req: Req, ctx: CallContext): Promise<Resp> {
    const current = CallContext.current();
    return new Resp({
      service: ctx.service,
      method: ctx.method,
      viaCurrent: current === ctx,
    });
  }

  @Rpc()
  async withImplicitCtx(_req: Req): Promise<Resp> {
    const ctx = CallContext.current();
    if (!ctx) throw new Error('CallContext.current() must be set');
    return new Resp({
      service: ctx.service,
      method: ctx.method,
      viaCurrent: true,
    });
  }
}

describe('handler CallContext injection', () => {
  it('injects CallContext as explicit second parameter', async () => {
    const registry = new ServiceRegistry();
    registry.register(new HandlerCtxService());
    const transport = new LocalTransport(registry);

    const resp = await transport.unary('HandlerCtxService', 'withExplicitCtx', { value: 'hi' }) as Resp;
    expect(resp.service).toBe('HandlerCtxService');
    expect(resp.method).toBe('withExplicitCtx');
    expect(resp.viaCurrent).toBe(true);
  });

  it('exposes CallContext via CallContext.current() for handlers without explicit param', async () => {
    const registry = new ServiceRegistry();
    registry.register(new HandlerCtxService());
    const transport = new LocalTransport(registry);

    const resp = await transport.unary('HandlerCtxService', 'withImplicitCtx', { value: 'hi' }) as Resp;
    expect(resp.service).toBe('HandlerCtxService');
    expect(resp.method).toBe('withImplicitCtx');
    expect(resp.viaCurrent).toBe(true);
  });

  it('records acceptsCtx on MethodInfo via Function.length', () => {
    const info = getServiceInfo(HandlerCtxService)!;
    const withExplicit = info.methods.get('withExplicitCtx')!;
    const withImplicit = info.methods.get('withImplicitCtx')!;
    expect(withExplicit.acceptsCtx).toBe(true);
    expect(withImplicit.acceptsCtx).toBe(false);
  });

  it('CallContext.current() returns undefined outside of a dispatch', () => {
    expect(CallContext.current()).toBeUndefined();
  });
});
