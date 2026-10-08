/**
 * O7: `AsterServer` and the Rust health listener (design §6) — the server
 * starts it from `healthEnabled` / `healthPort` / `ASTER_HEALTH_*`, reports
 * readiness and its service count, and starts the observability pipeline from
 * the environment (implicit init, Q2).
 */

import { request } from 'node:http';
import { createServer } from 'node:net';
import { describe, it, expect } from 'vitest';
import { AsterServer, logStats } from '@aster-rpc/aster';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' } as const;

function get(url: string): Promise<{ status: number; body: string }> {
  return new Promise((resolve, reject) => {
    request(url, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode ?? 0, body: Buffer.concat(chunks).toString() }));
    })
      .on('error', reject)
      .end();
  });
}

describe('AsterServer health listener', () => {
  it('is off by default', async () => {
    const server = new AsterServer({ services: [], allowAllConsumers: true, config: { ...OFFLINE } });
    await server.start();
    try {
      expect(server.health).toBeNull();
      // Starting a server started the pipeline and the Rust log delivery.
      expect(logStats().running).toBe(true);
    } finally {
      await server.close();
    }
  });

  it('serves /healthz /readyz /metrics once enabled, and stops with the server', async () => {
    const server = new AsterServer({
      services: [],
      allowAllConsumers: true,
      config: { ...OFFLINE, healthEnabled: true, healthHost: '127.0.0.1' },
    });
    await server.start();
    const health = server.health!;
    expect(health).not.toBeNull();
    const base = `http://127.0.0.1:${health.port}`;
    try {
      expect((await get(`${base}/healthz`)).status).toBe(200);
      const ready = await get(`${base}/readyz`);
      expect(ready.status).toBe(200);
      expect(JSON.parse(ready.body)).toMatchObject({ status: 'ready', services: 0 });

      // Applications add their own readiness checks.
      const db = health.check('db');
      expect((await get(`${base}/readyz`)).status).toBe(503);
      db.set(true);
      expect((await get(`${base}/readyz`)).status).toBe(200);

      const metrics = await get(`${base}/metrics`);
      expect(metrics.status).toBe(200);
      expect(metrics.body).toContain('target_info');
      expect(metrics.body).toContain('aster_health_ready');
    } finally {
      await server.close();
    }
    expect(server.health).toBeNull();
    await expect(get(`${base}/healthz`)).rejects.toThrow();
  });
  it('honors disabled config, rejects bind conflict and releases its port', async () => {
    const occupied = createServer();
    await new Promise<void>((resolve) => occupied.listen(0, '127.0.0.1', resolve));
    const port = (occupied.address() as import('node:net').AddressInfo).port;
    const cfg = { ...OFFLINE, healthPort: port, healthEnabled: false };
    const off = new AsterServer({ services: [], allowAllConsumers: true, config: cfg });
    const conflict = new AsterServer({ services: [], allowAllConsumers: true,
      config: { ...cfg, healthEnabled: true } });
    try {
      await off.start();
      expect(off.health).toBeNull();
      await expect(conflict.start()).rejects.toThrow();
    } finally {
      await off.close(); await conflict.close();
      await new Promise<void>((resolve, reject) => occupied.close((err) => err ? reject(err) : resolve()));
    }
    const server = new AsterServer({ services: [], allowAllConsumers: true,
      config: { ...cfg, healthEnabled: true } });
    try { await server.start(); expect((await get(`http://127.0.0.1:${port}/readyz`)).status).toBe(200); }
    finally { await server.close(); }
    const rebound = createServer();
    await new Promise<void>((resolve, reject) => {
      rebound.once('error', reject); rebound.listen(port, '127.0.0.1', resolve);
    });
    await new Promise<void>((resolve) => rebound.close(() => resolve()));
  });

});
