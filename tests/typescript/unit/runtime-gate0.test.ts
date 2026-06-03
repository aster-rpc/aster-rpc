import { afterEach, describe, expect, it } from 'vitest';

import { AsterServer } from '../../../bindings/typescript/packages/aster/src/runtime.js';
import { gate0EndpointConfig } from '../../../bindings/typescript/packages/aster/src/gate0.js';
import {
  CONSUMER_ADMISSION_ALPN,
  DELEGATED_ADMISSION_ALPN,
  MeshEndpointHook,
  PRODUCER_ADMISSION_ALPN,
} from '../../../bindings/typescript/packages/aster/src/trust/hooks.js';

function serverAllowAllConsumers(server: AsterServer): boolean {
  return (server as unknown as { _allowAllConsumers: boolean })._allowAllConsumers;
}

function serverProtectedEndpointConfig(server: AsterServer) {
  return gate0EndpointConfig(!serverAllowAllConsumers(server));
}

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

describe('AsterServer Gate0 config resolution', () => {
  const savedAllowAllConsumers = process.env.ASTER_ALLOW_ALL_CONSUMERS;

  afterEach(() => {
    if (savedAllowAllConsumers === undefined) {
      delete process.env.ASTER_ALLOW_ALL_CONSUMERS;
    } else {
      process.env.ASTER_ALLOW_ALL_CONSUMERS = savedAllowAllConsumers;
    }
  });

  it('uses config.allowAllConsumers when the constructor option is omitted', () => {
    const server = new AsterServer({
      services: [],
      config: { allowAllConsumers: false },
    });

    expect(serverAllowAllConsumers(server)).toBe(false);
    expect(serverProtectedEndpointConfig(server)).toEqual({
      enableHooks: true,
      hookTimeoutMs: 5000,
      hookFailureMode: 'fail_closed',
    });
  });

  it('uses ASTER_ALLOW_ALL_CONSUMERS when the constructor option is omitted', () => {
    process.env.ASTER_ALLOW_ALL_CONSUMERS = 'false';

    const server = new AsterServer({ services: [] });

    expect(serverAllowAllConsumers(server)).toBe(false);
    expect(serverProtectedEndpointConfig(server)).toEqual({
      enableHooks: true,
      hookTimeoutMs: 5000,
      hookFailureMode: 'fail_closed',
    });
  });

  it('lets the explicit constructor option override config', () => {
    const server = new AsterServer({
      services: [],
      allowAllConsumers: true,
      config: { allowAllConsumers: false },
    });

    expect(serverAllowAllConsumers(server)).toBe(true);
    expect(serverProtectedEndpointConfig(server)).toBeUndefined();
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
