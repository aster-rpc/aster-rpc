import { describe, it, expect } from 'vitest';
import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  ServiceRegistry,
  LocalTransport,
  createClient,
  StatusCode,
  RpcError,
} from '@aster-rpc/aster';

// -- Test service -------------------------------------------------------------

class GreetRequest {
  name = '';
  constructor(init?: Partial<GreetRequest>) { if (init) Object.assign(this, init); }
}

class GreetResponse {
  message = '';
  constructor(init?: Partial<GreetResponse>) { if (init) Object.assign(this, init); }
}

@Service({ name: 'Greeter', version: 1 })
class GreeterService {
  @Rpc()
  async greet(req: GreetRequest): Promise<GreetResponse> {
    return new GreetResponse({ message: `Hello, ${req.name}!` });
  }

  @Rpc({ idempotent: true })
  async greetIdempotent(req: GreetRequest): Promise<GreetResponse> {
    return new GreetResponse({ message: `Hello again, ${req.name}!` });
  }

  @ServerStream()
  async *countdown(req: { count: number }): AsyncGenerator<{ value: number }> {
    for (let i = req.count; i > 0; i--) {
      yield { value: i };
    }
  }

  @ClientStream()
  async collectNames(requests: AsyncIterable<GreetRequest>): Promise<GreetResponse> {
    const names: string[] = [];
    for await (const req of requests) names.push(req.name);
    return new GreetResponse({ message: `Hello ${names.join(', ')}!` });
  }

  @BidiStream()
  async *echoStream(requests: AsyncIterable<GreetRequest>): AsyncGenerator<GreetResponse> {
    for await (const req of requests) {
      yield new GreetResponse({ message: `echo: ${req.name}` });
    }
  }
}

// -- Helpers ------------------------------------------------------------------

function setupTransport() {
  const registry = new ServiceRegistry();
  registry.register(new GreeterService());
  return new LocalTransport(registry);
}

// -- Tests --------------------------------------------------------------------

describe('LocalTransport', () => {
  describe('unary', () => {
    it('dispatches unary calls', async () => {
      const transport = setupTransport();
      const result = await transport.unary('Greeter', 'greet', { name: 'World' }) as GreetResponse;
      expect(result.message).toBe('Hello, World!');
    });

    it('throws NOT_FOUND for unknown service', async () => {
      const transport = setupTransport();
      await expect(transport.unary('Unknown', 'foo', {})).rejects.toThrow(RpcError);
      try {
        await transport.unary('Unknown', 'foo', {});
      } catch (e) {
        expect((e as RpcError).code).toBe(StatusCode.NOT_FOUND);
      }
    });

    it('throws NOT_FOUND for unknown method', async () => {
      const transport = setupTransport();
      await expect(transport.unary('Greeter', 'unknown', {})).rejects.toThrow(RpcError);
    });
  });

  describe('server stream', () => {
    it('yields items from server stream', async () => {
      const transport = setupTransport();
      const items: { value: number }[] = [];
      for await (const item of transport.serverStream('Greeter', 'countdown', { count: 3 })) {
        items.push(item as { value: number });
      }
      expect(items).toEqual([{ value: 3 }, { value: 2 }, { value: 1 }]);
    });

    it('handles empty stream', async () => {
      const transport = setupTransport();
      const items: unknown[] = [];
      for await (const item of transport.serverStream('Greeter', 'countdown', { count: 0 })) {
        items.push(item);
      }
      expect(items).toEqual([]);
    });
  });

  describe('client stream', () => {
    it('collects items and returns result', async () => {
      const transport = setupTransport();
      async function* requests() {
        yield new GreetRequest({ name: 'Alice' });
        yield new GreetRequest({ name: 'Bob' });
      }
      const result = await transport.clientStream('Greeter', 'collectNames', requests()) as GreetResponse;
      expect(result.message).toBe('Hello Alice, Bob!');
    });
  });

  describe('bidi stream', () => {
    it('echoes messages bidirectionally', async () => {
      const transport = setupTransport();
      const channel = transport.bidiStream('Greeter', 'echoStream');

      // Send messages
      await channel.send(new GreetRequest({ name: 'one' }));
      await channel.send(new GreetRequest({ name: 'two' }));
      await channel.close();

      // Collect responses
      const responses: GreetResponse[] = [];
      for await (const msg of channel) {
        responses.push(msg as GreetResponse);
      }
      expect(responses.length).toBe(2);
      expect(responses[0]!.message).toBe('echo: one');
      expect(responses[1]!.message).toBe('echo: two');
    });
  });
});

describe('createClient', () => {
  it('creates a typed client proxy', async () => {
    const transport = setupTransport();
    const client = createClient(GreeterService, transport);
    const result = await client.greet(new GreetRequest({ name: 'TypeScript' }));
    expect(result.message).toBe('Hello, TypeScript!');
  });

  it('proxies server stream calls', async () => {
    const transport = setupTransport();
    const client = createClient(GreeterService, transport);
    const items: { value: number }[] = [];
    const stream = client.countdown({ count: 2 });
    for await (const item of stream as AsyncIterable<{ value: number }>) {
      items.push(item);
    }
    expect(items).toEqual([{ value: 2 }, { value: 1 }]);
  });

  it('throws for undecorated class', () => {
    class Plain {}
    const transport = setupTransport();
    expect(() => createClient(Plain as any, transport)).toThrow('not decorated');
  });

  it('close() closes the transport', async () => {
    const transport = setupTransport();
    const client = createClient(GreeterService, transport);
    await client.close(); // should not throw
  });
});
