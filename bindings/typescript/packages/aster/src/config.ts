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

/**
 * Load config from a TOML file, then overlay env vars.
 *
 * Supports aster.toml with sections: [trust], [connect], [storage],
 * [network], [logging], [health].
 */
export function configFromFile(filePath: string): AsterConfig {
  const { readFileSync } = require('node:fs');
  const text = readFileSync(filePath, 'utf-8');
  const toml = parseSimpleToml(text);
  const base = configFromEnv(); // env always wins

  // Merge TOML values (only if env didn't set them)
  const trust = toml.trust as Record<string, unknown> | undefined;
  if (trust) {
    if (!base.rootPubkey && trust.root_pubkey) {
      base.rootPubkey = new Uint8Array(Buffer.from(trust.root_pubkey as string, 'hex'));
    }
    if (!base.rootPubkeyFile && trust.root_pubkey_file) {
      base.rootPubkeyFile = trust.root_pubkey_file as string;
    }
    if (!base.enrollmentCredentialFile && trust.enrollment_credential) {
      base.enrollmentCredentialFile = trust.enrollment_credential as string;
    }
    if (trust.allow_all_consumers !== undefined && !process.env.ASTER_ALLOW_ALL_CONSUMERS) {
      base.allowAllConsumers = !!trust.allow_all_consumers;
    }
    if (trust.allow_all_producers !== undefined && !process.env.ASTER_ALLOW_ALL_PRODUCERS) {
      base.allowAllProducers = !!trust.allow_all_producers;
    }
  }

  const connect = toml.connect as Record<string, unknown> | undefined;
  if (connect) {
    if (!base.endpointAddr && connect.endpoint_addr) {
      base.endpointAddr = connect.endpoint_addr as string;
    }
  }

  const storage = toml.storage as Record<string, unknown> | undefined;
  if (storage) {
    if (!base.storagePath && storage.path) {
      base.storagePath = storage.path as string;
    }
  }

  const network = toml.network as Record<string, unknown> | undefined;
  if (network) {
    if (!base.secretKey && network.secret_key) {
      base.secretKey = new Uint8Array(Buffer.from(network.secret_key as string, 'base64'));
    }
    if (!base.relayMode && network.relay_mode) {
      base.relayMode = network.relay_mode as string;
    }
    if (!base.bindAddr && network.bind_addr) {
      base.bindAddr = network.bind_addr as string;
    }
    if (network.enable_monitoring !== undefined && !process.env.ASTER_ENABLE_MONITORING) {
      base.enableMonitoring = !!network.enable_monitoring;
    }
    if (network.enable_hooks !== undefined && !process.env.ASTER_ENABLE_HOOKS) {
      base.enableHooks = !!network.enable_hooks;
    }
    if (network.hook_timeout_ms !== undefined && !process.env.ASTER_HOOK_TIMEOUT_MS) {
      base.hookTimeoutMs = network.hook_timeout_ms as number;
    }
  }

  const logging = toml.logging as Record<string, unknown> | undefined;
  if (logging) {
    if (!process.env.ASTER_LOG_FORMAT && logging.format) {
      base.logFormat = logging.format as 'json' | 'text';
    }
    if (!process.env.ASTER_LOG_LEVEL && logging.level) {
      base.logLevel = logging.level as any;
    }
    if (!process.env.ASTER_LOG_MASK && logging.mask !== undefined) {
      base.logMask = !!logging.mask;
    }
  }

  const health = toml.health as Record<string, unknown> | undefined;
  if (health) {
    if (!process.env.ASTER_HEALTH_PORT && health.port !== undefined) {
      base.healthPort = health.port as number;
    }
    if (!process.env.ASTER_HEALTH_HOST && health.host) {
      base.healthHost = health.host as string;
    }
  }

  return base;
}

/** Parsed identity data from a .aster-identity TOML file. */
export interface IdentityData {
  node: Record<string, unknown>;
  peers: Record<string, unknown>[];
  published_services: Record<string, Record<string, unknown>>;
}

/**
 * Load and parse an .aster-identity TOML file.
 *
 * Synthesizes ``aster.role`` and ``aster.name`` into each peer's
 * ``attributes`` from the top-level ``role`` and ``name`` fields.
 * Merges the top-level ``[published_services.*]`` into each peer's
 * ``published_services`` so callers always see a complete peer dict.
 *
 * Returns the full parsed identity or null if the file doesn't exist.
 */
export function loadIdentityFile(
  filePath?: string,
): IdentityData | null {
  const { existsSync, readFileSync } = require('node:fs');
  const { join } = require('node:path');

  const path = filePath ?? join(process.cwd(), '.aster-identity');
  if (!existsSync(path)) return null;

  try {
    const text = readFileSync(path, 'utf-8');
    const data = parseSimpleToml(text);

    const node = (data.node ?? {}) as Record<string, unknown>;
    const peers = (data.peers ?? []) as Record<string, unknown>[];
    const publishedServices = (data.published_services ?? {}) as Record<string, Record<string, unknown>>;

    // Synthesize aster.role / aster.name into attributes, merge published_services
    for (const peer of peers) {
      const attrs = (peer.attributes ?? {}) as Record<string, unknown>;
      if (!('aster.role' in attrs) && peer.role) {
        attrs['aster.role'] = peer.role;
      }
      if (!('aster.name' in attrs) && peer.name) {
        attrs['aster.name'] = peer.name;
      }
      peer.attributes = attrs;

      // Each peer gets the top-level published_services as default
      if (!peer.published_services) {
        peer.published_services = publishedServices;
      }
    }

    return { node, peers, published_services: publishedServices };
  } catch {
    return null;
  }
}

/**
 * Load identity from a .aster-identity TOML file.
 *
 * Returns secretKey + matching peer, or null if not found.
 * This is the simple convenience wrapper; use loadIdentityFile()
 * for full access to published_services and all peers.
 */
export function loadIdentity(
  filePath?: string,
  peerName?: string,
  role?: string,
): { secretKey: Uint8Array; peer: Record<string, unknown> } | null {
  const identity = loadIdentityFile(filePath);
  if (!identity) return null;

  const node = identity.node;
  if (!node?.secret_key) return null;
  const secretKey = new Uint8Array(Buffer.from(node.secret_key as string, 'base64'));

  // Find matching peer
  let peer: Record<string, unknown> | undefined;
  if (peerName) {
    peer = identity.peers.find(p => p.name === peerName);
  } else if (role) {
    peer = identity.peers.find(p => p.role === role);
  } else {
    peer = identity.peers[0];
  }

  return { secretKey, peer: peer ?? {} };
}

/**
 * Find a peer entry by name or role from parsed identity data.
 */
export function findPeer(
  identity: IdentityData,
  name?: string,
  role?: string,
): Record<string, unknown> | undefined {
  for (const peer of identity.peers) {
    if (name !== undefined && peer.name === name) return peer;
    if (name === undefined && role !== undefined && peer.role === role) return peer;
  }
  return undefined;
}

/**
 * Extract producer tokens from a peer's published_services.
 *
 * Returns a map of service name to token data (entries that
 * have a ``producer_token`` field).
 */
export function getProducerTokens(
  peer: Record<string, unknown>,
): Record<string, Record<string, unknown>> {
  const published = peer.published_services as Record<string, Record<string, unknown>> | undefined;
  if (!published || typeof published !== 'object') return {};

  const tokens: Record<string, Record<string, unknown>> = {};
  for (const [svcName, entry] of Object.entries(published)) {
    if (entry && typeof entry === 'object' && entry.producer_token) {
      tokens[svcName] = entry;
    }
  }
  return tokens;
}

/**
 * Minimal TOML parser for config files.
 * Handles sections (including dotted like [a.b]), array-of-tables,
 * strings, numbers, booleans, inline tables, and simple arrays.
 * Not a full TOML parser.
 */
export function parseSimpleToml(text: string): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  let currentSection: Record<string, unknown> = result;

  for (const line of text.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;

    // Array of tables [[name]]
    const arrayMatch = trimmed.match(/^\[\[([^\]]+)\]\]$/);
    if (arrayMatch) {
      const name = arrayMatch[1]!;
      if (!Array.isArray(result[name])) result[name] = [];
      const entry: Record<string, unknown> = {};
      (result[name] as Record<string, unknown>[]).push(entry);
      currentSection = entry;
      continue;
    }

    // Section header [name] or [name.subkey]
    const sectionMatch = trimmed.match(/^\[([^\]]+)\]$/);
    if (sectionMatch) {
      const name = sectionMatch[1]!;
      const dotIdx = name.indexOf('.');
      if (dotIdx !== -1) {
        // Dotted section: [parent.child] → result.parent.child = {}
        const parent = name.slice(0, dotIdx);
        const child = name.slice(dotIdx + 1);
        if (!result[parent] || typeof result[parent] !== 'object' || Array.isArray(result[parent])) {
          result[parent] = {};
        }
        const parentObj = result[parent] as Record<string, unknown>;
        parentObj[child] = parentObj[child] ?? {};
        currentSection = parentObj[child] as Record<string, unknown>;
      } else {
        result[name] = result[name] ?? {};
        currentSection = result[name] as Record<string, unknown>;
      }
      continue;
    }

    // Key = value
    const kvMatch = trimmed.match(/^(\w+)\s*=\s*(.+)$/);
    if (kvMatch) {
      const [, key, rawValue] = kvMatch;
      currentSection[key!] = parseTomlValue(rawValue!.trim());
    }
  }

  return result;
}

function splitTopLevel(s: string, sep: string): string[] {
  const out: string[] = [];
  let depth = 0;
  let inStr: string | null = null;
  let buf = '';
  for (let i = 0; i < s.length; i++) {
    const ch = s[i]!;
    if (inStr) {
      if (ch === inStr) inStr = null;
      buf += ch;
      continue;
    }
    if (ch === '"' || ch === "'") {
      inStr = ch;
      buf += ch;
      continue;
    }
    if (ch === '{' || ch === '[') depth++;
    else if (ch === '}' || ch === ']') depth--;
    if (ch === sep && depth === 0) {
      out.push(buf);
      buf = '';
      continue;
    }
    buf += ch;
  }
  if (buf.length > 0) out.push(buf);
  return out;
}

function findTopLevelEquals(s: string): number {
  let inStr: string | null = null;
  for (let i = 0; i < s.length; i++) {
    const ch = s[i]!;
    if (inStr) {
      if (ch === inStr) inStr = null;
      continue;
    }
    if (ch === '"' || ch === "'") {
      inStr = ch;
      continue;
    }
    if (ch === '=') return i;
  }
  return -1;
}

function parseTomlValue(raw: string): unknown {
  // Quoted string
  if ((raw.startsWith('"') && raw.endsWith('"')) || (raw.startsWith("'") && raw.endsWith("'"))) {
    return raw.slice(1, -1);
  }
  // Boolean
  if (raw === 'true') return true;
  if (raw === 'false') return false;
  // Number
  const num = Number(raw);
  if (!isNaN(num) && raw !== '') return num;
  // Inline table { key = "val", ... }
  if (raw.startsWith('{') && raw.endsWith('}')) {
    const inner = raw.slice(1, -1).trim();
    if (!inner) return {};
    const obj: Record<string, unknown> = {};
    for (const pair of splitTopLevel(inner, ',')) {
      const eqIdx = findTopLevelEquals(pair);
      if (eqIdx === -1) continue;
      const k = pair.slice(0, eqIdx).trim().replace(/^["']|["']$/g, '');
      const v = pair.slice(eqIdx + 1).trim();
      obj[k] = parseTomlValue(v);
    }
    return obj;
  }
  // Array (simple flat arrays)
  if (raw.startsWith('[') && raw.endsWith(']')) {
    const inner = raw.slice(1, -1).trim();
    if (!inner) return [];
    return inner.split(',').map(v => parseTomlValue(v.trim()));
  }
  return raw;
}

/**
 * Load endpoint config from a file path (alias for configFromFile).
 */
export function loadEndpointConfig(filePath: string): AsterConfig {
  return configFromFile(filePath);
}

/**
 * Resolve the root public key from config (raw bytes or from file).
 * Returns undefined if neither is set.
 */
export function resolveRootPubkey(config: AsterConfig): Uint8Array | undefined {
  if (config.rootPubkey) return config.rootPubkey;
  if (config.rootPubkeyFile) {
    try {
      const { readFileSync } = require('node:fs');
      const raw = readFileSync(config.rootPubkeyFile, 'utf-8').trim();
      // Support both raw hex and JSON {"public_key": "hex"} format
      let hex = raw;
      if (raw.startsWith('{')) {
        try { hex = JSON.parse(raw).public_key; } catch { /* fall through */ }
      }
      return new Uint8Array(Buffer.from(hex, 'hex'));
    } catch { return undefined; }
  }
  return undefined;
}

/**
 * Convert AsterConfig to an endpoint-specific config subset.
 * Returns the config fields relevant to network endpoint setup.
 */
export function toEndpointConfig(config: AsterConfig): Record<string, unknown> {
  return {
    secretKey: config.secretKey,
    bindAddr: config.bindAddr,
    relayMode: config.relayMode,
    storagePath: config.storagePath,
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
