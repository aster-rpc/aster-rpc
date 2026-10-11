import { expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({ tokens: [] as { cancelled: boolean }[] }));
vi.mock('../../../bindings/typescript/packages/aster/src/native.js', () => ({
  loadNative: () => ({ BlobCancelToken: class {
    cancelled = false;
    constructor() { state.tokens.push(this); }
    cancel() { this.cancelled = true; }
  } }),
}));

import { BlobCustody, wrapBlobsClient } from '../../../bindings/typescript/packages/aster/src/blobs.js';
import type { BlobCustody as NativeCustody, BlobsClient as NativeBlobs } from '@aster-rpc/transport';

it('abort requests native cancellation and keeps waiting for cleanup acknowledgement', async () => {
  let reject!: (error: Error) => void;
  const native = { read: () => new Promise<Buffer>((_, fail) => { reject = fail; }) };
  const custody = new BlobCustody(native as unknown as NativeCustody);
  const abort = new AbortController();
  let settled = false;
  const read = custody.read('hash', { signal: abort.signal });
  void read.then(() => { settled = true; }, () => { settled = true; });
  abort.abort();
  await Promise.resolve();
  expect(state.tokens.at(-1)!.cancelled).toBe(true);
  expect(settled).toBe(false);
  reject(new Error('ASTER_BLOB_CANCELLED: cleaned up'));
  await expect(read).rejects.toMatchObject({ kind: 'cancelled' });
});

it('an irrevocable named-root commit returns success despite a racing abort', async () => {
  let resolve!: () => void;
  const native = { retain: () => new Promise<void>(done => { resolve = done; }) };
  const custody = new BlobCustody(native as unknown as NativeCustody);
  const abort = new AbortController();
  const retain = custody.retain('hash', 'raw', 'tag', { signal: abort.signal });
  abort.abort();
  resolve();
  await expect(retain).resolves.toBeUndefined();
});

it('keeps existing native methods bound to their NAPI receiver', async () => {
  const native = {
    marker: 'native',
    async read() { expect(this).toBe(native); return Buffer.from(this.marker); },
    async addBytesVerified() { expect(this).toBe(native); return 'hash'; },
  };
  const blobs = wrapBlobsClient(native as unknown as NativeBlobs);
  expect((await blobs.read('hash')).toString()).toBe('native');
  await expect(blobs.addBytesVerified(new Uint8Array(), 'hash', 'raw', 'tag')).resolves.toBe('hash');
});
