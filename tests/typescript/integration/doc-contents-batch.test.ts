import { describe, expect, it } from 'vitest';
import { Node } from '@aster-rpc/aster';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;
const MISSING = '00'.repeat(32);

describe('locally complete Docs content batch', () => {
  it('preserves order, duplicates, empty bytes, missing values and large values without retention', async () => {
    await using node = await Node.start(OFFLINE);
    const doc = await node.docs().create();
    const custody = node.blobs().custody();
    const small = await custody.addBytes(Buffer.from('small'));
    const empty = await custody.addBytes(Buffer.alloc(0));
    const data = Buffer.alloc(128 * 1024);
    for (let i = 0; i < data.length; i++) data[i] = i & 255;
    const large = await custody.addBytes(data);
    const result = await doc.readEntryContentsIfComplete([large, small, empty, MISSING, small]);
    expect(result).toHaveLength(5);
    expect(result[0]).toEqual(data);
    expect(result[1]).toEqual(Buffer.from('small'));
    expect(result[2]).toEqual(Buffer.alloc(0));
    expect(result[3]).toBeNull();
    result[1]![0] = 0;
    expect(result[4]).toEqual(Buffer.from('small'));
    expect(await doc.readEntryContentsIfComplete([])).toEqual([]);
    expect(await node.blobs().tagListPrefix('')).toEqual([]);
    custody.close();
    await node.blobs().gcRunOnce();
    expect(await doc.readEntryContentsIfComplete([large, small])).toEqual([null, null]);
  });

  it('never fetches an absent value from a reachable provider', async () => {
    await using source = await Node.start(OFFLINE);
    await using receiver = await Node.start(OFFLINE);
    using custody = source.blobs().custody();
    const hash = await custody.addBytes(Buffer.from('provider only'));
    const sourceDoc = await source.docs().create();
    const receiverDoc = await receiver.docs().create();
    for (let i = 0; i < 100 && source.directAddresses().length === 0; i++) await new Promise(r => setTimeout(r, 10));
    receiver.native.addNodeAddr(source.native);
    expect(await sourceDoc.readEntryContentsIfComplete([hash])).toEqual([Buffer.from('provider only')]);
    expect(await receiverDoc.readEntryContentsIfComplete([hash, hash])).toEqual([null, null]);
    expect((await receiver.blobs().status(hash)).state).toBe('not_found');
    expect(await receiver.blobs().tagListPrefix('')).toEqual([]);
  });

  it('rejects an invalid hash anywhere in the batch and remains usable for retry', async () => {
    await using node = await Node.start(OFFLINE);
    const doc = await node.docs().create();
    using custody = node.blobs().custody();
    const hash = await custody.addBytes(Buffer.from('valid'));
    await expect(doc.readEntryContentsIfComplete([hash, 'invalid'])).rejects.toThrow('content hash must contain exactly 64 ASCII hexadecimal characters');
    expect(await doc.readEntryContentsIfComplete([hash, MISSING, hash])).toEqual([Buffer.from('valid'), null, Buffer.from('valid')]);
  });
});
