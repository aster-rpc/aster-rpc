import { describe, expect, it } from 'vitest';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { resolve } from 'node:path';
import { existsSync } from 'node:fs';
import { WebTransportListener, generateWebTransportCertificate } from '@aster-rpc/aster';
const run = promisify(execFile);
const repo = resolve(import.meta.dirname, '../../..');
const python = process.env.ASTER_TEST_PYTHON ?? (existsSync(resolve(repo, '.venv/bin/python')) ? resolve(repo, '.venv/bin/python') : 'python3');
function options() { const cert = generateWebTransportCertificate('raw-test', ['localhost', '127.0.0.1']); return { certificatePem: cert.certificatePem, privateKeyPem: cert.privateKeyPem, path: '/wt' }; }
function peer(addr: string, extra: string[] = []) { return run(python, [resolve(repo, 'tests/python/raw_wt_peer.py'), '--url', `https://${addr}/wt`, ...extra], { timeout: 20000 }); }
async function bytes(stream: { read(maximum?: number): Promise<Uint8Array> }) { const parts: Uint8Array[] = []; for (;;) { const part = await stream.read(3); if (!part.length) return Buffer.concat(parts); parts.push(part); } }

describe('raw WebTransport media', () => {
  it('runs unframed bidi/uni/datagrams with priorities and ACK ownership', async () => {
    const listener = await WebTransportListener.startPublic(options());
    const remote = peer(listener.quicAddr);
    // Attach immediately: the child can fail before server accept completes.
    void remote.catch(() => {});
    let session;
    try {
      session = await listener.accept();
      expect(session.maxDatagramSize).toBeGreaterThan(0);
      const control = await session.acceptBi();
      expect(Buffer.from(await control.read(3)).toString()).toBe('con');
      expect((await bytes(control)).toString()).toBe('trol');
      await control.setPriority(-20); expect(await control.priority()).toBe(-20);
      await expect(control.write(new Uint8Array(1048577))).rejects.toThrow(/byte limit/);
      await control.write(Buffer.from('CONTROL')); await control.finish();
      expect(await control.transportAcked()).toBe(true);
      const incoming = await session.acceptUni();
      expect(incoming.info().has_send).toBe(false);
      expect((await bytes(incoming)).toString()).toBe('input');
      const frame = await session.openUni(); await frame.setPriority(20);
      expect(await frame.priority()).toBe(20);
      await frame.write(Buffer.from('frame')); await frame.finish();
      expect(await frame.transportAcked()).toBe(true);
      expect(Buffer.from(await session.receiveDatagram()).toString()).toBe('feedback');
      session.sendDatagram(Buffer.from('reply'));
      expect(JSON.parse((await remote).stdout).result).toBe('PASS');
      expect(session.stats().udp_tx_bytes).toBeGreaterThan(0);
    } finally { session?.close(); await listener.close(); }
  }, 30000);
  it('cancels pending accept promptly and still joins listener shutdown', async () => {
    const listener = await WebTransportListener.startPublic(options());
    try {
      const abort = new AbortController();
      const accept = listener.accept({ signal: abort.signal });
      abort.abort(); await expect(accept).rejects.toThrow(/cancel/i);
      const waiting = listener.accept(); const rejected = expect(waiting).rejects.toThrow(/closed/i);
      await listener.close(); await rejected;
    } finally { await listener.close(); }
  }, 10000);
  it('rejects unauthenticated CONNECT before publishing a session', async () => {
    const listener = await WebTransportListener.start(options(), () => null);
    try {
      expect(JSON.parse((await peer(listener.quicAddr, ['--scenario', 'connect', '--expected-status', '401'])).stdout).result).toBe('PASS');
      const abort = new AbortController(); const pending = listener.accept({ signal: abort.signal });
      abort.abort(); await expect(pending).rejects.toThrow(/cancel/i);
    } finally { await listener.close(); }
  }, 20000);
  it('preserves a canceled read tail, resets and stops streams, opens bidi and wakes on close', async () => {
    const listener = await WebTransportListener.startPublic(options());
    const remote = peer(listener.quicAddr, ['--scenario', 'lifecycle']);
    void remote.catch(() => {});
    let session: Awaited<ReturnType<typeof listener.accept>> | undefined;
    const streams: Array<{ close(): Promise<void> }> = [];
    try {
      session = await listener.accept();
      const pending = await session.acceptBi(); streams.push(pending);
      expect(Buffer.from(await pending.read(1024)).toString()).toBe('pending');
      const abort = new AbortController();
      const canceled = pending.read(1024, { signal: abort.signal });
      const rejected = expect(canceled).rejects.toThrow(/cancel/i);
      abort.abort(); await rejected;
      session.sendDatagram(Buffer.from('continue'));
      expect((await bytes(pending)).toString()).toBe('-tail');

      const reset = await session.acceptBi(); streams.push(reset);
      expect(Buffer.from(await reset.read(1024)).toString()).toBe('reset-me');
      const stoppedRead = expect(reset.read(1024)).rejects.toThrow(/stop|closed|cancel/i);
      await reset.reset(7); await reset.stop(9); await stoppedRead;
      expect(await reset.transportAcked()).toBe(false);

      const serverBi = await session.openBi(); streams.push(serverBi);
      expect(serverBi.info()).toMatchObject({ has_send: true, has_receive: true });
      await serverBi.write(Buffer.from('server-bi')); await serverBi.finish();
      expect((await bytes(serverBi)).toString()).toBe('SERVER-BI');
      expect(await serverBi.transportAcked()).toBe(true);
      expect(await session.receiveDatagram()).toHaveLength(0);
      expect(Buffer.from(await session.receiveDatagram()).toString()).toBe('lifecycle');
      expect(() => session!.sendDatagram(new Uint8Array(session!.maxDatagramSize! + 1))).toThrow();

      const waiting = expect(session.acceptUni()).rejects.toThrow(/closed/i);
      session.sendDatagram(Buffer.from('done'));
      expect(Buffer.from(await session.receiveDatagram()).toString()).toBe('ready-close');
      session.close(42, Buffer.from('test complete'));
      await waiting;
      expect((await session.closed()).kind).toBeTruthy();
      expect(JSON.parse((await remote).stdout)).toMatchObject({ result: 'PASS', reset: true, server_bidi: true, cancel_read_tail: true, empty_datagram: true, closed: true });
    } finally {
      await Promise.allSettled(streams.map(stream => stream.close()));
      session?.close(); await listener.close();
      await remote.catch(() => {});
    }
  }, 30000);

});
