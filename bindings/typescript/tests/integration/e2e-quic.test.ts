/**
 * E2E test: TypeScript server + TypeScript client over real QUIC.
 *
 * Requires the NAPI-RS native addon to be built:
 *   cd native && npx napi build --release --platform
 *
 * Creates two IrohNodes (in-memory), connects them, runs an RPC server
 * on one, and calls it from the other via IrohTransport.
 */

import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { resolve, dirname } from 'node:path';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  WireType,
  ServiceRegistry,
  RpcServer,
  JsonCodec,
  RpcPattern,
  IrohTransport,
  createClient,
} from '@aster-rpc/aster';

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

// Load native addon
const candidates = [
  resolve(__dirname, '../../native/aster-transport.darwin-arm64.node'),
  resolve(__dirname, '../../native/aster-transport.darwin-x64.node'),
  resolve(__dirname, '../../native/aster-transport.linux-x64-gnu.node'),
];

let native: any;
for (const path of candidates) {
  if (existsSync(path)) {
    try { native = require(path); break; } catch { /* next */ }
  }
}

const available = !!native;
if (!available) {
  console.warn('Native addon not found — skipping E2E tests.');
  console.warn('Build with: cd native && npx napi build --release --platform');
}

// -- Test service -------------------------------------------------------------

@WireType('e2e/PingRequest')
class PingRequest {
  message = '';
  constructor(init?: Partial<PingRequest>) { if (init) Object.assign(this, init); }
}

@WireType('e2e/PingResponse')
class PingResponse {
  reply = '';
  constructor(init?: Partial<PingResponse>) { if (init) Object.assign(this, init); }
}

@Service({ name: 'PingService', version: 1 })
class PingService {
  @Rpc()
  async ping(req: PingRequest): Promise<PingResponse> {
    return new PingResponse({ reply: `pong: ${req.message}` });
  }

  @ServerStream()
  async *count(req: { n: number }): AsyncGenerator<{ value: number }> {
    for (let i = 1; i <= req.n; i++) {
      yield { value: i };
    }
  }

  @ClientStream()
  async sum(reqs: AsyncIterable<{ value: number }>): Promise<{ total: number }> {
    let total = 0;
    for await (const req of reqs) {
      total += req.value;
    }
    return { total };
  }

  @BidiStream()
  async *echo(reqs: AsyncIterable<PingRequest>): AsyncGenerator<PingResponse> {
    for await (const req of reqs) {
      yield new PingResponse({ reply: `echo: ${req.message}` });
    }
  }
}

// -- E2E tests ----------------------------------------------------------------

describe('E2E: TS server + TS client over QUIC', () => {
  let serverNode: any;
  let clientNode: any;
  let rpcServer: RpcServer;
  let serverRunning: Promise<void>;

  beforeAll(async () => {
    if (!available) return;

    const RPC_ALPN = Buffer.from('aster/1');

    // Create two in-memory nodes with aster ALPN
    serverNode = await native.IrohNode.memoryWithAlpns([RPC_ALPN]);
    clientNode = await native.IrohNode.memory();

    // Add peer addresses so they can find each other
    clientNode.addNodeAddr(serverNode);

    // Set up RPC server
    const registry = new ServiceRegistry();
    registry.register(new PingService());
    rpcServer = new RpcServer({ registry, codec: new JsonCodec() });

    // Start server in background
    serverRunning = rpcServer.serve(serverNode);
  });

  afterAll(async () => {
    if (!available) return;
    rpcServer?.close();
    await serverNode?.close();
    await clientNode?.close();
  });

  it.skipIf(!available)('unary RPC over real QUIC', async () => {
    const serverEndpointId = serverNode.nodeId();
    const RPC_ALPN = Buffer.from('aster/1');

    // Client connects to server
    const conn = await clientNode.connect(serverEndpointId, RPC_ALPN);
    const transport = new IrohTransport(conn);

    // Make a real unary RPC call
    const response = await transport.unary('PingService', 'ping', { message: 'hello' });

    expect(response).toBeDefined();
    expect((response as any).reply).toBe('pong: hello');

    conn.close(0, 'done');
  });

  it.skipIf(!available)('server stream over real QUIC', async () => {
    const serverEndpointId = serverNode.nodeId();
    const RPC_ALPN = Buffer.from('aster/1');

    const conn = await clientNode.connect(serverEndpointId, RPC_ALPN);
    const transport = new IrohTransport(conn);

    const items: number[] = [];
    for await (const item of transport.serverStream('PingService', 'count', { n: 5 })) {
      items.push((item as any).value);
    }

    expect(items).toEqual([1, 2, 3, 4, 5]);

    conn.close(0, 'done');
  });

  it.skipIf(!available)('server node has correct ALPN', async () => {
    expect(serverNode.nodeId()).toBeTruthy();
    expect(serverNode.hasHooks()).toBe(false);
  });

  it.skipIf(!available)('client node can be created', async () => {
    expect(clientNode.nodeId()).toBeTruthy();
    expect(clientNode.nodeId()).not.toBe(serverNode.nodeId());
  });

  it.skipIf(!available)('blobs client is accessible', () => {
    const blobs = serverNode.blobsClient();
    expect(blobs).toBeDefined();
  });

  it.skipIf(!available)('docs client is accessible', () => {
    const docs = serverNode.docsClient();
    expect(docs).toBeDefined();
  });

  it.skipIf(!available)('gossip client is accessible', () => {
    const gossip = serverNode.gossipClient();
    expect(gossip).toBeDefined();
  });

  it.skipIf(!available)('blobs add + read roundtrip', async () => {
    const blobs = serverNode.blobsClient();
    const hash = await blobs.addBytes(Buffer.from('hello world'));
    expect(hash).toBeTruthy();
    expect(hash.length).toBe(64); // hex BLAKE3

    const data = await blobs.read(hash);
    expect(Buffer.from(data).toString()).toBe('hello world');
  });

  it.skipIf(!available)('docs create + set + get roundtrip', async () => {
    const docs = serverNode.docsClient();
    const doc = await docs.create();
    expect(doc.docId()).toBeTruthy();
  });

  it.skipIf(!available)('docs createAuthor', async () => {
    const docs = serverNode.docsClient();
    const authorId = await docs.createAuthor();
    expect(authorId).toBeTruthy();
    expect(authorId.length).toBeGreaterThan(0);
  });

  it.skipIf(!available)('docs download policy get/set', async () => {
    const docs = serverNode.docsClient();
    const doc = await docs.create();

    // Default should be 'everything'
    const policy = await doc.getDownloadPolicy();
    expect(policy).toBe('everything');

    // Set a filtered policy
    await doc.setDownloadPolicy('nothing_except:prefix1,prefix2');
    const updated = await doc.getDownloadPolicy();
    expect(updated).toBe('nothing_except:prefix1,prefix2');
  });

  it.skipIf(!available)('blobs observe snapshot', async () => {
    const blobs = serverNode.blobsClient();
    const hash = await blobs.addBytes(Buffer.from('observe test'));
    const result = await blobs.blobObserveSnapshot(hash);
    expect(result.isComplete).toBe(true);
    expect(result.size).toBeGreaterThan(0);
  });

  it.skipIf(!available)('blobs local info', async () => {
    const blobs = serverNode.blobsClient();
    const hash = await blobs.addBytes(Buffer.from('local info test'));
    const info = await blobs.blobLocalInfo(hash);
    expect(info.isComplete).toBe(true);
    expect(info.localBytes).toBeGreaterThan(0);
  });

  it.skipIf(!available)('client stream over real QUIC', async () => {
    const serverEndpointId = serverNode.nodeId();
    const RPC_ALPN = Buffer.from('aster/1');

    const conn = await clientNode.connect(serverEndpointId, RPC_ALPN);
    const transport = new IrohTransport(conn);

    async function* values() {
      yield { value: 10 };
      yield { value: 20 };
      yield { value: 30 };
    }

    const result = await transport.clientStream('PingService', 'sum', values());
    expect((result as any).total).toBe(60);

    conn.close(0, 'done');
  });

  it.skipIf(!available)('bidi stream over real QUIC', async () => {
    const serverEndpointId = serverNode.nodeId();
    const RPC_ALPN = Buffer.from('aster/1');

    const conn = await clientNode.connect(serverEndpointId, RPC_ALPN);
    const transport = new IrohTransport(conn);

    const channel = transport.bidiStream('PingService', 'echo');

    // Send messages
    await channel.send({ message: 'hello' });
    await channel.send({ message: 'world' });
    await channel.close(); // Signal end of sends

    // Collect responses
    const replies: string[] = [];
    for await (const msg of channel) {
      replies.push((msg as any).reply);
    }

    expect(replies).toEqual(['echo: hello', 'echo: world']);

    conn.close(0, 'done');
  });

  it.skipIf(!available)('docs subscribe receives insert event', async () => {
    const docs = serverNode.docsClient();
    const doc = await docs.create();
    const authorId = await docs.createAuthor();

    // Subscribe to events
    const receiver = await doc.subscribe();

    // Write a key — should trigger an insert_local event
    await doc.setBytes(authorId, 'test-key', Buffer.from('test-value'));

    // Receive the event
    const event = await receiver.recv();
    expect(event).toBeDefined();
    expect(event!.kind).toBe('insert_local');
    expect(event!.author).toBe(authorId);
  });

  it.skipIf(!available)('client connects and makes RPC via IrohTransport + createClient', async () => {
    const serverEndpointId = serverNode.nodeId();
    const RPC_ALPN = Buffer.from('aster/1');
    const conn = await clientNode.connect(serverEndpointId, RPC_ALPN);
    const transport = new IrohTransport(conn);

    const client = createClient(PingService, transport);
    const response = await client.ping(new PingRequest({ message: 'typed client' }));
    expect((response as any).reply).toBe('pong: typed client');

    conn.close(0, 'done');
  });
});
