import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash, X509Certificate } from 'node:crypto';
import { request, Agent } from 'node:https';
import { createServer } from 'node:net';
import { createSocket } from 'node:dgram';
import { HttpListener, encodeFrame, decodeFrame, decodeRpcStatus, END_STREAM, TRAILER, Service, Rpc, ServiceRegistry, RpcServer, CallContext, AsterServer, Node, generateSelfSignedCertificate, generateWebTransportCertificate, verifyCertificateBinding, generateKeypair } from '@aster-rpc/aster';
import { DispatchLoop, DispatchEventKind, DispatchFlags, respondCall, sendFrame, finishCall } from '../../../bindings/typescript/packages/aster/src/dispatch-server.js';

let dir: string;
let certificatePem: Buffer;
let privateKeyPem: Buffer;
beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), 'aster-ts-http-'));
  execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes', '-days', '1', '-subj', '/CN=localhost', '-keyout', join(dir, 'key.pem'), '-out', join(dir, 'cert.pem')], { stdio: 'ignore' });
  certificatePem = readFileSync(join(dir, 'cert.pem'));
  privateKeyPem = readFileSync(join(dir, 'key.pem'));
});
afterAll(() => rmSync(dir, { recursive: true, force: true }));
const options = () => ({ certificatePem, privateKeyPem, webTransport: true });

function call(addr: string, method: string, items: number[][] = [[1]], token = 'ok', agent: Agent | false = false, service = 'Listener') {
  const body = Buffer.concat(items.map((v, i) => Buffer.from(encodeFrame(Uint8Array.from(v), i === items.length - 1 ? END_STREAM : 0))));
  return new Promise<{ code: number; data: number[][] }>((resolve, reject) => {
    const req = request(`https://${addr}/aster/${service}/${method}`, { method: 'POST', rejectUnauthorized: false, headers: { authorization: token }, agent }, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (b) => chunks.push(b));
      res.on('error', reject);
      res.on('end', () => {
        try {
          let body = Buffer.concat(chunks), code = -1;
          const data: number[][] = [];
          while (body.length) {
            const n = body.readUInt32LE(0) + 4;
            const [payload, flags] = decodeFrame(body.subarray(0, n));
            if (flags & TRAILER) code = decodeRpcStatus(payload).code;
            else data.push([...payload]);
            body = body.subarray(n);
          }
          resolve({ code, data });
        } catch (e) { reject(e); }
      });
    });
    req.setTimeout(5000, () => req.destroy(new Error('request timed out')));
    req.on('error', reject);
    req.end(body);
  });
}
async function released(addr: string) {
  const [host, port] = addr.split(':');
  const tcp = createServer();
  await new Promise<void>((resolve, reject) => { tcp.once('error', reject); tcp.listen(Number(port), host, resolve); });
  await new Promise<void>((resolve) => tcp.close(() => resolve()));
  const udp = createSocket('udp4');
  await new Promise<void>((resolve, reject) => { udp.once('error', reject); udp.bind(Number(port), host, resolve); });
  udp.close();
}
async function fixture() {
  const seen: any[] = [];
  let cancelled!: () => void;
  const cancellation = new Promise<void>((r) => cancelled = r);
  const loop = await DispatchLoop.start({
    config: { relayMode: 'disabled', bindAddr: '127.0.0.1:0' }, builtinNodeInfo: false,
    services: [{ name: 'Listener', version: 1, methods: ['unary', 'server_stream', 'client_stream', 'bidi', 'hang'].map((pattern, i) => ({
      name: pattern, pattern: i === 4 ? 'unary' : i === 3 ? 'bidi_stream' : pattern as any,
      request: { hash: 'a1'.repeat(32) }, response: { hash: 'b2'.repeat(32) },
    })) },
    { name: 'SessionOnly', version: 1, scope: 'session', methods: [{ name: 'unary', pattern: 'unary', request: { hash: 'a1'.repeat(32) }, response: { hash: 'b2'.repeat(32) } }] },
    { name: 'IrohOnly', version: 1, iroh_only: true, methods: [{ name: 'unary', pattern: 'unary', request: { hash: 'a1'.repeat(32) }, response: { hash: 'b2'.repeat(32) } }] }],
    onEvent(ev) { if (ev.kind === DispatchEventKind.Cancelled) cancelled(); },
    async onCall(ev, srv) {
      seen.push(ev);
      if (ev.methodIdx === 4) return;
      if (ev.methodIdx === 0) { await respondCall(srv, ev.callId, ev.request!); return; }
      if (ev.methodIdx === 1) {
        await sendFrame(srv, ev.callId, Buffer.from([7]));
        await sendFrame(srv, ev.callId, Buffer.from([8]));
      } else {
        const items = [ev.request!];
        if (!(ev.flags & DispatchFlags.FirstIsEnd)) {
          for (let item = await srv.recv(ev.callId); item !== null; item = await srv.recv(ev.callId)) items.push(item);
        }
        if (ev.methodIdx === 2) { await respondCall(srv, ev.callId, Buffer.concat(items)); return; }
        for (const item of items) await sendFrame(srv, ev.callId, item);
      }
      await finishCall(srv, ev.callId);
    },
  });
  return { loop, seen, cancellation };
}

@Service({ name: 'Listener', version: 1 })
class TypedListener {
  readonly calls: CallContext[] = [];

  @Rpc({ requires: 'admin' })
  async unary(request: Uint8Array, context: CallContext): Promise<Uint8Array> {
    this.calls.push(context);
    return request;
  }
}

describe('explicit public listeners and certificate helpers', () => {
  it('generates hash-pinnable signed certificates and rejects incorrect binding', async () => {
    const [secret, publicKey] = await generateKeypair();
    const nodeId = Buffer.from(publicKey).toString('hex');
    for (const cert of [generateSelfSignedCertificate(['localhost'], secret), generateWebTransportCertificate('test', ['127.0.0.1'], secret)]) {
      const parsed = new X509Certificate(cert.certificatePem);
      expect(Buffer.from(cert.sha256)).toEqual(createHash('sha256').update(parsed.raw).digest());
      expect(cert.sha256).toHaveLength(32);
      expect(() => verifyCertificateBinding(cert.certificatePem, nodeId)).not.toThrow();
      expect(() => verifyCertificateBinding(cert.certificatePem, '00'.repeat(32))).toThrow();
    }
    const wt = new X509Certificate(generateWebTransportCertificate('test').certificatePem);
    expect(new Date(wt.validTo).getTime() - new Date(wt.validFrom).getTime()).toBeLessThan(14 * 86400000);
    expect(wt.publicKey.asymmetricKeyDetails?.namedCurve).toBe('prime256v1');
    expect(() => verifyCertificateBinding(generateSelfSignedCertificate().certificatePem, nodeId)).toThrow();
    expect(() => verifyCertificateBinding(Uint8Array.of(1), nodeId)).toThrow();
    expect(() => generateSelfSignedCertificate([], Uint8Array.of(1))).toThrow();
  });

  it('serves all four public patterns anonymously without an authenticator and joins shutdown', async () => {
    const f = await fixture();
    const cert = generateWebTransportCertificate('public', ['127.0.0.1']);
    const listener = await HttpListener.startPublic(f.loop.native!, { ...cert, webTransport: true });
    try {
      expect(await call(listener.tcpAddr, 'unary')).toEqual({ code: 0, data: [[1]] });
      expect(await call(listener.tcpAddr, 'server_stream')).toEqual({ code: 0, data: [[7], [8]] });
      expect(await call(listener.tcpAddr, 'client_stream', [[1], [2]])).toEqual({ code: 0, data: [[1, 2]] });
      expect(await call(listener.tcpAddr, 'bidi', [[1], [2]])).toEqual({ code: 0, data: [[1], [2]] });
      expect((await call(listener.tcpAddr, 'unary', [[1]], 'ignored', false, 'SessionOnly')).code).toBe(16);
      expect((await call(listener.tcpAddr, 'unary', [[1]], 'ignored', false, 'IrohOnly')).code).toBe(5);
      expect(f.seen).toHaveLength(4);
      for (const event of f.seen) {
        expect(event.flags & DispatchFlags.IrohAuthenticated).toBe(0);
        expect(event.attributes ?? {}).toEqual({});
      }
      await f.loop.close();
      await listener.close();
      await released(listener.tcpAddr);
    } finally { await listener.close(); await f.loop.close(); }
  });

  it('high-level public mode keeps attribute gates and rejects auth-limit configuration', async () => {
    const node = await Node.start({ relayMode: 'disabled', bindAddr: '127.0.0.1:0' });
    const service = new TypedListener(); const registry = new ServiceRegistry(); registry.register(service);
    const server = new RpcServer({ registry, codec: { encode: x => x as Uint8Array, decode: x => x } });
    await server.start(node);
    const listener = await server.startPublicHttp(options());
    try {
      expect((await call(listener.tcpAddr, 'unary')).code).toBe(7);
      expect(service.calls).toHaveLength(0);
      await expect(server.startPublicHttp({ ...options(), maxPendingAuth: 1 })).rejects.toThrow();
      await expect(server.startHttp(options(), undefined as any)).rejects.toThrow('authenticator');
    } finally { await listener.close(); await server.close(); await node.close(); }
    const lazy = new AsterServer({ services: [], config: { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } });
    const owned = await lazy.startPublicHttp(options());
    await lazy.close(); await owned.close(); await released(owned.tcpAddr);
    await expect(lazy.startPublicHttp(options())).rejects.toThrow('closed');
  });
});

describe('authenticated HTTP listener', () => {
  it('high-level RpcServer isolates Alice/Bob identity, role and provenance on one HTTPS client', async () => {
    const node = await Node.start({ relayMode: 'disabled', bindAddr: '127.0.0.1:0' });
    const service = new TypedListener();
    const registry = new ServiceRegistry(); registry.register(service);
    const server = new RpcServer({
      registry, codec: { encode: (x) => x as Uint8Array, decode: (x) => x },
      peerAttributes: () => ({ stale: 'must not reach HTTP', 'aster.role': 'stale' }),
    });
    const client = new Agent({ keepAlive: true, maxSockets: 1 });
    await server.start(node);
    server.setPeerAttributes('alice', { 'aster.role': 'stale', stale: 'must not reach HTTP' });
    const listener = await server.startHttp(options(), (request) => ({
      principal: request.metadata.authorization === 'bob' ? 'bob' : 'alice',
      attributes: request.metadata.authorization === 'low' ? {} : {
        'aster.role': 'admin', tenant: request.metadata.authorization === 'bob' ? 'b' : 'a',
      },
    }));
    try {
      expect((await call(listener.tcpAddr, 'unary', [[1]], 'low', client)).code).toBe(7);
      expect(service.calls).toHaveLength(0);
      for (const principal of ['alice', 'bob', 'alice']) {
        expect(await call(listener.tcpAddr, 'unary', [[1, 255]], principal, client)).toEqual({ code: 0, data: [[1, 255]] });
        const context = service.calls.at(-1)!;
        expect(context.peer).toBe(principal);
        expect(context.attributes).toEqual({ 'aster.role': 'admin', tenant: principal === 'bob' ? 'b' : 'a' });
        expect(context.irohAuthenticated).toBe(false);
        expect(context.peerAddr).toBeUndefined();
      }
      await listener.close();
      expect((server as any).httpListeners.size).toBe(0);
      await released(listener.tcpAddr);
    } finally { client.destroy(); await listener.close(); await server.close(); await node.close(); }
  });

  it('AsterServer lazy startup is shared and close cannot leave a late listener', async () => {
    const server = new AsterServer({ services: [], config: { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } });
    const first = server.start();
    expect(server.start()).toBe(first);
    await first;
    const listener = await server.startHttp(options(), (request) => request.metadata.authorization === 'ok' ? { principal: 'alice' } : null);
    try {
      expect((await call(listener.tcpAddr, 'describe', [[]], 'bad', false, 'aster.ops.NodeInfo')).code).toBe(16);
      const result = await call(listener.tcpAddr, 'describe', [[]], 'ok', false, 'aster.ops.NodeInfo');
      expect(result.code).toBe(0);
      expect(result.data[0]?.length).toBeGreaterThan(0);
      await server.close();
      await released(listener.tcpAddr);
      await expect(server.startHttp(options(), () => null)).rejects.toThrow('closed');
    } finally { await listener.close(); await server.close(); }

    const racing = new AsterServer({ services: [], config: { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } });
    const pending = racing.startHttp(options(), () => ({ principal: 'alice' })).then((l) => l.close(), (error) => { expect(String(error)).toContain('closed'); });
    const close = racing.close();
    expect(racing.close()).toBe(close);
    await Promise.all([pending, close]);
    await expect(racing.start()).rejects.toThrow('closed');
  });

  it('dispatches all four patterns after per-call authentication', async () => {
    const f = await fixture();
    const auth: any[] = [];
    const listener = await HttpListener.start(f.loop.native, options(), (r) => {
      auth.push(r);
      return r.metadata.authorization === 'ok' ? { principal: 'alice', attributes: { role: 'reader' } } : null;
    });
    try {
      expect(listener.quicAddr).toBe(listener.tcpAddr);
      expect((await call(listener.tcpAddr, 'unary', [[1]], 'bad')).code).toBe(16);
      expect(f.seen).toHaveLength(0);
      expect(await call(listener.tcpAddr, 'unary', [[1, 2]])).toEqual({ code: 0, data: [[1, 2]] });
      expect(await call(listener.tcpAddr, 'server_stream')).toEqual({ code: 0, data: [[7], [8]] });
      expect(await call(listener.tcpAddr, 'client_stream', [[1], [2]])).toEqual({ code: 0, data: [[1, 2]] });
      expect(await call(listener.tcpAddr, 'bidi', [[3], [4]])).toEqual({ code: 0, data: [[3], [4]] });
      expect(auth).toHaveLength(5);
      expect(f.seen.every((e) => e.peer === 'alice')).toBe(true);
    } finally { await listener.close(); await f.loop.close(); }
    await released(listener.tcpAddr);
  });

  it('fails closed for thrown or invalid authentication replies', async () => {
    const f = await fixture();
    for (const authenticate of [() => { throw new Error('private auth backend failure'); }, () => ({ principal: '' })]) {
      const l = await HttpListener.start(f.loop.native, options(), authenticate);
      try { expect((await call(l.tcpAddr, 'unary')).code).toBe(14); expect(f.seen).toHaveLength(0); }
      finally { await l.close(); }
    }
    await f.loop.close();
  });

  it('server close cancels a pending handler and releases child sockets', async () => {
    const f = await fixture();
    const l = await HttpListener.start(f.loop.native, options(), () => ({ principal: 'alice' }));
    const pending = call(l.tcpAddr, 'hang').catch(() => null);
    while (!f.seen.length) await new Promise((r) => setTimeout(r, 5));
    await f.loop.close();
    await pending;
    await released(l.tcpAddr);
    await l.close();
  });

  it('stop cancels pending authentication and refuses malformed configuration', async () => {
    const f = await fixture();
    let begin!: () => void, cancelled!: () => void;
    const started = new Promise<void>((r) => begin = r);
    const ended = new Promise<void>((r) => cancelled = r);
    const l = await HttpListener.start(f.loop.native, options(), (_r, ctx) => new Promise((resolve) => {
      begin(); ctx.signal.addEventListener('abort', () => { cancelled(); resolve(null); }, { once: true });
    }));
    const pending = call(l.tcpAddr, 'unary').catch(() => null);
    await started;
    await l.close();
    await ended;
    await pending;
    await released(l.tcpAddr);
    await expect(HttpListener.start(f.loop.native, { ...options(), privateKeyPem: Buffer.from('bad') }, () => null)).rejects.toThrow();
    for (const maxPendingAuth of [-1, 0.5, NaN, 0x100000000]) {
      await expect(HttpListener.start(f.loop.native, { ...options(), maxPendingAuth }, () => null)).rejects.toThrow(RangeError);
    }
    const ctrl = new AbortController(); ctrl.abort();
    await expect(HttpListener.start(f.loop.native, { ...options(), signal: ctrl.signal }, () => null)).rejects.toThrow();
    await f.loop.close();
    await expect(HttpListener.start(f.loop.native, options(), () => null)).rejects.toThrow('closed');
  });

  it('cancellation after native readiness joins the newly bound owner before rejecting', async () => {
    const f = await fixture();
    const reserve = createServer();
    await new Promise<void>((resolve) => reserve.listen(0, '127.0.0.1', resolve));
    const address = `127.0.0.1:${(reserve.address() as import('node:net').AddressInfo).port}`;
    await new Promise<void>((resolve) => reserve.close(() => resolve()));
    const controller = new AbortController();
    let checks = 0;
    Object.defineProperty(controller.signal, 'throwIfAborted', { value: () => {
      if (++checks === 2) controller.abort(new Error('cancel after readiness'));
      AbortSignal.prototype.throwIfAborted.call(controller.signal);
    } });
    try {
      await expect(HttpListener.start(f.loop.native, { ...options(), bindAddr: address, signal: controller.signal }, () => null)).rejects.toThrow('cancel after readiness');
      expect(checks).toBe(2);
      await released(address);
    } finally { await f.loop.close(); }
  });
});
