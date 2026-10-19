/**
 * `AsterServer.peerEvents()` over QUIC: the binding mapping of
 * `aster::rpc::peers` (snapshot, connected, disconnected with a reason, close
 * ends the iterator). Admission, per-peer counting, every reason and the
 * `lagged` behaviour are tested once in aster/tests/peer_events.rs.
 */
import { describe, it, expect, afterEach } from 'vitest';
import { AsterClientWrapper, AsterServer, Rpc, Service, WireType, type PeerEvent, type PeerEvents } from '@aster-rpc/aster';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.js';
import { WAIT_BUDGET_MS } from '../helpers/wait-budget.js';

const available = !!tryLoadNative();
const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;

@WireType('peerev/Msg')
class Msg {
  text = '';
  constructor(init?: Partial<Msg>) { if (init) Object.assign(this, init); }
}

@Service({ name: 'PeerEv', version: 1 })
class PeerEv {
  @Rpc({ request: Msg, response: Msg })
  async ping(req: Msg): Promise<Msg> {
    return new Msg({ text: req.text });
  }
}

let typeInfos: any[] | undefined;
function buildAllTypes(_fory: any, Type: any, codec: any): Map<string, any> {
  if (!typeInfos) {
    const t = Type.struct({ namespace: 'peerev', typeName: 'Msg' }, { text: Type.string() }, { withConstructor: true });
    t.initMeta(Msg);
    typeInfos = [t];
  }
  for (const t of typeInfos) codec.registerType(t);
  return new Map();
}
const generated = { SERVICES: [] as any[], WIRE_TYPES: [] as any[], buildAllTypes };

async function nextWithin(events: PeerEvents): Promise<PeerEvent | null> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const budget = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error('timed out waiting for a peer event')), WAIT_BUDGET_MS);
  });
  try {
    return await Promise.race([events.recv(), budget]);
  } finally {
    clearTimeout(timer);
  }
}

const cleanup: Array<() => Promise<void>> = [];
afterEach(async () => {
  for (const f of cleanup.splice(0).reverse()) await f().catch(() => {}); // swallow-ok: best-effort teardown; the resource may already be closed
});

describe.skipIf(!available)('AsterServer.peerEvents', () => {
  it('reports a client connecting and closing', async () => {
    const server = new AsterServer({ services: [new PeerEv()], config: { ...OFFLINE }, generated });
    await server.start();
    cleanup.push(() => server.close());
    const { snapshot, events } = await server.peerEvents();
    cleanup.push(async () => events.close());
    expect(snapshot).toEqual([]);

    const client = new AsterClientWrapper({ address: server.address, config: { ...OFFLINE }, generated });
    await client.connect();
    const svc: any = await client.client(PeerEv);
    expect((await svc.ping(new Msg({ text: 'hi' }))).text).toBe('hi');
    const clientId = client.node!.id;

    const connected = await nextWithin(events);
    expect(connected).toMatchObject({ kind: 'connected', peer: clientId, alpn: 'aster/1' });
    expect((connected as any).identity).toMatchObject({ endpointId: clientId, roles: [] });
    expect((connected as any).identity.principal).toBeUndefined();

    const later = await server.peerEvents();
    expect(later.snapshot.map((p) => p.peer)).toEqual([clientId]);
    expect(later.snapshot[0].connections).toBeGreaterThanOrEqual(1);
    later.events.close();

    await client.close();
    expect(await nextWithin(events)).toEqual({ kind: 'disconnected', peer: clientId, reason: 'closed' });
  });

  it('close() ends the iterator, including a pending next()', async () => {
    const server = new AsterServer({ services: [new PeerEv()], config: { ...OFFLINE }, generated });
    await server.start();
    cleanup.push(() => server.close());
    const { events } = await server.peerEvents();
    const pending = events.next();
    events.close();
    expect(await pending).toEqual({ value: undefined, done: true });
    const seen: PeerEvent[] = [];
    for await (const ev of events) seen.push(ev);
    expect(seen).toEqual([]);
  });
});
