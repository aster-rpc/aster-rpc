import { describe, it, expect } from 'vitest';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import {
  encodeFrame,
  decodeFrame,
  writeFrame,
  readFrame,
  FramingError,
  COMPRESSED,
  TRAILER,
  HEADER,
  ROW_SCHEMA,
  CALL,
  CANCEL,
} from '@aster-rpc/aster';

// -- Helpers ------------------------------------------------------------------

function hex(data: Uint8Array): string {
  return Array.from(data, b => b.toString(16).padStart(2, '0')).join('');
}

function fromHex(h: string): Uint8Array {
  const clean = h.replace(/\s/g, '');
  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/** In-memory send stream for testing. */
class MemSendStream {
  chunks: Uint8Array[] = [];
  async writeAll(data: Uint8Array): Promise<void> {
    this.chunks.push(new Uint8Array(data));
  }
  get bytes(): Uint8Array {
    const total = this.chunks.reduce((s, c) => s + c.byteLength, 0);
    const buf = new Uint8Array(total);
    let offset = 0;
    for (const c of this.chunks) {
      buf.set(c, offset);
      offset += c.byteLength;
    }
    return buf;
  }
}

/** In-memory recv stream for testing. */
class MemRecvStream {
  private data: Uint8Array;
  private pos = 0;

  constructor(data: Uint8Array) {
    this.data = data;
  }

  async readExact(n: number): Promise<Uint8Array> {
    if (this.pos + n > this.data.byteLength) {
      throw new Error('EOF');
    }
    const slice = this.data.subarray(this.pos, this.pos + n);
    this.pos += n;
    return slice;
  }
}

// -- Flag constants -----------------------------------------------------------

describe('flag constants', () => {
  it('has correct bit values', () => {
    expect(COMPRESSED).toBe(0x01);
    expect(TRAILER).toBe(0x02);
    expect(HEADER).toBe(0x04);
    expect(ROW_SCHEMA).toBe(0x08);
    expect(CALL).toBe(0x10);
    expect(CANCEL).toBe(0x20);
  });

  it('flags are distinct bits', () => {
    const flags = [COMPRESSED, TRAILER, HEADER, ROW_SCHEMA, CALL, CANCEL];
    for (let i = 0; i < flags.length; i++) {
      for (let j = i + 1; j < flags.length; j++) {
        expect(flags[i]! & flags[j]!).toBe(0);
      }
    }
  });
});

// -- Conformance vectors ------------------------------------------------------

describe('conformance vectors', () => {
  let vectors: any;

  // Load conformance vectors
  it('loads framing vectors', async () => {
    const path = resolve(__dirname, '../../../conformance/vectors/framing.json');
    const raw = await readFile(path, 'utf-8');
    vectors = JSON.parse(raw);
    expect(vectors.encode_vectors.length).toBeGreaterThan(0);
  });

  it('encode vectors match expected wire bytes', () => {
    for (const v of vectors.encode_vectors) {
      const payload = v.payload_hex ? fromHex(v.payload_hex) : new Uint8Array(0);
      const wire = encodeFrame(payload, v.flags);
      expect(hex(wire)).toBe(v.expected_wire_hex);
    }
  });

  it('decode vectors match expected payload and flags', () => {
    for (const v of vectors.decode_vectors) {
      const wire = fromHex(v.wire_hex);
      const [payload, flags] = decodeFrame(wire);
      expect(hex(payload)).toBe(v.expected_payload_hex);
      expect(flags).toBe(v.expected_flags);
    }
  });

  it('error vectors: zero-length frame rejected on decode', () => {
    const zeroLen = fromHex('00000000');
    expect(() => decodeFrame(zeroLen)).toThrow(FramingError);
    expect(() => decodeFrame(zeroLen)).toThrow('zero-length frame');
  });
});

// -- encodeFrame / decodeFrame round-trips ------------------------------------

describe('encodeFrame / decodeFrame', () => {
  it('round-trips simple payload', () => {
    const payload = new TextEncoder().encode('Hello');
    const wire = encodeFrame(payload, 0);
    const [decoded, flags] = decodeFrame(wire);
    expect(new TextDecoder().decode(decoded)).toBe('Hello');
    expect(flags).toBe(0);
  });

  it('round-trips empty TRAILER', () => {
    const wire = encodeFrame(new Uint8Array(0), TRAILER);
    const [decoded, flags] = decodeFrame(wire);
    expect(decoded.byteLength).toBe(0);
    expect(flags).toBe(TRAILER);
  });

  it('round-trips empty CANCEL', () => {
    const wire = encodeFrame(new Uint8Array(0), CANCEL);
    const [decoded, flags] = decodeFrame(wire);
    expect(decoded.byteLength).toBe(0);
    expect(flags).toBe(CANCEL);
  });

  it('round-trips combined HEADER|COMPRESSED flags', () => {
    const payload = new Uint8Array([1, 2, 3, 4]);
    const wire = encodeFrame(payload, HEADER | COMPRESSED);
    const [decoded, flags] = decodeFrame(wire);
    expect(Array.from(decoded)).toEqual([1, 2, 3, 4]);
    expect(flags).toBe(HEADER | COMPRESSED);
  });

  it('handles large payload up to MAX_FRAME_SIZE', () => {
    // Don't actually allocate 16 MiB, just test a moderate size
    const payload = new Uint8Array(1024 * 1024); // 1 MiB
    const wire = encodeFrame(payload, 0);
    const [decoded, flags] = decodeFrame(wire);
    expect(decoded.byteLength).toBe(1024 * 1024);
    expect(flags).toBe(0);
  });
});

// -- writeFrame / readFrame (stream-based) ------------------------------------

describe('writeFrame / readFrame', () => {
  it('round-trips through in-memory streams', async () => {
    const send = new MemSendStream();
    const payload = new TextEncoder().encode('Aster RPC');
    await writeFrame(send, payload, HEADER);

    const recv = new MemRecvStream(send.bytes);
    const result = await readFrame(recv, 0); // timeout=0 disables
    expect(result).not.toBeNull();
    const [decoded, flags] = result!;
    expect(new TextDecoder().decode(decoded)).toBe('Aster RPC');
    expect(flags).toBe(HEADER);
  });

  it('returns null on empty stream (EOF)', async () => {
    const recv = new MemRecvStream(new Uint8Array(0));
    const result = await readFrame(recv, 0);
    expect(result).toBeNull();
  });

  it('rejects zero-length payload without TRAILER/CANCEL', async () => {
    await expect(
      writeFrame(new MemSendStream(), new Uint8Array(0), 0),
    ).rejects.toThrow('zero-length payload is not permitted');
  });

  it('allows empty payload with TRAILER', async () => {
    const send = new MemSendStream();
    await writeFrame(send, new Uint8Array(0), TRAILER);
    expect(send.bytes.byteLength).toBe(5); // 4 length + 1 flags
  });

  it('allows empty payload with CANCEL', async () => {
    const send = new MemSendStream();
    await writeFrame(send, new Uint8Array(0), CANCEL);
    expect(send.bytes.byteLength).toBe(5);
  });

  it('reads multiple frames sequentially', async () => {
    const send = new MemSendStream();
    await writeFrame(send, new TextEncoder().encode('one'), 0);
    await writeFrame(send, new TextEncoder().encode('two'), 0);
    await writeFrame(send, new Uint8Array(0), TRAILER);

    const recv = new MemRecvStream(send.bytes);
    const r1 = await readFrame(recv, 0);
    const r2 = await readFrame(recv, 0);
    const r3 = await readFrame(recv, 0);

    expect(new TextDecoder().decode(r1![0])).toBe('one');
    expect(new TextDecoder().decode(r2![0])).toBe('two');
    expect(r3![0].byteLength).toBe(0);
    expect(r3![1]).toBe(TRAILER);
  });
});

// -- Error cases --------------------------------------------------------------

describe('error handling', () => {
  it('decode rejects truncated wire bytes', () => {
    expect(() => decodeFrame(new Uint8Array([1, 0, 0, 0]))).toThrow(
      FramingError,
    );
  });

  it('decode rejects incomplete frame body', () => {
    // Length says 5 bytes but only 2 follow
    const wire = new Uint8Array([5, 0, 0, 0, 0, 1]);
    expect(() => decodeFrame(wire)).toThrow('incomplete frame');
  });

  it('readFrame rejects zero-length in wire', async () => {
    const wire = new Uint8Array([0, 0, 0, 0]);
    const recv = new MemRecvStream(wire);
    await expect(readFrame(recv, 0)).rejects.toThrow('zero-length frame');
  });
});
