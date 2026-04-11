/**
 * Hello World example -- the simplest Aster service in TypeScript.
 *
 * Demonstrates a unary RPC and a server-streaming RPC running entirely
 * in-process via LocalTransport. No network setup, no credentials, no
 * aster1... ticket -- just one file you can run with `bun service.ts`.
 *
 * For the real-network version (AsterServer + AsterClientWrapper across
 * two processes, with credentials and streaming), see the Mission Control
 * walkthrough at https://docs.aster.site/docs/quickstart/mission-control
 */

import {
  Service,
  Rpc,
  ServerStream,
  WireType,
  ServiceRegistry,
  LocalTransport,
  createClient,
} from '@aster-rpc/aster';

// -- Wire types ---------------------------------------------------------------

@WireType("hello/HelloRequest")
class HelloRequest {
  name = "";
  constructor(init?: Partial<HelloRequest>) { if (init) Object.assign(this, init); }
}

@WireType("hello/HelloResponse")
class HelloResponse {
  message = "";
  constructor(init?: Partial<HelloResponse>) { if (init) Object.assign(this, init); }
}

// -- Service ------------------------------------------------------------------

@Service({ name: "Hello", version: 1 })
class HelloService {
  @Rpc({ timeout: 30, idempotent: true })
  async sayHello(req: HelloRequest): Promise<HelloResponse> {
    return new HelloResponse({ message: `Hello, ${req.name}!` });
  }

  @ServerStream()
  async *countdown(req: HelloRequest): AsyncGenerator<HelloResponse> {
    for (let i = 3; i > 0; i--) {
      yield new HelloResponse({ message: `${req.name}: ${i}...` });
    }
    yield new HelloResponse({ message: `${req.name}: Go!` });
  }
}

// -- Main ---------------------------------------------------------------------

const registry = new ServiceRegistry();
registry.register(new HelloService());
const transport = new LocalTransport(registry);

const client = createClient(HelloService, transport);

// Unary call
console.log("--- Unary ---");
const result = await client.sayHello(new HelloRequest({ name: "TypeScript" }));
console.log(result.message);

// Server streaming
console.log("\n--- Server Stream ---");
for await (const item of client.countdown(new HelloRequest({ name: "Aster" }))) {
  console.log(item.message);
}

await client.close();
console.log("\nDone!");
