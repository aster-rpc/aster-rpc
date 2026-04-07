import { describe, it, expect } from 'vitest';
import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  WireType,
  WIRE_TYPE_KEY,
  getServiceInfo,
  ServiceRegistry,
  RpcPattern,
} from '@aster-rpc/aster';

// -- Test service definitions -------------------------------------------------

@WireType('test/EchoRequest')
class EchoRequest {
  message = '';
  constructor(init?: Partial<EchoRequest>) {
    if (init) Object.assign(this, init);
  }
}

@WireType('test/EchoResponse')
class EchoResponse {
  reply = '';
  constructor(init?: Partial<EchoResponse>) {
    if (init) Object.assign(this, init);
  }
}

@Service({ name: 'Echo', version: 1 })
class EchoService {
  @Rpc({ timeout: 30, idempotent: true })
  async echo(req: EchoRequest): Promise<EchoResponse> {
    return new EchoResponse({ reply: req.message });
  }

  @ServerStream()
  async *watchItems(req: EchoRequest): AsyncGenerator<EchoResponse> {
    yield new EchoResponse({ reply: `item1: ${req.message}` });
    yield new EchoResponse({ reply: `item2: ${req.message}` });
  }

  @ClientStream()
  async uploadBatch(requests: AsyncIterable<EchoRequest>): Promise<EchoResponse> {
    let count = 0;
    for await (const _req of requests) count++;
    return new EchoResponse({ reply: `received ${count}` });
  }
}

@Service({ name: 'Chat', version: 1, scoped: 'stream' })
class ChatService {
  @BidiStream()
  async *chat(requests: AsyncIterable<EchoRequest>): AsyncGenerator<EchoResponse> {
    for await (const req of requests) {
      yield new EchoResponse({ reply: `echo: ${req.message}` });
    }
  }
}

// -- Tests --------------------------------------------------------------------

describe('@WireType', () => {
  it('stores wire type tag on class', () => {
    expect((EchoRequest as any)[WIRE_TYPE_KEY]).toBe('test/EchoRequest');
    expect((EchoResponse as any)[WIRE_TYPE_KEY]).toBe('test/EchoResponse');
  });
});

describe('@Service', () => {
  it('stores ServiceInfo on decorated class', () => {
    const info = getServiceInfo(EchoService);
    expect(info).toBeDefined();
    expect(info!.name).toBe('Echo');
    expect(info!.version).toBe(1);
    expect(info!.scoped).toBe('shared');
  });

  it('collects decorated methods', () => {
    const info = getServiceInfo(EchoService)!;
    expect(info.methods.size).toBe(3);
    expect(info.methods.has('echo')).toBe(true);
    expect(info.methods.has('watchItems')).toBe(true);
    expect(info.methods.has('uploadBatch')).toBe(true);
  });

  it('stores correct patterns', () => {
    const info = getServiceInfo(EchoService)!;
    expect(info.methods.get('echo')!.pattern).toBe(RpcPattern.UNARY);
    expect(info.methods.get('watchItems')!.pattern).toBe(RpcPattern.SERVER_STREAM);
    expect(info.methods.get('uploadBatch')!.pattern).toBe(RpcPattern.CLIENT_STREAM);
  });

  it('stores method options', () => {
    const echo = getServiceInfo(EchoService)!.methods.get('echo')!;
    expect(echo.timeout).toBe(30);
    expect(echo.idempotent).toBe(true);
  });

  it('supports session-scoped services', () => {
    const info = getServiceInfo(ChatService)!;
    expect(info.scoped).toBe('stream');
    expect(info.methods.get('chat')!.pattern).toBe(RpcPattern.BIDI_STREAM);
  });
});

describe('ServiceRegistry', () => {
  it('registers and looks up services', () => {
    const registry = new ServiceRegistry();
    const info = registry.register(new EchoService());
    expect(info.name).toBe('Echo');

    const found = registry.lookup('Echo');
    expect(found).toBe(info);
  });

  it('looks up by name and version', () => {
    const registry = new ServiceRegistry();
    registry.register(new EchoService());
    expect(registry.lookup('Echo', 1)).toBeDefined();
    expect(registry.lookup('Echo', 2)).toBeUndefined();
  });

  it('looks up methods', () => {
    const registry = new ServiceRegistry();
    registry.register(new EchoService());
    const result = registry.lookupMethod('Echo', 'echo');
    expect(result).toBeDefined();
    expect(result![1].pattern).toBe(RpcPattern.UNARY);
  });

  it('rejects undecorated classes', () => {
    const registry = new ServiceRegistry();
    class Plain {}
    expect(() => registry.register(new Plain())).toThrow('not decorated');
  });

  it('rejects duplicate registration', () => {
    const registry = new ServiceRegistry();
    registry.register(new EchoService());
    expect(() => registry.register(new EchoService())).toThrow('already registered');
  });

  it('tracks size', () => {
    const registry = new ServiceRegistry();
    expect(registry.size).toBe(0);
    registry.register(new EchoService());
    expect(registry.size).toBe(1);
    registry.register(new ChatService());
    expect(registry.size).toBe(2);
  });
});
