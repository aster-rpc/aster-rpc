import { describe, expect, it } from 'vitest';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { BlobCustodyError, Node } from '@aster-rpc/aster';
import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0' } as const;

describe('blob custody and verified retained imports', () => {
  it('holds temporary roots without anonymous tags; closes admission after accepted work', async () => {
    await using node = await Node.start(OFFLINE);
    const blobs = node.blobs();
    const scope = blobs.custody();
    const hash = await scope.addBytes(Buffer.from('temporary'));
    expect(await blobs.tagListPrefix('')).toEqual([]);
    await blobs.gcRunOnce();
    expect((await blobs.status(hash)).state).toBe('complete');
    const acceptedRead = scope.read(hash);
    scope.close();
    expect(Buffer.from(await acceptedRead).toString()).toBe('temporary');
    await expect(scope.read(hash)).rejects.toMatchObject({ kind: 'closed' });
    scope.close();
    await blobs.gcRunOnce();
    expect((await blobs.status(hash)).state).toBe('not_found');
  });

  it('treats HashSeq as a hash graph, protects children, and hands over the exact named root', async () => {
    await using node = await Node.start(OFFLINE);
    const blobs = node.blobs();
    const scope = blobs.custody();
    const child = await scope.addBytes(Buffer.from('child'));
    const sequence = Buffer.from(child, 'hex'); // One child, no Collection metadata.
    const root = await scope.addBytes(sequence, 'hash_seq');
    expect(Buffer.from(await scope.read(root))).toEqual(sequence);
    await scope.retain(root, 'hash_seq', 'custody/树');
    scope.close();
    await blobs.gcRunOnce();
    expect((await blobs.status(root)).state).toBe('complete');
    expect((await blobs.status(child)).state).toBe('complete');
    expect(await blobs.tagListPrefix('')).toEqual(['custody/树']);
    await blobs.tagDeletePrefix('custody/树');
    await blobs.gcRunOnce();
    expect((await blobs.status(root)).state).toBe('not_found');
    expect((await blobs.status(child)).state).toBe('not_found');
  });

  it('rejects unprotected roots, wrong-format handover, and malformed HashSeq', async () => {
    await using node = await Node.start(OFFLINE);
    using scope = node.blobs().custody();
    const hash = await scope.addBytes(Buffer.from('raw'));
    await expect(scope.retain(hash, 'hash_seq', 'bad')).rejects.toMatchObject({ kind: 'unprotected_root' });
    await expect(scope.retain('00'.repeat(32), 'raw', 'bad')).rejects.toMatchObject({ kind: 'unprotected_root' });
    await expect(scope.addBytes(Buffer.from('not a sequence'), 'hash_seq')).rejects.toBeInstanceOf(BlobCustodyError);
    expect(await node.blobs().tagListPrefix('')).toEqual([]);
  });

  it('verified byte/path Copy imports replace a named root only after a hash match', async () => {
    await using node = await Node.start(OFFLINE);
    const blobs = node.blobs();
    const scope = blobs.custody();
    const first = Buffer.from('first');
    const second = Buffer.from('second');
    const h1 = await scope.addBytes(first);
    const h2 = await scope.addBytes(second);
    scope.close();
    await blobs.addBytesVerified(first, h1, 'raw', 'custody/current');
    await expect(blobs.addBytesVerified(second, h1, 'raw', 'custody/current')).rejects.toMatchObject({ kind: 'hash_mismatch' });
    await blobs.gcRunOnce();
    expect((await blobs.status(h1)).state).toBe('complete');
    expect((await blobs.status(h2)).state).toBe('not_found');
    const temp = await mkdtemp(join(tmpdir(), 'aster-custody-'));
    try {
      const file = join(temp, 'copy');
      await writeFile(file, second);
      expect(await blobs.addPathVerified(file, h2, 'raw', 'custody/current')).toBe(h2);
      await writeFile(file, 'mutated');
      await blobs.gcRunOnce();
      expect((await blobs.status(h1)).state).toBe('not_found');
      expect(Buffer.from(await blobs.read(h2))).toEqual(second);
      expect(await blobs.tagListPrefix('')).toEqual(['custody/current']);
      await blobs.tagDeletePrefix('custody/current');
      await blobs.gcRunOnce();
      expect((await blobs.status(h2)).state).toBe('not_found');
    } finally { await rm(temp, { recursive: true, force: true }); }
  });

  it('pre-aborted host operations and pre-cancelled native workers do not mutate named roots', async () => {
    await using node = await Node.start(OFFLINE);
    const blobs = node.blobs();
    const data = Buffer.from('cancel');
    const scope = blobs.custody();
    const hash = await scope.addBytes(data);
    scope.close();
    const abort = new AbortController();
    abort.abort(new Error('stop before submission'));
    await expect(blobs.addBytesVerified(data, hash, 'raw', 'cancel', { signal: abort.signal })).rejects.toThrow('stop before submission');
    const token = new (loadNative().BlobCancelToken)();
    token.cancel();
    await expect(node.native.blobsClient().addBytesVerified(data, hash, 'raw', 'cancel', token)).rejects.toThrow('ASTER_BLOB_CANCELLED');
    expect(await blobs.tagListPrefix('')).toEqual([]);
    await blobs.gcRunOnce();
    expect((await blobs.status(hash)).state).toBe('not_found');
  });

  it('protects an absent HashSeq before fetching its root and children from a real peer', async () => {
    await using provider = await Node.start(OFFLINE);
    await using fetcher = await Node.start(OFFLINE);
    using source = provider.blobs().custody();
    using target = fetcher.blobs().custody();
    const child = await source.addBytes(Buffer.from('fetched child'));
    const sequence = Buffer.from(child, 'hex');
    const root = await source.addBytes(sequence, 'hash_seq');
    for (let i = 0; i < 100 && provider.directAddresses().length === 0; i++) await new Promise(r => setTimeout(r, 10));
    fetcher.native.addNodeAddr(provider.native);
    await target.protect(root, 'hash_seq');
    const report = await target.downloadHashFrom(root, [provider.id], 'hash_seq', 'split_children');
    expect(report.served).toEqual([provider.id]);
    await fetcher.blobs().gcRunOnce();
    expect(Buffer.from(await target.read(root))).toEqual(sequence);
    expect(Buffer.from(await target.read(child)).toString()).toBe('fetched child');
    expect(await fetcher.blobs().tagListPrefix('')).toEqual([]);
  });

  it('keeps verified named roots through a clean FsStore restart', async () => {
    const temp = await mkdtemp(join(tmpdir(), 'aster-custody-restart-'));
    try {
      let hash: string;
      {
        await using node = await Node.start({ ...OFFLINE, storagePath: temp });
        const scope = node.blobs().custody();
        const data = Buffer.from('persistent');
        hash = await scope.addBytes(data);
        scope.close();
        await node.blobs().addBytesVerified(data, hash, 'raw', 'persistent/root');
      }
      await using reopened = await Node.start({ ...OFFLINE, storagePath: temp });
      await reopened.blobs().gcRunOnce();
      expect(Buffer.from(await reopened.blobs().read(hash)).toString()).toBe('persistent');
      expect(await reopened.blobs().tagListPrefix('')).toEqual(['persistent/root']);
    } finally { await rm(temp, { recursive: true, force: true }); }
  });
});
