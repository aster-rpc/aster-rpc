import { afterEach, describe, expect, it } from 'vitest';
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { once } from 'node:events';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { AuditOutbox, AuditOutboxError, type AuditOutboxConfig } from '@aster-rpc/aster';

const repo = resolve(fileURLToPath(new URL('../..', import.meta.url)));
const receiverScript = resolve(repo, 'tests/audit_outbox/receiver.py');
const directories: string[] = [];
const receivers: ChildProcess[] = [];

async function startReceiver(database: string, port = 0): Promise<{ child: ChildProcess; endpoint: string }> {
  const child = spawn('python3', [receiverScript, '--database', database, '--port', String(port)], { stdio: ['ignore', 'pipe', 'pipe'] });
  receivers.push(child);
  let output = '';
  const endpoint = await new Promise<string>((resolveEndpoint, reject) => {
    child.once('error', reject);
    child.once('exit', () => reject(new Error('receiver exited before ready')));
    child.stdout!.on('data', (chunk) => {
      output += chunk.toString();
      if (output.includes('\n')) resolveEndpoint(output.split('\n')[0]);
    });
  });
  return { child, endpoint };
}

async function stopReceiver(child: ChildProcess) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, 'exit');
  child.kill('SIGTERM');
  await exited;
}

afterEach(async () => {
  await Promise.all(receivers.splice(0).map(stopReceiver));
  for (const path of directories.splice(0)) rmSync(path, { recursive: true, force: true });
});

function temporary(): string {
  const path = mkdtempSync(resolve(tmpdir(), 'aster-audit-ts-'));
  directories.push(path);
  return path;
}

describe('shared native durable audit outbox', () => {
  it('reopens committed events, preserves bigint timestamps, maps errors, and flushes to persistent receiver', async () => {
    const directory = temporary();
    const database = resolve(directory, 'receiver.sqlite');
    const fixture = await startReceiver(database);
    const port = Number(new URL(fixture.endpoint).port);
    await stopReceiver(fixture.child);
    const config: AuditOutboxConfig = {
      directory: resolve(directory, 'outbox'), destination: { endpoint: fixture.endpoint }, maxEventBytes: 1024,
    };
    const outbox = await AuditOutbox.open(config);
    let eventId: string;
    try {
      eventId = await outbox.submit({ timestampMs: 0xffff_ffff_ffff_ffffn, kind: 'intent', payload: { integer: 123, nested: [true, null] } });
      await expect(outbox.submit({ timestampMs: 1n, kind: 'oversized', payload: { data: 'x'.repeat(2048) } }))
        .rejects.toMatchObject({ kind: 'event_too_large' });
      const state = await outbox.status();
      expect(state.pendingRecords).toBe(1n);
      expect(typeof state.storageRecovered).toBe("boolean");
      await expect(outbox.flush(10)).rejects.toMatchObject({ kind: 'timeout' });
    } finally { await outbox.close(); }
    await startReceiver(database, port);
    const reopened = await AuditOutbox.open(config);
    try {
      await reopened.flush(10_000);
      expect((await reopened.status()).pendingRecords).toBe(0n);
    } finally { await reopened.close(); }
    const query = spawnSync('python3', ['-c',
      'import sqlite3,sys,base64,json; db=sqlite3.connect(sys.argv[1]); print(json.dumps([[k,base64.b64encode(v).decode()] for k,v in db.execute("SELECT id,envelope FROM events")]))', database], { encoding: 'utf8' });
    expect(query.status).toBe(0);
    const events: [string, string][] = JSON.parse(query.stdout);
    expect(events).toHaveLength(1);
    expect(events[0][0]).toBe(eventId!);
    const raw = Buffer.from(events[0][1], 'base64');
    expect(createHash('sha256').update(raw).digest('hex')).toBe(eventId!.split(':').at(-1));
    // Inspect the opaque original bytes: JSON.parse would round this timestamp.
    expect(raw.toString()).toContain('18446744073709551615');
    expect(JSON.parse(raw.toString()).payload).toEqual({ integer: 123, nested: [true, null] });
  });

  it('shares workers through clones; ignored flush promises and close keep accepted work', async () => {
    const directory = temporary();
    const fixture = await startReceiver(resolve(directory, 'receiver.sqlite'));
    await stopReceiver(fixture.child);
    const config: AuditOutboxConfig = { directory: resolve(directory, 'outbox'), destination: { endpoint: fixture.endpoint } };
    const outbox = await AuditOutbox.open(config);
    const clone = outbox.clone();
    try {
      const accepted = await clone.submit({ timestampMs: 1n, kind: 'intent', payload: { value: 'kept' } });
      expect((await outbox.status()).committedSequence).toBe(1n);
      await clone.reconfigure({ endpoint: fixture.endpoint, authorization: "Bearer rotated" });
      expect(JSON.stringify(await clone.status(), (_key, value) => typeof value === "bigint" ? value.toString() : value)).not.toContain("Bearer rotated");
      const flushing = outbox.flush(10_000).catch((error: unknown) => error);
      await clone.close();
      expect(await flushing).toBeInstanceOf(AuditOutboxError);
      await expect(outbox.submit({ timestampMs: 2n, kind: 'closed', payload: {} })).rejects.toMatchObject({ kind: 'closed' });
      const reopened = await AuditOutbox.open(config);
      try {
        expect((await reopened.status()).pendingRecords).toBe(1n);
        expect((await reopened.export(accepted)).eventId).toBe(accepted);
      } finally { await reopened.close(); }
    } finally { await outbox.close(); }
  });

  it('rejects nonfinite durations and unsafe JSON integer payloads without coercion', async () => {
    const directory = temporary();
    const fixture = await startReceiver(resolve(directory, 'receiver.sqlite'));
    const outbox = await AuditOutbox.open({ directory: resolve(directory, 'outbox'), destination: { endpoint: fixture.endpoint } });
    try {
      for (const duration of [-1, NaN, Infinity]) await expect(outbox.flush(duration)).rejects.toMatchObject({ kind: 'invalid_event' });
      await expect(outbox.submit({ timestampMs: 1n, kind: 'invalid', payload: { number: Number.MAX_SAFE_INTEGER + 1 } }))
        .rejects.toMatchObject({ kind: 'invalid_event' });
      await expect(outbox.submit({ timestampMs: -1n, kind: 'invalid', payload: {} })).rejects.toMatchObject({ kind: 'invalid_event' });
      expect((await outbox.status()).pendingRecords).toBe(0n);
    } finally { await outbox.close(); }
  });
});
