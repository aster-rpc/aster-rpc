import { describe, it, expect, beforeAll } from 'vitest';
import {
  canonicalXlangBytes,
  setNativeContract,
  ScopeKind,
  MethodPattern,
  CapabilityKind,
  type ServiceContract,
} from '@aster-rpc/aster';
import { createHash } from 'node:crypto';

// -- Mock native contract binding for testing ---------------------------------
// In production, the NAPI-RS addon provides these. For unit tests without
// a native build, we mock using the golden vectors from the spec (Appendix B).

const GOLDEN: Record<string, { canonical: string; hash: string }> = {};

function fromHex(h: string): Uint8Array {
  const bytes = new Uint8Array(h.length / 2);
  for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(h.slice(i * 2, i * 2 + 2), 16);
  return bytes;
}

function toHex(data: Uint8Array): string {
  return Array.from(data, b => b.toString(16).padStart(2, '0')).join('');
}

// Register golden vectors by their JSON key
function registerGolden(json: string, canonical: string, hash: string) {
  GOLDEN[json] = { canonical, hash };
}

beforeAll(() => {
  // Golden Vector 1: Echo
  registerGolden(
    '{"name":"Echo","version":1,"methods":[{"name":"echo","pattern":"unary","request_type":"0000000000000000000000000000000000000000000000000000000000000000","response_type":"0000000000000000000000000000000000000000000000000000000000000000","idempotent":false,"default_timeout":0,"requires":null}],"serialization_modes":[],"scoped":"shared","requires":null}',
    '124563686f02010c126563686f00200000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000fd000c00fd',
    '73ac6c9e70c7dcdd825221a4eb1d1ac9432d890685e65987f7d8d74c8d3191be',
  );

  setNativeContract({
    canonicalBytesFromJson(_typeName: string, json: string): Uint8Array {
      const g = GOLDEN[json];
      if (g) return fromHex(g.canonical);
      return new TextEncoder().encode(json); // fallback for non-golden
    },
    computeTypeHash(data: Uint8Array): Uint8Array {
      return new Uint8Array(createHash('sha256').update(data).digest());
    },
    computeContractIdFromJson(json: string): string {
      const g = GOLDEN[json];
      if (g) return g.hash;
      return createHash('sha256').update(json).digest('hex');
    },
  });
});

// -- Golden vector conformance ------------------------------------------------

describe('golden vectors', () => {
  it('Vector 1: Echo produces correct canonical bytes (94 bytes)', () => {
    const contract: ServiceContract = {
      name: 'Echo', version: 1, scoped: ScopeKind.SHARED,
      methods: [{
        name: 'echo', pattern: MethodPattern.UNARY,
        requestType: new Uint8Array(32), responseType: new Uint8Array(32),
        idempotent: false, defaultTimeout: 0,
      }],
      serializationModes: [],
    };
    const bytes = canonicalXlangBytes(contract);
    expect(toHex(bytes)).toBe(
      '124563686f02010c126563686f00200000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000fd000c00fd'
    );
    expect(bytes.length).toBe(94);
  });
});

// -- JSON serialization (serde compat) ----------------------------------------

describe('contract JSON serialization', () => {
  it('deterministic for same contract', () => {
    const contract: ServiceContract = {
      name: 'Test', version: 1, scoped: ScopeKind.SHARED,
      methods: [], serializationModes: [],
    };
    expect(toHex(canonicalXlangBytes(contract))).toBe(toHex(canonicalXlangBytes(contract)));
  });

  it('different contracts differ', () => {
    const c1: ServiceContract = { name: 'A', version: 1, scoped: 0, methods: [], serializationModes: [] };
    const c2: ServiceContract = { name: 'B', version: 1, scoped: 0, methods: [], serializationModes: [] };
    expect(toHex(canonicalXlangBytes(c1))).not.toBe(toHex(canonicalXlangBytes(c2)));
  });

  it('handles capability requirements', () => {
    const contract: ServiceContract = {
      name: 'Sec', version: 1, scoped: 0,
      methods: [{
        name: 'm', pattern: MethodPattern.UNARY,
        requestType: new Uint8Array(32), responseType: new Uint8Array(32),
        idempotent: true, defaultTimeout: 30000,
        requires: { kind: CapabilityKind.ANY_OF, roles: ['reader'] },
      }],
      serializationModes: [],
    };
    expect(canonicalXlangBytes(contract).length).toBeGreaterThan(0);
  });

  it('handles session-scoped', () => {
    const contract: ServiceContract = {
      name: 'Chat', version: 1, scoped: ScopeKind.SESSION,
      methods: [], serializationModes: [],
    };
    expect(canonicalXlangBytes(contract).length).toBeGreaterThan(0);
  });
});
