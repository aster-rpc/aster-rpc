/**
 * Strict-mode tests for JsonCodec.
 *
 * The producer owns the contract: any JSON dict key that doesn't map
 * to a declared field on the expected @WireType class MUST raise
 * ContractViolationError. The codec does not silently drop or rename
 * keys, no matter how innocuous they look. These tests pin that
 * behaviour at every depth.
 *
 * Mirrors tests/python/test_codec_strict.py.
 */

import { describe, it, expect, beforeEach } from 'vitest';
import {
  JsonCodec,
  WireType,
  ContractViolationError,
  RpcError,
  StatusCode,
} from '@aster-rpc/aster';

// ── Test wire types ──────────────────────────────────────────────────────────

@WireType('test.codec/StatusRequest')
class StatusRequest {
  agentId: string = '';
  region: string = '';
  constructor(init?: Partial<StatusRequest>) { if (init) Object.assign(this, init); }
}

@WireType('test.codec/Tag')
class Tag {
  key: string = '';
  value: string = '';
  constructor(init?: Partial<Tag>) { if (init) Object.assign(this, init); }
}

@WireType('test.codec/StatusResponse')
class StatusResponse {
  agentId: string = '';
  status: string = '';
  // Sample non-empty array so the validator can introspect element type
  tags: Tag[] = [new Tag()];
  // Non-null nested default so the validator can introspect the field type
  nested: Tag = new Tag();
  constructor(init?: Partial<StatusResponse>) { if (init) Object.assign(this, init); }
}

@WireType('test.codec/Empty')
class Empty {
  constructor(_init?: Partial<Empty>) {}
}

// ── Helpers ──────────────────────────────────────────────────────────────────

function encode(obj: unknown): Uint8Array {
  return new TextEncoder().encode(JSON.stringify(obj));
}

let codec: JsonCodec;
beforeEach(() => {
  codec = new JsonCodec();
});

// ── Top-level violations ─────────────────────────────────────────────────────

describe('JsonCodec strict mode', () => {
  it('rejects an unexpected top-level key', () => {
    const payload = encode({ agentId: 'ok', bogus: 1 });
    expect(() => codec.decode(payload, StatusRequest)).toThrow(ContractViolationError);
  });

  it('error names the offending key + class', () => {
    const payload = encode({ agentId: 'ok', bogus: 1 });
    let err: ContractViolationError | null = null;
    try {
      codec.decode(payload, StatusRequest);
    } catch (e) {
      err = e as ContractViolationError;
    }
    expect(err).not.toBeNull();
    expect(err!.code).toBe(StatusCode.CONTRACT_VIOLATION);
    expect(err!.message).toContain('bogus');
    expect(err!.message).toContain('StatusRequest');
    expect(err!.details.expected_class).toBe('StatusRequest');
    expect(err!.details.unexpected_fields).toContain('bogus');
  });

  it('valid payload decodes cleanly', () => {
    const payload = encode({ agentId: 'edge-7', region: 'us-east' });
    const obj = codec.decode(payload, StatusRequest) as Record<string, unknown>;
    expect(obj.agentId).toBe('edge-7');
    expect(obj.region).toBe('us-east');
  });

  it('missing field is permitted (defaults apply at construction)', () => {
    // The codec returns the raw parsed object. The handler is
    // responsible for hydrating it into a class instance with
    // defaults if needed. Strict mode only catches EXTRA fields.
    const payload = encode({ agentId: 'edge-7' });
    expect(() => codec.decode(payload, StatusRequest)).not.toThrow();
  });

  it('empty object against an empty wire type is valid', () => {
    const payload = encode({});
    expect(() => codec.decode(payload, Empty)).not.toThrow();
  });

  it('empty wire type rejects any field', () => {
    const payload = encode({ anything: 1 });
    expect(() => codec.decode(payload, Empty)).toThrow(ContractViolationError);
  });
});

// ── Nested violations ────────────────────────────────────────────────────────

describe('JsonCodec strict mode -- nested', () => {
  it('catches a bad field in a nested @WireType object', () => {
    const payload = encode({
      agentId: 'ok',
      status: 'running',
      tags: [{ key: 'k', value: 'v' }],
      nested: { key: 'n', value: 'v', rogue: 1 },
    });
    let err: ContractViolationError | null = null;
    try {
      codec.decode(payload, StatusResponse);
    } catch (e) {
      err = e as ContractViolationError;
    }
    expect(err).not.toBeNull();
    expect(err!.message).toContain('rogue');
    // Dotted path should mention the nested field
    expect(err!.message).toMatch(/nested|Tag/);
  });

  it('catches a bad field in a list[@WireType] element', () => {
    const payload = encode({
      agentId: 'ok',
      status: 'running',
      tags: [
        { key: 'good', value: '1' },
        { key: 'bad', value: '2', snuckIn: true },
      ],
      nested: { key: 'n', value: 'v' },
    });
    let err: ContractViolationError | null = null;
    try {
      codec.decode(payload, StatusResponse);
    } catch (e) {
      err = e as ContractViolationError;
    }
    expect(err).not.toBeNull();
    expect(err!.message).toContain('snuckIn');
    expect(err!.message).toMatch(/\[1\]|tags/);
  });

  it('valid nested payload passes', () => {
    const payload = encode({
      agentId: 'ok',
      status: 'running',
      tags: [{ key: 'k', value: 'v' }],
      nested: { key: 'n', value: 'v' },
    });
    expect(() => codec.decode(payload, StatusResponse)).not.toThrow();
  });
});

// ── Sanitization ─────────────────────────────────────────────────────────────

describe('JsonCodec strict mode -- sanitization', () => {
  it('escapes control characters in unexpected key names', () => {
    // Bad key with newline + ANSI escape -- the kind of thing a
    // malicious client might inject to corrupt server logs
    const badKey = 'fake\nINFO server compromised\u001b[31m';
    const payload = encode({ agentId: 'ok', [badKey]: 1 });
    let err: ContractViolationError | null = null;
    try {
      codec.decode(payload, StatusRequest);
    } catch (e) {
      err = e as ContractViolationError;
    }
    expect(err).not.toBeNull();
    // Raw newline must NOT appear unescaped
    const messageWithoutEscapes = err!.message.replace(/\\n/g, '');
    expect(messageWithoutEscapes).not.toContain('\n');
    // And the escaped form should be present
    expect(err!.message).toMatch(/\\n|\\u/);
  });

  it('truncates very long key names', () => {
    const hugeKey = 'x'.repeat(5000);
    const payload = encode({ agentId: 'ok', [hugeKey]: 1 });
    let err: ContractViolationError | null = null;
    try {
      codec.decode(payload, StatusRequest);
    } catch (e) {
      err = e as ContractViolationError;
    }
    expect(err).not.toBeNull();
    // The huge key shouldn't appear in full
    expect(err!.message).not.toContain('x'.repeat(200));
    expect(err!.message).toContain('truncated');
  });

  it('caps the number of unexpected keys listed', () => {
    const badPayload: Record<string, unknown> = { agentId: 'ok' };
    for (let i = 0; i < 100; i++) badPayload[`bad${i}`] = i;
    const payload = encode(badPayload);
    let err: ContractViolationError | null = null;
    try {
      codec.decode(payload, StatusRequest);
    } catch (e) {
      err = e as ContractViolationError;
    }
    expect(err).not.toBeNull();
    expect(err!.message).toContain('more');
  });
});

// ── Status code identity ─────────────────────────────────────────────────────

describe('CONTRACT_VIOLATION status code', () => {
  it('lives in the Aster-native 100+ range', () => {
    expect(StatusCode.CONTRACT_VIOLATION).toBeGreaterThanOrEqual(100);
  });

  it('does not collide with any gRPC-mirrored code', () => {
    const grpcCodes = [
      StatusCode.OK,
      StatusCode.CANCELLED,
      StatusCode.UNKNOWN,
      StatusCode.INVALID_ARGUMENT,
      StatusCode.DEADLINE_EXCEEDED,
      StatusCode.NOT_FOUND,
      StatusCode.ALREADY_EXISTS,
      StatusCode.PERMISSION_DENIED,
      StatusCode.RESOURCE_EXHAUSTED,
      StatusCode.FAILED_PRECONDITION,
      StatusCode.ABORTED,
      StatusCode.OUT_OF_RANGE,
      StatusCode.UNIMPLEMENTED,
      StatusCode.INTERNAL,
      StatusCode.UNAVAILABLE,
      StatusCode.DATA_LOSS,
      StatusCode.UNAUTHENTICATED,
    ];
    expect(grpcCodes).not.toContain(StatusCode.CONTRACT_VIOLATION);
  });

  it('ContractViolationError IS-A RpcError', () => {
    const err = new ContractViolationError('test');
    expect(err).toBeInstanceOf(RpcError);
    expect(err.code).toBe(StatusCode.CONTRACT_VIOLATION);
  });
});

// ── Cache behaviour (constructor side effects run once) ──────────────────────

describe('JsonCodec strict mode -- introspection cache', () => {
  it('does not re-instantiate the class on every decode', () => {
    let constructorCalls = 0;

    @WireType('test.codec/Counter')
    class Counter {
      value: number = 0;
      constructor(init?: Partial<Counter>) {
        constructorCalls++;
        if (init) Object.assign(this, init);
      }
    }

    // First decode -- the validator runs `new Counter()` once to
    // build the field-name set. constructorCalls should bump.
    const initialCalls = constructorCalls;
    codec.decode(encode({ value: 1 }), Counter);
    const afterFirst = constructorCalls;
    expect(afterFirst).toBeGreaterThan(initialCalls);

    // Second + third decode -- the cached shape is used. No new
    // constructor calls from the validator.
    codec.decode(encode({ value: 2 }), Counter);
    codec.decode(encode({ value: 3 }), Counter);
    expect(constructorCalls).toBe(afterFirst);
  });

  it('classes that throw on default construction fall back permissive', () => {
    class NotConstructible {
      constructor(required: string) {
        if (!required) throw new Error('positional arg required');
      }
    }

    // The validator should catch the constructor failure and skip
    // validation entirely (return permissive). It should NOT re-throw.
    const payload = encode({ anyKey: 1, anything: 'else' });
    expect(() =>
      codec.decode(payload, NotConstructible as any)
    ).not.toThrow(ContractViolationError);
  });
});

// ── Edge cases the user flagged ──────────────────────────────────────────────

describe('JsonCodec strict mode -- edge cases', () => {
  it('handles primitive enum field values without false positives', () => {
    enum Status { Idle = 'idle', Running = 'running' }

    @WireType('test.codec/EnumWire')
    class EnumWire {
      status: Status = Status.Idle;
      count: number = 0;
      constructor(init?: Partial<EnumWire>) { if (init) Object.assign(this, init); }
    }

    // Valid enum value -- should pass
    const payload = encode({ status: 'running', count: 3 });
    expect(() => codec.decode(payload, EnumWire)).not.toThrow();

    // Top-level extra still rejected
    const bad = encode({ status: 'idle', count: 0, rogue: true });
    expect(() => codec.decode(bad, EnumWire)).toThrow(ContractViolationError);
  });

  it('handles numeric enum field values', () => {
    enum Severity { Low, Medium, High }

    @WireType('test.codec/NumEnumWire')
    class NumEnumWire {
      severity: Severity = Severity.Low;
      label: string = '';
      constructor(init?: Partial<NumEnumWire>) { if (init) Object.assign(this, init); }
    }

    const payload = encode({ severity: 2, label: 'critical' });
    expect(() => codec.decode(payload, NumEnumWire)).not.toThrow();
  });

  it('does not try to recurse into Date field values', () => {
    @WireType('test.codec/Timestamped')
    class Timestamped {
      when: Date = new Date(0);
      label: string = '';
      constructor(init?: Partial<Timestamped>) { if (init) Object.assign(this, init); }
    }

    // Date is serialized as an ISO string on the wire. The validator
    // should not try to introspect it as a class.
    const payload = encode({ when: '2026-04-11T00:00:00Z', label: 'now' });
    expect(() => codec.decode(payload, Timestamped)).not.toThrow();
  });

  it('does NOT recurse into nested types behind a null default (documented limitation)', () => {
    @WireType('test.codec/WithOptional')
    class WithOptional {
      name: string = '';
      meta: Tag | null = null;  // null default -- validator can't see Tag
      constructor(init?: Partial<WithOptional>) { if (init) Object.assign(this, init); }
    }

    // The nested Tag has a `rogue` field that SHOULD be a contract
    // violation, but the validator can't introspect through a null
    // default. Top-level still validates -- name is fine.
    const payload = encode({
      name: 'ok',
      meta: { key: 'k', value: 'v', rogue: 1 },
    });
    // Documented limitation: this does NOT throw because the validator
    // can't see the type behind a null default. If this ever starts
    // throwing, the validator got smarter -- update the test to
    // assert it throws and remove the documented-limitation note from
    // codec.ts.
    expect(() => codec.decode(payload, WithOptional)).not.toThrow();
  });

  it('does NOT recurse into nested types behind an empty array default (documented limitation)', () => {
    @WireType('test.codec/WithEmptyArray')
    class WithEmptyArray {
      name: string = '';
      // Empty array default -- validator can't sample element type
      tags: Tag[] = [];
      constructor(init?: Partial<WithEmptyArray>) { if (init) Object.assign(this, init); }
    }

    const payload = encode({
      name: 'ok',
      tags: [{ key: 'k', value: 'v', rogue: 1 }],
    });
    // Same documented limitation as the null case above
    expect(() => codec.decode(payload, WithEmptyArray)).not.toThrow();
  });
});
