/**
 * Transfer metrics through NAPI (mirror items M1-M3): node-wide
 * `blobs().transferStats()`, the per-provider bytes in a `FetchReport`, and
 * the per-peer `quicTraffic` / `blobTransfer` on the topology peer view.
 */

import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { randomBytes } from 'node:crypto';
import { Node, type PeerView } from '@aster-rpc/aster';
import { tryLoadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', enableMonitoring: true } as const;
const available = !!tryLoadNative();

async function until<T>(what: string, probe: () => T | undefined | null | false, ms = 10_000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = probe();
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

async function ready(node: Node): Promise<void> {
  await until('direct addresses', () => node.directAddresses().length > 0);
}

describe.skipIf(!available)('transfer metrics', () => {
  let provider: Node;
  let fetcher: Node;

  beforeAll(async () => {
    provider = await Node.start(OFFLINE);
    fetcher = await Node.start(OFFLINE);
    await Promise.all([ready(provider), ready(fetcher)]);
    fetcher.native.addNodeAddr(provider.native);
    provider.native.addNodeAddr(fetcher.native);
  });

  afterAll(async () => {
    await Promise.all([provider?.close(), fetcher?.close()]);
  });

  it('a fetch shows in the report, the node stats and the peer view', async () => {
    const before = fetcher.blobs().transferStats();
    expect(before.fetchedPayloadBytes).toBe(0);
    expect(before.fetchTransfers).toEqual({ plainOk: 0, plainFailed: 0, zstdOk: 0, zstdFailed: 0 });

    const data = randomBytes(64 * 1024);
    const hash = await provider.blobs().addBytes(data);
    const { data: got, report } = await fetcher.blobs().downloadHashFrom(hash, [provider.id]);
    expect(Buffer.compare(got, data)).toBe(0);

    // Per provider: the payload it carried sums to the report's total.
    expect(report.providers).toHaveLength(1);
    const p = report.providers[0];
    expect(p.nodeId).toBe(provider.id);
    expect(p.inFlight).toBe(0);
    expect(p.bytesTransferred).toBe(report.bytesTransferred);
    expect(p.bytesTransferred).toBeGreaterThanOrEqual(data.length);
    expect(report.inFlight).toEqual([]);
    // Both nodes serve zstd by default: the fetch went compressed, so the
    // provider's wire bytes are reported and no plain fallback happened.
    expect(p.plainFallbacks).toBe(0);
    expect(report.plainOnly).toEqual([]);
    expect(p.wireBytes).toBeGreaterThan(0);

    const fetched = fetcher.blobs().transferStats();
    expect(fetched.fetchedPayloadBytes).toBe(p.bytesTransferred);
    expect(fetched.fetchedZstdPayloadBytes).toBe(p.bytesTransferred);
    expect(fetched.fetchedWireBytes).toBe(p.wireBytes);
    expect(fetched.fetchTransfers.zstdOk).toBe(1);
    expect(fetched.fetchDecodeErrors).toBe(0);

    // The serving side counts when the serve ends, shortly after the fetch.
    const served = await until('serve counted', () => {
      const s = provider.blobs().transferStats();
      return s.servedPayloadBytes >= data.length ? s : undefined;
    });
    expect(served.serveTransfers.zstdOk).toBe(1);

    // Per peer, on both ends.
    const fetcherView: PeerView = await until('fetcher peer view', () => fetcher.topology().peer(provider.id));
    expect(fetcherView.blobTransfer.fetchedPayloadBytes).toBe(p.bytesTransferred);
    expect(fetcherView.blobTransfer.fetchedWireBytes).toBe(p.wireBytes);
    expect(fetcherView.quicTraffic.blobsZstd.quicBytesReceived).toBeGreaterThanOrEqual(p.wireBytes!);
    expect(Object.keys(fetcherView.quicTraffic).sort()).toEqual(['blobs', 'blobsZstd', 'docs', 'gossip', 'other', 'rpc']);

    const providerView = await until('provider peer view', () => {
      const v = provider.topology().peer(fetcher.id);
      return v && v.blobTransfer.servedPayloadBytes >= data.length ? v : undefined;
    });
    expect(providerView.blobTransfer.servedZstdPayloadBytes).toBe(providerView.blobTransfer.servedPayloadBytes);
    expect(providerView.quicTraffic.blobsZstd.quicBytesSent).toBeGreaterThan(0);
  });
});
