/**
 * `registerGenerated` must keep the `requires` the decorators recorded:
 * `aster-gen` emits `requires: undefined` (it does not evaluate expressions
 * such as `Role.STATUS` or `anyOf(...)`), and dropping the decorator's
 * value would silently open every gated method of an enrolled server.
 */

import { describe, expect, it } from 'vitest';
import { Rpc, Service, SERVICE_INFO_KEY, anyOf, registerGenerated, RpcPattern } from '@aster-rpc/aster';

@Service({ name: 'Gated', version: 1, requires: 'svc.role' })
class Gated {
  @Rpc({ requires: 'ops.status' })
  async status(): Promise<void> {}

  @Rpc({ requires: anyOf('ops.logs', 'ops.admin') })
  async logs(): Promise<void> {}

  @Rpc()
  async open(): Promise<void> {}
}

const method = (name: string) => ({
  name,
  pattern: RpcPattern.UNARY,
  requestType: undefined,
  responseType: undefined,
  acceptsCtx: false,
  idempotent: false,
  timeout: undefined,
  serialization: undefined,
  requires: undefined,
  metadata: undefined,
  requestFields: [],
  responseFields: [],
  requestTypeHash: undefined,
  responseTypeHash: undefined,
});

describe('registerGenerated keeps decorator requires', () => {
  it('a generated entry without requires does not open gated methods', () => {
    registerGenerated({
      SERVICES: [{
        ctor: Gated,
        name: 'Gated',
        version: 1,
        scoped: 'shared',
        serializationModes: ['xlang'],
        requires: undefined,
        metadata: undefined,
        methods: [method('status'), method('logs'), method('open')],
      }] as any,
      WIRE_TYPES: [],
    });
    const info = (Gated as any)[SERVICE_INFO_KEY];
    expect(info.requires).toBe('svc.role');
    expect(info.methods.get('status').requires).toBe('ops.status');
    expect(info.methods.get('logs').requires).toEqual(anyOf('ops.logs', 'ops.admin'));
    expect(info.methods.get('open').requires).toBeUndefined();
  });
});
