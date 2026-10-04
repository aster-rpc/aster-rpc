/**
 * T4: decorators → `DynamicService` JSON (design §1.5) and the contract id
 * Rust derives from it, against `conformance/vectors/dynamic-service.json`.
 *
 * The TS-built services below mirror the vector's MissionControl v1 / v2 and
 * AgentSession cases (same names, patterns, `requires`, timeouts,
 * idempotency; type hashes taken from the vector, as `aster-gen` would stamp
 * them). Their lowered form and `fromServiceInfo` must both produce the
 * vector's `contract_id` -- the same id Rust and Python compute.
 */

import { describe, it, expect, beforeAll } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  Service,
  Rpc,
  ServerStream,
  ClientStream,
  BidiStream,
  getServiceInfo,
  lowerService,
  lowerServiceJson,
  dynamicContractId,
  fromServiceInfo,
  contractIdFromContract,
  contractIdFromService,
  setNativeContract,
} from '@aster-rpc/aster';
import { loadNative } from '../../../bindings/typescript/packages/aster/src/native.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const VECTOR = JSON.parse(
  readFileSync(resolve(__dirname, '../../../conformance/vectors/dynamic-service.json'), 'utf8'),
);

function vectorCase(name: string): any {
  const c = VECTOR.cases.find((x: any) => x.name === name);
  if (!c) throw new Error(`no vector case ${name}`);
  return c;
}

function unhex(h: string): Uint8Array {
  return Uint8Array.from(Buffer.from(h, 'hex'));
}

/** Stamp the vector's type hashes onto a decorated service (what aster-gen does). */
function stampHashes(cls: Function, caseName: string): void {
  const svc = vectorCase(caseName).service;
  const info = getServiceInfo(cls)!;
  for (const m of svc.methods) {
    const mi = info.methods.get(m.name)!;
    expect(mi, m.name).toBeDefined();
    mi.requestTypeHash = unhex(m.request.hash);
    mi.responseTypeHash = unhex(m.response.hash);
  }
}

@Service({ name: 'MissionControl', version: 1 })
class MissionControlV1 {
  @Rpc() async getStatus(_r: unknown): Promise<unknown> { return {}; }
  @Rpc() async submitLog(_r: unknown): Promise<unknown> { return {}; }
  @ServerStream() async *tailLogs(_r: unknown): AsyncGenerator<unknown> { /* */ }
  @ClientStream() async ingestMetrics(_r: AsyncIterable<unknown>): Promise<unknown> { return {}; }
  @BidiStream() async *runCommand(_r: AsyncIterable<unknown>): AsyncGenerator<unknown> { /* */ }
}

@Service({ name: 'MissionControl', version: 2 })
class MissionControlV2 {
  @Rpc({ idempotent: true, requires: { kind: 'role', roles: ['status'] } })
  async getStatus(_r: unknown): Promise<unknown> { return {}; }
  @Rpc() async submitLog(_r: unknown): Promise<unknown> { return {}; }
  @ServerStream({ requires: { kind: 'any_of', roles: ['logs', 'admin'] } })
  async *tailLogs(_r: unknown): AsyncGenerator<unknown> { /* */ }
  // A bare role string is one role of kind `role`.
  @ClientStream({ requires: 'ingest' })
  async ingestMetrics(_r: AsyncIterable<unknown>): Promise<unknown> { return {}; }
}

@Service({
  name: 'AgentSession',
  version: 1,
  scoped: 'session',
  requires: { kind: 'any_of', roles: ['agent', 'admin'] },
})
class AgentSession {
  @Rpc({ timeout: 30 }) async register(_r: unknown): Promise<unknown> { return {}; }
  @Rpc({ timeout: 2.5, idempotent: true }) async heartbeat(_r: unknown): Promise<unknown> { return {}; }
  @BidiStream({ requires: { kind: 'role', roles: ['admin'] } })
  async *runCommand(_r: AsyncIterable<unknown>): AsyncGenerator<unknown> { /* */ }
}

const CASES: [string, Function][] = [
  ['mission-control: MissionControl v1', MissionControlV1],
  ['mission-control: MissionControl v2 with per-method requires', MissionControlV2],
  ['mission-control: AgentSession v1 (session scope, service requires, timeouts)', AgentSession],
];

describe('decorator lowering → DynamicService (dynamic-service.json)', () => {
  beforeAll(() => {
    setNativeContract(loadNative() as any);
    for (const [name, cls] of CASES) stampHashes(cls, name);
  });

  it('every valid vector case: Rust contract id through the NAPI binding', () => {
    for (const c of VECTOR.cases) {
      expect(dynamicContractId(c.service), c.name).toBe(c.expected_contract_id);
    }
  });

  it('every invalid vector case is refused', () => {
    for (const c of VECTOR.invalid) {
      const json = typeof c.json === 'string' ? c.json : JSON.stringify(c.service ?? c.json);
      expect(() => loadNative().dynamicServiceContractId(json), c.name).toThrow();
    }
  });

  for (const [name, cls] of CASES) {
    it(`${name}: lowered id and fromServiceInfo id equal the vector`, () => {
      const want = vectorCase(name).expected_contract_id;
      expect(dynamicContractId(cls)).toBe(want);
      expect(contractIdFromContract(fromServiceInfo(getServiceInfo(cls)! as any))).toBe(want);
      expect(contractIdFromService(cls as any)).toBe(want);
    });
  }

  it('lowers requires, timeouts, idempotency, scope and method order', () => {
    const v2 = lowerService(MissionControlV2);
    expect(v2.methods.map((m) => m.name)).toEqual(['getStatus', 'submitLog', 'tailLogs', 'ingestMetrics']);
    expect(v2.methods[0]).toMatchObject({ pattern: 'unary', idempotent: true, requires: { kind: 'role', roles: ['status'] } });
    expect(v2.methods[2]).toMatchObject({ pattern: 'server_stream', requires: { kind: 'any_of', roles: ['logs', 'admin'] } });
    expect(v2.methods[3]).toMatchObject({ pattern: 'client_stream', requires: { kind: 'role', roles: ['ingest'] } });
    expect(v2.methods[1]!.requires).toBeUndefined();
    const s = lowerService(AgentSession);
    expect(s.scope).toBe('session');
    expect(s.requires).toEqual({ kind: 'any_of', roles: ['agent', 'admin'] });
    expect(s.methods.map((m) => m.default_timeout)).toEqual([30, 2.5, undefined]);
    expect(s.serialization_modes).toEqual(['xlang']);
  });

  it('requires is part of the contract id', () => {
    const plain = JSON.parse(lowerServiceJson(MissionControlV2));
    for (const m of plain.methods) delete m.requires;
    expect(dynamicContractId(plain)).not.toBe(dynamicContractId(MissionControlV2));
  });

  it('a service without codegen hashes lowers to zero hashes (same id as fromServiceInfo)', () => {
    @Service({ name: 'Plain', version: 3 })
    class Plain {
      @Rpc({ timeout: 1.5 }) async a(_r: unknown): Promise<unknown> { return {}; }
    }
    const spec = lowerService(Plain);
    expect(spec.methods[0]!.request.hash).toBe('0'.repeat(64));
    expect(dynamicContractId(Plain)).toBe(contractIdFromContract(fromServiceInfo(getServiceInfo(Plain)! as any)));
  });
});
