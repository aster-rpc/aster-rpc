/**
 * Data-plane parity with the Rust SDK (`aster::{Blobs, Docs, Gossip}`,
 * connection paths, stream priority) through the NAPI clients.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { randomBytes } from 'node:crypto';
import {
  Node,
  SecretKey,
  authorityFingerprint,
  defaultAuthorId,
  type DocEvent,
  type GossipTopicEvent,
} from '@aster-rpc/aster';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
const NO_HASH = '00'.repeat(32);

async function ready(node: Node): Promise<void> {
  for (let i = 0; i < 200 && node.directAddresses().length === 0; i++) {
    await new Promise((r) => setTimeout(r, 10));
  }
}

async function within<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let t: ReturnType<typeof setTimeout>;
  return Promise.race([
    p.finally(() => clearTimeout(t)),
    new Promise<T>((_, rej) => { t = setTimeout(() => rej(new Error(`timed out: ${what}`)), ms); }),
  ]);
}

let a: Node; // provider
let b: Node; // fetcher
let c: Node; // reachable, holds nothing

beforeAll(async () => {
  a = await Node.start(OFFLINE);
  b = await Node.start(OFFLINE);
  c = await Node.start(OFFLINE);
  await Promise.all([ready(a), ready(b), ready(c)]);
  for (const [x, y] of [[a, b], [b, a], [b, c], [c, b]] as const) x.native.addNodeAddr(y.native);
});

afterAll(async () => {
  await Promise.all([a?.close(), b?.close(), c?.close()]);
});

describe('blobs', () => {
  it('readRange and typed status', async () => {
    const blobs = a.blobs();
    const hash = await blobs.addBytes(Buffer.from('0123456789'));
    expect(Buffer.from(await blobs.readRange(hash, 2, 5)).toString()).toBe('23456');
    expect(await blobs.status(hash)).toEqual({ state: 'complete', size: 10 });
    expect(await blobs.status(NO_HASH)).toEqual({ state: 'not_found' });
    await expect(blobs.readRange(hash, -1, 2)).rejects.toThrow(/offset/);
  });

  it('downloadHash / downloadHashToStore from one peer', async () => {
    const data = randomBytes(64 * 1024);
    const hash = await a.blobs().addBytes(data);
    expect(Buffer.compare(await b.blobs().downloadHash(hash, a.id), data)).toBe(0);

    const hash2 = await a.blobs().addBytes(randomBytes(20_000));
    expect((await c.blobs().status(hash2)).state).toBe('not_found');
    c.native.addNodeAddr(a.native);
    await c.blobs().downloadHashToStore(hash2, a.id, 'raw');
    expect(await c.blobs().status(hash2)).toEqual({ state: 'complete', size: 20_000 });
    expect((await c.blobs().readRange(hash2, 0, 4)).length).toBe(4);
  });

  it('downloadHashFrom an ordered provider list returns a FetchReport', async () => {
    const data = randomBytes(10_000);
    const hash = await a.blobs().addBytes(data);
    const { data: got, report } = await b.blobs().downloadHashFrom(hash, [c.id, a.id]);
    expect(Buffer.compare(got, data)).toBe(0);
    expect(report.attempted).toEqual([c.id, a.id]);
    expect(report.failed).toEqual([c.id]);
    expect(report.served).toEqual([a.id]);
    expect(report.bytesTransferred).toBeGreaterThanOrEqual(10_000);
    const byId = Object.fromEntries(report.providers.map((p) => [p.nodeId, p]));
    expect(byId[a.id]).toMatchObject({ attempts: 1, failures: 0, completedRequests: 1 });
    expect(byId[c.id].failures).toBe(1);

    // Already resident: nothing to transfer.
    const again = await b.blobs().downloadHashToStoreFrom(hash, [a.id], { format: 'raw', strategy: 'whole' });
    expect(again.bytesTransferred).toBe(0);
  });

  it('downloadHashFrom validates providers and strategy', async () => {
    const hash = await a.blobs().addBytes(Buffer.from('x'));
    await expect(b.blobs().downloadHashFrom(hash, [])).rejects.toThrow(/no providers/);
    await expect(b.blobs().downloadHashFrom(hash, [a.id], { strategy: 'split_children' }))
      .rejects.toThrow(/HashSeq/);
    await expect(b.blobs().downloadHashFrom(hash, [a.id], { format: 'bogus' as any }))
      .rejects.toThrow(/blob format/);
  });

  it('downloadCollectionFrom with split_children', async () => {
    const hash = await a.blobs().addCollection([
      ['one.txt', Buffer.from('one')],
      ['two.txt', Buffer.from('two')],
    ]);
    const { files, report } = await b.blobs().downloadCollectionFrom(hash, [a.id], 'split_children');
    expect(files.map((f) => [f.name, Buffer.from(f.data).toString()])).toEqual([
      ['one.txt', 'one'],
      ['two.txt', 'two'],
    ]);
    expect(report.served).toEqual([a.id]);
  });

  it('tagDeletePrefix and gcRunOnce', async () => {
    const blobs = a.blobs();
    const h1 = await blobs.addBytes(Buffer.from('tag me 1'));
    const h2 = await blobs.addBytes(Buffer.from('tag me 2'));
    await blobs.tagSet('dp/x/1', h1, 'raw');
    await blobs.tagSet('dp/x/2', h2, 'raw');
    await blobs.tagSet('dp/keep', h1, 'raw');
    expect(await blobs.tagDeletePrefix('dp/x/')).toBe(2);
    expect(await blobs.tagListPrefix('dp/')).toEqual(['dp/keep']);
    await blobs.gcRunOnce();
    expect((await blobs.status(h1)).state).toBe('complete'); // still tagged
  });
});

describe('docs', () => {
  it('authority report, fingerprint and default author', async () => {
    const docs = a.docs();
    const doc = await docs.create();
    const ns = doc.docId();
    const report = await doc.authorityReport();
    expect(report).toEqual({ namespace: ns, access: 'write', fingerprint: authorityFingerprint(ns, 'write') });
    expect(await docs.authorityReport(ns)).toEqual(report);
    expect(authorityFingerprint(Buffer.from(ns, 'hex'), 'read')).not.toBe(report.fingerprint);
    await expect(docs.authorityReport(NO_HASH)).rejects.toThrow();

    expect(await docs.defaultAuthor()).toBe(a.id);
    const key = SecretKey.generate();
    expect(defaultAuthorId(key)).toBe(SecretKey.nodeId(key));
  });

  it('open / open-or-import namespaces', async () => {
    const docs = a.docs();
    const secret = randomBytes(32);
    const writable = await docs.openOrImportWriteNamespace(secret);
    const ns = writable.docId();
    expect((await docs.openOrImportWriteNamespace(secret.toString('hex'))).docId()).toBe(ns);
    expect((await docs.open(ns))!.docId()).toBe(ns);
    expect(await docs.open(randomBytes(32))).toBeNull();
    // A read import never narrows the stored write capability.
    const read = await docs.openOrImportReadNamespace(Buffer.from(ns, 'hex'));
    expect((await read.authorityReport()).access).toBe('write');
    await expect(docs.open('zz')).rejects.toThrow(/namespace id/);
  });

  it('retireLocalWriteAuthority: busy while referenced, retired after shutdownForRoleSwap', async () => {
    const docs = b.docs();
    const doc = await docs.openOrImportWriteNamespace(randomBytes(32));
    const ns = doc.docId();
    const author = await docs.defaultAuthor();
    await doc.setBytes(author, 'k', Buffer.from('v'));

    const busy = await docs.retireLocalWriteAuthority(ns);
    expect(busy.outcome).toBe('busy');
    expect(busy.handles).toBeGreaterThan(0);

    await doc.shutdownForRoleSwap(5000);
    const retired = await docs.retireLocalWriteAuthority(ns);
    expect(retired).toEqual({ outcome: 'retired', access: 'read' });

    const reopened = (await docs.open(ns))!;
    expect((await reopened.authorityReport()).access).toBe('read');
    expect(Buffer.from((await reopened.getExact(author, 'k'))!).toString()).toBe('v');
  });

  it('durable set, entryExists vs tombstones, content status, typed entries', async () => {
    const docs = a.docs();
    const doc = await docs.create();
    const author = await docs.defaultAuthor();
    const h = await doc.setBytesDurable(author, Buffer.from([1, 2, 3]), Buffer.from('durable'));
    expect(await doc.entryExists(author, Buffer.from([1, 2, 3]))).toBe(true);
    expect(await doc.entryExists(author, 'missing')).toBe(false);

    const latest = (await doc.queryLatestExact(Buffer.from([1, 2, 3])))!;
    expect(latest.author).toBe(author);
    expect([...latest.key]).toEqual([1, 2, 3]);
    expect(latest.contentHash).toBe(h);
    expect(latest.contentLen).toBe(7);
    expect(latest.timestamp).toBeGreaterThan(0);

    expect(await doc.entryContentStatus(h)).toEqual({ state: 'complete', size: 7 });
    expect(await doc.entryContentStatus(NO_HASH)).toEqual({ state: 'not_found' });
    const contents = await doc.readEntryContentsIfComplete([h, NO_HASH]);
    expect(Buffer.from(contents[0]!).toString()).toBe('durable');
    expect(contents[1]).toBeNull();

    await doc.setBytes(author, 'p/1', Buffer.from('a'));
    await doc.setBytes(author, 'p/2', Buffer.from('b'));
    expect((await doc.queryKeyPrefix('p/')).map((e) => Buffer.from(e.key).toString())).toEqual(['p/1', 'p/2']);
    expect(await doc.del(author, 'p/1')).toBe(1);
    // A tombstone: absent for getExact, present for entryExists.
    expect(await doc.getExact(author, 'p/1')).toBeNull();
    expect(await doc.entryExists(author, 'p/1')).toBe(true);
    expect((await doc.queryLatestPrefix('p/')).length).toBe(1);
  });

  it('typed share mode', async () => {
    const doc = await a.docs().create();
    expect(await doc.share('read')).toMatch(/^doc/);
    expect(await doc.shareWithAddr('write')).toMatch(/^doc/);
    await expect(doc.share('admin' as any)).rejects.toThrow(/share mode/);
  });

  it('doc events are an AsyncIterable', async () => {
    const docs = a.docs();
    const doc = await docs.create();
    const author = await docs.defaultAuthor();
    const events = await doc.subscribe();
    await doc.setBytes(author, 'ev', Buffer.from('1'));
    const got: DocEvent[] = [];
    await within((async () => {
      for await (const ev of events) {
        got.push(ev);
        if (ev.kind === 'insert_local') break;
      }
    })(), 5000, 'insert_local');
    const ins = got.at(-1)!;
    expect(ins.kind).toBe('insert_local');
    expect(Buffer.from(ins.entry!.key).toString()).toBe('ev');
    expect(ins.entry!.author).toBe(author);
  });

  it('sync_finished carries origin and result on both ends', async () => {
    const doc = await a.docs().create();
    const eventsA = await doc.subscribe();
    await doc.setBytes(await a.docs().defaultAuthor(), 'k', Buffer.from('v'));
    const joined = await b.docs().joinAndSubscribe(await doc.shareWithAddr('read'));
    const eventsB = joined.takeEvents();
    const firstCompleted = (events: AsyncIterable<DocEvent>, peer: string) =>
      within((async () => {
        for await (const ev of events) {
          if (ev.kind === 'sync_finished' && ev.peer === peer && ev.syncOk) return ev;
        }
        throw new Error('event stream ended');
      })(), 10_000, 'sync_finished');
    const evA = await firstCompleted(eventsA, b.id);
    const evB = await firstCompleted(eventsB, a.id);
    for (const ev of [evA, evB]) {
      expect(ev.syncError).toBeUndefined();
      expect(['accept', 'connect']).toContain(ev.syncOrigin);
    }
    // One end accepted what the other dialled.
    expect(new Set([evA.syncOrigin, evB.syncOrigin])).toEqual(new Set(['accept', 'connect']));
  });
});

describe('gossip', () => {
  it('topic events are an AsyncIterable', async () => {
    const topic = randomBytes(32);
    await expect(a.gossip().subscribe(Buffer.from('short'), [])).rejects.toThrow(/32 bytes/);
    // subscribe joins the swarm (waits for a neighbour), so join both sides together.
    const [ta, tb] = await within(
      Promise.all([a.gossip().subscribe(topic, [b.id]), b.gossip().subscribe(topic, [a.id])]),
      15_000,
      'gossip join',
    );

    const received = within((async () => {
      for await (const ev of ta as AsyncIterable<GossipTopicEvent>) {
        if (ev.kind === 'received') return ev;
      }
      throw new Error('topic ended');
    })(), 15_000, 'gossip message');

    await tb.broadcast(Buffer.from('hello gossip'));
    const ev = await received;
    expect(Buffer.from(ev.data!).toString()).toBe('hello gossip');
    expect(ev.peer ?? null).toBeNull();
  });
});

describe('net', () => {
  it('paths(), selectedPath() and stream priority', async () => {
    await using server = await Node.start({ ...OFFLINE, alpns: ['dp/1'] });
    await ready(server);
    const conn = await b.connect(server.addr, 'dp/1');
    const accepted = await server.native.acceptAster();

    let selected = conn.selectedPath();
    for (let i = 0; i < 100 && !selected; i++) {
      await new Promise((r) => setTimeout(r, 10));
      selected = conn.selectedPath();
    }
    expect(selected).toMatchObject({ kind: 'direct', isSelected: true });
    expect(selected!.addr!.host).toBe('127.0.0.1');
    expect(selected!.addr!.port).toBe(Number(server.directAddresses()[0].split(':').pop()));
    const paths = conn.paths();
    expect(paths.length).toBeGreaterThanOrEqual(1);
    expect(paths.filter((p) => p.isSelected)).toHaveLength(1);

    const bi = await conn.openBi();
    const send = bi.takeSend();
    expect(await send.priority()).toBe(0);
    await send.setPriority(7);
    expect(await send.priority()).toBe(7);

    conn.close(0, 'bye');
    const info = await accepted.closed();
    expect(info.code).toBe(0);
    expect(info.reason).toBe('bye');
  });
});
