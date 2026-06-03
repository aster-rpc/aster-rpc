const GATE0_HOOK_TIMEOUT_MS = 5000;

export interface Gate0EndpointConfig {
  enableHooks: true;
  hookTimeoutMs: number;
  hookFailureMode: 'fail_closed';
}

export function gate0EndpointConfig(required: boolean): Gate0EndpointConfig | undefined {
  if (!required) {
    return undefined;
  }
  return {
    enableHooks: true,
    hookTimeoutMs: GATE0_HOOK_TIMEOUT_MS,
    hookFailureMode: 'fail_closed',
  };
}
