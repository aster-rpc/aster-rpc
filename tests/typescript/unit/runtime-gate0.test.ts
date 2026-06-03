import { describe, expect, it } from 'vitest';

import { gate0EndpointConfig } from '../../../bindings/typescript/packages/aster/src/gate0.js';
import {
  CONSUMER_ADMISSION_ALPN,
  DELEGATED_ADMISSION_ALPN,
  MeshEndpointHook,
  PRODUCER_ADMISSION_ALPN,
} from '../../../bindings/typescript/packages/aster/src/trust/hooks.js';

describe('Gate0 endpoint config', () => {
  it('uses fail-closed hooks for protected server mode', () => {
    expect(gate0EndpointConfig(true)).toEqual({
      enableHooks: true,
      hookTimeoutMs: 5000,
      hookFailureMode: 'fail_closed',
    });
  });

  it('does not enable hooks when Gate0 is not required', () => {
    expect(gate0EndpointConfig(false)).toBeUndefined();
  });
});

describe('MeshEndpointHook admission ALPNs', () => {
  it('allows producer, consumer, and delegated admission before peer enrollment', () => {
    const hook = new MeshEndpointHook(false);

    expect(hook.shouldAllow('unenrolled-peer', PRODUCER_ADMISSION_ALPN)).toBe(true);
    expect(hook.shouldAllow('unenrolled-peer', CONSUMER_ADMISSION_ALPN)).toBe(true);
    expect(hook.shouldAllow('unenrolled-peer', DELEGATED_ADMISSION_ALPN)).toBe(true);
  });
});
