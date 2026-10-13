import { describe, expect, it } from 'vitest';
import { request } from 'node:https';
import { AsterServer, Node, HttpListener, generateSelfSignedCertificate, encodeFrame, decodeFrame, decodeRpcStatus, END_STREAM, TRAILER } from '@aster-rpc/aster';
import { DispatchLoop } from '../../../bindings/typescript/packages/aster/src/dispatch-server.js';
import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

const OFFLINE = { relayMode: 'disabled', bindAddr: '127.0.0.1:0', logLevel: 'warn' } as const;
const requirement = { kind: 'role', roles: ['operator'] } as const;
const tls = () => {
  const cert = generateSelfSignedCertificate(['localhost', '127.0.0.1']);
  return { certificatePem: cert.certificatePem, privateKeyPem: cert.privateKeyPem };
};
function call(address: string, role: string) {
  return new Promise<{ code: number; payloads: Buffer[] }>((resolve, reject) => {
    const req = request(`https://${address}/aster/aster.net.Topology/peers`, {
      method: 'POST', rejectUnauthorized: false, agent: false, headers: { authorization: role },
    }, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (value) => chunks.push(value)); res.on('error', reject);
      res.on('end', () => {
        try {
          let input = Buffer.concat(chunks), code = -1;
          const payloads: Buffer[] = [];
          while (input.length) {
            const length = input.readUInt32LE(0) + 4;
            const [bytes, flags] = decodeFrame(input.subarray(0, length));
            if (flags & TRAILER) code = decodeRpcStatus(bytes).code;
            else payloads.push(Buffer.from(bytes));
            input = input.subarray(length);
          }
          resolve({ code, payloads });
        } catch (error) { reject(error); }
      });
    });
    req.setTimeout(5000, () => req.destroy(new Error('topology request timeout')));
    req.on('error', reject); req.end(encodeFrame(new Uint8Array(), END_STREAM));
  });
}
const auth = (req: { metadata: Record<string, string> }) => ({ principal: 'alice', attributes: req.metadata.authorization === 'operator' ? { 'aster.role': 'operator' } : {} });

describe('topology baseline configuration', () => {
  it('high-level opt-in is absent by default and the native role requirement gates every caller', async () => {
    for (const enabled of [false, true]) {
      const server = new AsterServer({ services: [], config: { ...OFFLINE, enableMonitoring: true }, builtinTopology: enabled, topologyRequires: { ...requirement, roles: [...requirement.roles] } });
      const listener = await server.startHttp(tls(), auth);
      try {
        expect((await call(listener.tcpAddr, 'none')).code).toBe(enabled ? 7 : 5);
        const allowed = await call(listener.tcpAddr, 'operator');
        expect(allowed.code).toBe(enabled ? 0 : 5);
        if (enabled) expect(allowed.payloads[0].length).toBeGreaterThan(0);
      } finally { await listener.close(); await server.close(); }
    }
  });
  it('owned native startup forwards policy and public mode cannot bypass Gate 3', async () => {
    const loop = await DispatchLoop.start({ config: { ...OFFLINE, enableMonitoring: true }, services: [], delivery: 'ring', builtinTopology: true, topologyRequires: { ...requirement, roles: [...requirement.roles] }, onCall: () => { throw new Error('baseline is native'); } });
    const authenticated = await HttpListener.start(loop.native, tls(), auth);
    const anonymous = await HttpListener.startPublic(loop.native, tls());
    try {
      expect((await call(authenticated.tcpAddr, 'none')).code).toBe(7);
      expect((await call(authenticated.tcpAddr, 'operator')).code).toBe(0);
      expect((await call(anonymous.tcpAddr, 'operator')).code).toBe(7);
    } finally { await authenticated.close(); await anonymous.close(); await loop.close(); }
  });
  it('owned and attached enablement reject unmonitored nodes and malformed native policy', async () => {
    const highLevel = new AsterServer({ services: [], config: OFFLINE, builtinTopology: true });
    try {
      await expect(highLevel.start()).rejects.toThrow(/monitoring/);
      await expect(highLevel.start()).rejects.toThrow(/monitoring/);
    } finally { await highLevel.close(); }
    await expect(DispatchLoop.start({ config: OFFLINE, services: [], delivery: 'ring', builtinTopology: true, onCall() {} })).rejects.toThrow(/monitoring/);
    const node = await Node.start(OFFLINE);
    try {
      await expect(DispatchLoop.start({ node: node.native, services: [], delivery: 'ring', builtinTopology: true, onCall() {} })).rejects.toThrow(/monitoring/);
      expect(() => loadNative().DispatchServer.serve(node.native, [], { delivery: 'ring', topologyRequiresJson: '{"kind":"invalid","roles":[]}' })).toThrow(/topology requirement/);
    } finally { await node.close(); }
  });
  it('huge finite topology timing returns an argument error without a native panic', async () => {
    const node = await Node.start({ ...OFFLINE, enableMonitoring: true });
    try {
      expect(() => node.topology().enableShared(new Uint8Array(32), { positionRefreshMs: Number.MAX_VALUE })).toThrow(/duration range/);
      await node.topology().enableShared(new Uint8Array(32), { positionRefreshMs: 0, timing: { minHoldMs: 0 } });
      const snap = await node.topology().snapshot();
      expect(snap.separated(node.id, '00'.repeat(32))).toBe('unknown');
    } finally { await node.close(); }
  });
});
