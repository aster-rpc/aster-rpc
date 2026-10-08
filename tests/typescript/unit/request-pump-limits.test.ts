import { describe, it, expect, vi } from 'vitest';
import { IrohTransport } from '../../../bindings/typescript/packages/aster/src/transport/iroh.js';

function fixture(size: number, count: number) {
  let release!: () => void;
  let failWrite!: (error: Error) => void;
  const blocked = new Promise<void>((resolve, reject) => { release = resolve; failWrite = reject; });
  let answer!: (value: Buffer) => void;
  const response = new Promise<Buffer>((resolve) => { answer = resolve; });
  let encoded = 0;
  const sent: number[] = [];
  const send = async (payloads: Uint8Array[]) => {
    await blocked;
    for (const payload of payloads) sent.push(payload.byteLength ? Buffer.from(payload).readUInt32LE() : 0);
  };
  const call = {
    send: vi.fn((payload: Uint8Array) => send([payload])),
    sendMany: vi.fn((payloads: Uint8Array[]) => send(payloads)),
    finish: vi.fn(async () => { answer(Buffer.from('done')); }),
    response: () => response,
    cancel: vi.fn(async () => { answer(Buffer.from('cancelled')); }),
  };
  let produced = 0;
  const source = {
    [Symbol.asyncIterator]() { return this; },
    async next() { return produced < count ? { done: false as const, value: produced++ } : { done: true as const, value: undefined }; },
    return: vi.fn(async () => ({ done: true as const, value: undefined })),
  };
  const transport = new IrohTransport({}, {
    encode(value: unknown) { encoded++; const payload = Buffer.alloc(size); if (size) payload.writeUInt32LE(value as number); return payload; },
    decode(payload: Uint8Array) { return Buffer.from(payload).toString(); },
  }, { rpc: { clientStream: async () => call } as any });
  return { call, source, sent, release, failWrite, answer, encoded: () => encoded, start: () => transport.clientStream('Echo', 'ingest', source) };
}

const quiescent = () => new Promise<void>((resolve) => setImmediate(resolve));

describe('request pump retained limits', () => {
  it('backpressures a byte-heavy source while a native write is blocked', async () => {
    const f = fixture(1024 * 1024, 64);
    const result = f.start();
    try {
      await quiescent();
      expect(f.call.send).toHaveBeenCalledOnce();
      expect(f.encoded()).toBeGreaterThan(1);
      expect(f.encoded() * 1024 * 1024).toBeLessThanOrEqual(8 * 1024 * 1024);
    } finally { f.release(); await result; }
    expect(f.sent).toEqual(Array.from({ length: 64 }, (_, i) => i));
    expect(f.call.finish).toHaveBeenCalledOnce();
  });
  it('counts the in-flight request toward the zero-byte item ceiling', async () => {
    const f = fixture(0, 2048);
    const result = f.start();
    try {
      await quiescent();
      expect(f.encoded()).toBe(1024);
      expect(f.call.sendMany).not.toHaveBeenCalled();
    } finally { f.release(); await result; }
    expect(f.sent).toHaveLength(2048);
    expect(f.call.finish).toHaveBeenCalledOnce();
  });

  it('admits a legal maximum-sized frame without advancing the source behind it', async () => {
    const f = fixture(16 * 1024 * 1024, 2);
    const result = f.start();
    try {
      await quiescent();
      expect(f.encoded()).toBe(1);
    } finally { f.release(); await result; }
    expect(f.sent).toEqual([0, 1]);
  });

  it.each(['resolve', 'reject'] as const)('terminal response wakes a byte-blocked source before a late write %s', async (late) => {
    const f = fixture(1024 * 1024, 64);
    const result = f.start();
    try {
      await quiescent();
      expect(f.encoded()).toBe(8);
      f.answer(Buffer.from('early'));
      expect(await result).toBe('early');
      expect(f.source.return).toHaveBeenCalledOnce();
      await quiescent();
      expect(f.encoded()).toBe(8);
      expect(f.call.finish).not.toHaveBeenCalled();
      expect(f.call.sendMany).not.toHaveBeenCalled();
    } finally {
      if (late === 'resolve') f.release();
      else f.failWrite(new Error('late native write rejection'));
      await quiescent();
    }
    expect(f.call.sendMany).not.toHaveBeenCalled();
  });
  it('preserves a synchronous native send failure for a single exhausted source', async () => {
    const error = new Error('synchronous native send validation');
    const call = {
      send() { throw error; }, sendMany: vi.fn(async () => {}),
      response: () => new Promise<Buffer>(() => {}),
      finish: vi.fn(async () => {}), cancel: vi.fn(async () => {}),
    };
    const transport = new IrohTransport({}, {
      encode: () => Buffer.from('one'), decode: () => undefined,
    }, { rpc: { clientStream: async () => call } as any });
    await expect(transport.clientStream('Echo', 'ingest', (async function* () { yield 1; })())).rejects.toBe(error);
    expect(call.cancel).toHaveBeenCalledOnce();
    expect(call.finish).not.toHaveBeenCalled();
  });
});
