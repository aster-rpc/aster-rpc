/**
 * AsterConfig — configuration resolution.
 *
 * Three-layer resolution order:
 * 1. Built-in defaults
 * 2. Config file (aster.config.ts, aster.toml, .asterrc)
 * 3. Environment variables (ASTER_* prefix, always win)
 */

/** Aster configuration. */
export interface AsterConfig {
  // Trust
  rootPubkey?: Uint8Array;
  rootPubkeyFile?: string;
  enrollmentCredentialFile?: string;
  allowAllConsumers: boolean;
  allowAllProducers: boolean;

  // Connect
  endpointAddr?: string;

  // Storage
  storagePath?: string;

  // Health
  healthPort: number;
  healthHost: string;

  // Network
  secretKey?: Uint8Array;
  relayMode?: string;
  bindAddr?: string;
  enableMonitoring: boolean;
  enableHooks: boolean;
  hookTimeoutMs: number;

  // Logging
  logFormat: 'json' | 'text';
  logLevel: 'debug' | 'info' | 'warning' | 'error';
  logMask: boolean;

  // Identity
  identityFile?: string;
}

function envBool(key: string, fallback: boolean): boolean {
  const v = process.env[key]?.toLowerCase();
  if (!v) return fallback;
  return ['true', '1', 'yes', 'on'].includes(v);
}

function envInt(key: string, fallback: number): number {
  const v = process.env[key];
  if (!v) return fallback;
  const n = parseInt(v, 10);
  return isNaN(n) ? fallback : n;
}

function envString(key: string): string | undefined {
  return process.env[key] || undefined;
}

function envBytes(key: string): Uint8Array | undefined {
  const hex = process.env[key];
  if (!hex) return undefined;
  return new Uint8Array(Buffer.from(hex, 'hex'));
}

/** Load config from environment variables with built-in defaults. */
export function configFromEnv(): AsterConfig {
  return {
    rootPubkey: envBytes('ASTER_ROOT_PUBKEY'),
    rootPubkeyFile: envString('ASTER_ROOT_PUBKEY_FILE'),
    enrollmentCredentialFile: envString('ASTER_ENROLLMENT_CREDENTIAL'),
    allowAllConsumers: envBool('ASTER_ALLOW_ALL_CONSUMERS', false),
    allowAllProducers: envBool('ASTER_ALLOW_ALL_PRODUCERS', true),
    endpointAddr: envString('ASTER_ENDPOINT_ADDR'),
    storagePath: envString('ASTER_STORAGE_PATH'),
    healthPort: envInt('ASTER_HEALTH_PORT', 0),
    healthHost: envString('ASTER_HEALTH_HOST') ?? '127.0.0.1',
    secretKey: envBytes('ASTER_SECRET_KEY'),
    relayMode: envString('ASTER_RELAY_MODE'),
    bindAddr: envString('ASTER_BIND_ADDR'),
    enableMonitoring: envBool('ASTER_ENABLE_MONITORING', false),
    enableHooks: envBool('ASTER_ENABLE_HOOKS', false),
    hookTimeoutMs: envInt('ASTER_HOOK_TIMEOUT_MS', 5000),
    logFormat: (envString('ASTER_LOG_FORMAT') ?? 'text') as 'json' | 'text',
    logLevel: (envString('ASTER_LOG_LEVEL') ?? 'info') as any,
    logMask: envBool('ASTER_LOG_MASK', true),
    identityFile: envString('ASTER_IDENTITY_FILE'),
  };
}

/** Print resolved config (masks sensitive values). */
export function printConfig(config: AsterConfig): void {
  const mask = (v: unknown) => v ? '****' : '<not set>';
  console.log(`  [trust]`);
  console.log(`    root_pubkey             : ${config.rootPubkey ? `${Buffer.from(config.rootPubkey).toString('hex').slice(0, 8)}...` : '<not set>'}`);
  console.log(`    allow_all_consumers     : ${config.allowAllConsumers}`);
  console.log(`    allow_all_producers     : ${config.allowAllProducers}`);
  console.log(`  [connect]`);
  console.log(`    endpoint_addr           : ${config.endpointAddr ?? '<not set>'}`);
  console.log(`  [network]`);
  console.log(`    secret_key              : ${mask(config.secretKey)}`);
  console.log(`    relay_mode              : ${config.relayMode ?? '<default>'}`);
  console.log(`    bind_addr               : ${config.bindAddr ?? '<any>'}`);
  console.log(`  [storage]`);
  console.log(`    path                    : ${config.storagePath ?? '<in-memory>'}`);
  console.log(`  [logging]`);
  console.log(`    format                  : ${config.logFormat}`);
  console.log(`    level                   : ${config.logLevel}`);
  console.log(`  [health]`);
  console.log(`    port                    : ${config.healthPort || 'disabled'}`);
  console.log(`    host                    : ${config.healthHost}`);
}
