/**
 * AsterConfig — node / server / client configuration.
 *
 * A typed options object lowered onto the Rust `aster::AsterConfig`. Loading
 * is done by the Rust loaders (plan D1), so env / TOML semantics are the same
 * in every binding (`docs/aster-rust-getstarted.md` §2.2):
 *
 *   defaults < `aster.toml` ({@link AsterConfig.fromFile}) <
 *   `ASTER_*` env ({@link AsterConfig.fromEnv}) < fields set in code
 *
 * @example
 * ```ts
 * await using node = await Node.start({
 *   ...AsterConfig.fromEnv(),
 *   storagePath: '/var/lib/my-app',
 *   secretKeyFile: '/var/lib/my-app/node.key',
 * });
 * ```
 */

import type {
  AsterConfig as NativeAsterConfig,
  AsterIdentity as NativeAsterIdentity,
} from '@aster-rpc/transport';
import { loadNative } from './native.js';

/** Relay selection. A bare relay URL is also accepted (means `custom`). */
export type RelayModeName = 'default' | 'staging' | 'disabled' | 'custom';
/** What a Gate 0 hook does when the host does not answer in time. */
export type HookFailureMode = 'fail_open' | 'fail_closed';
/** Log output format. */
export type LogFormat = 'text' | 'json';
/** Log level. */
export type LogLevel = 'debug' | 'info' | 'warning' | 'error';
/** A 32-byte key: 64 hex characters or the raw bytes. */
export type KeyMaterial = string | Uint8Array;

/**
 * Aster configuration. Every field is optional; omitted fields take the Rust
 * defaults. The loaders return every field filled in (keys as hex strings).
 *
 * The `u64` knobs accept `number` or `bigint`; loaders return a `number`
 * unless the value exceeds `Number.MAX_SAFE_INTEGER`.
 *
 * @group Server and Client
 */
export interface AsterConfig {
  // ── Trust ──────────────────────────────────────────────────────────────
  /** Deployment root public key. `ASTER_ROOT_PUBKEY`, `[trust] root_pubkey`. */
  rootPubkey?: KeyMaterial;
  /** File with the root public key (hex or `{"public_key": "<hex>"}`). */
  rootPubkeyFile?: string;
  /** Enrollment credential file. `ASTER_ENROLLMENT_CREDENTIAL`. */
  enrollmentCredentialFile?: string;
  /** Cloud instance identity token. `ASTER_ENROLLMENT_CREDENTIAL_IID`. */
  enrollmentCredentialIid?: string;
  /** Skip the consumer admission gate (dev mode). `ASTER_ALLOW_ALL_CONSUMERS`. */
  allowAllConsumers?: boolean;
  /**
   * Kept for schema parity with Rust. Producer mesh admission is not
   * supported by the TypeScript binding: `AsterServer` throws on `false`.
   */
  allowAllProducers?: boolean;

  // ── Connect ────────────────────────────────────────────────────────────
  /** Producer address for consumer-side dialing. `ASTER_ENDPOINT_ADDR`. */
  endpointAddr?: string;

  // ── Storage ────────────────────────────────────────────────────────────
  /** Persistent node directory; in-memory when unset. `ASTER_STORAGE_PATH`. */
  storagePath?: string;
  /** Blob GC interval (ms); `0` / unset = no GC. `ASTER_GC_INTERVAL_MS`. */
  gcIntervalMs?: number | bigint;

  // ── Identity ───────────────────────────────────────────────────────────
  /** Node secret key. `ASTER_SECRET_KEY`, `[network] secret_key`. */
  secretKey?: KeyMaterial;
  /**
   * Node **secret key file** (64 hex characters) — the one on-disk identity
   * format shared with Rust and every binding. Loaded at start if it exists
   * (an explicit `secretKey` that disagrees is an error), otherwise created
   * (mode 0600) from `secretKey` or a fresh key. `ASTER_SECRET_KEY_FILE`.
   */
  secretKeyFile?: string;
  /** `.aster-identity` credential file. `ASTER_IDENTITY_FILE`, `[identity] file`. */
  identityFile?: string;

  // ── Network ────────────────────────────────────────────────────────────
  /** `ASTER_RELAY_MODE`, `[network] relay_mode`. */
  relayMode?: RelayModeName | (string & {});
  /** Custom relay URLs (implies `custom`). `ASTER_RELAY_URLS`. */
  relayUrls?: string[];
  /** Local bind address, e.g. `"0.0.0.0:9000"`. `ASTER_BIND_ADDR`. */
  bindAddr?: string;
  /** mDNS local discovery. `ASTER_LOCAL_DISCOVERY`. */
  localDiscovery?: boolean;
  /** Connection monitoring. `ASTER_ENABLE_MONITORING`. */
  enableMonitoring?: boolean;
  /** Admission hooks (Gate 0). `ASTER_ENABLE_HOOKS`. */
  enableHooks?: boolean;
  /** Hook decision timeout (ms, default 5000). `ASTER_HOOK_TIMEOUT_MS`. */
  hookTimeoutMs?: number | bigint;
  /** `ASTER_HOOK_FAILURE_MODE`. */
  hookFailureMode?: HookFailureMode;
  /** Relay-only. `ASTER_CLEAR_IP_TRANSPORTS`. */
  clearIpTransports?: boolean;
  /** Direct-only. `ASTER_CLEAR_RELAY_TRANSPORTS`. */
  clearRelayTransports?: boolean;
  /** UPnP / NAT-PMP port mapping; unset = enabled. `ASTER_PORTMAPPER_CONFIG`. */
  portmapper?: boolean;
  /** HTTP/SOCKS proxy URL. `ASTER_PROXY_URL`. */
  proxyUrl?: string;
  /** Use `HTTP(S)_PROXY`. `ASTER_PROXY_FROM_ENV`. */
  proxyFromEnv?: boolean;

  // ── QUIC transport (`ASTER_TRANSPORT_*`, `[network] transport_*`) ────────
  transportMaxConcurrentBidiStreams?: number | bigint;
  transportMaxConcurrentUniStreams?: number | bigint;
  /** Per-stream receive window (bytes). */
  transportStreamReceiveWindow?: number | bigint;
  /** Per-connection receive window (bytes). */
  transportReceiveWindow?: number | bigint;
  /** Send window (bytes). */
  transportSendWindow?: number | bigint;
  transportMaxIdleTimeoutMs?: number | bigint;
  transportKeepAliveIntervalMs?: number | bigint;
  /** Initial UDP payload size (u16). */
  transportInitialMtu?: number;
  /** Incoming datagram buffer (bytes); `0` disables incoming datagrams. */
  transportDatagramReceiveBufferSize?: number | bigint;
  transportDatagramSendBufferSize?: number | bigint;
  transportSendFairness?: boolean;
  transportEnableSegmentationOffload?: boolean;
  /** Extra inbound ALPNs beyond blobs / docs / gossip. Not loaded from env. */
  alpns?: Array<string | Uint8Array>;

  // ── Logging ────────────────────────────────────────────────────────────
  /** `ASTER_LOG_FORMAT`, `[logging] format`. */
  logFormat?: LogFormat;
  /** `ASTER_LOG_LEVEL`, `[logging] level`. */
  logLevel?: LogLevel;
  /** Mask sensitive fields. `ASTER_LOG_MASK`. */
  logMask?: boolean;

  // ── Health (TypeScript binding only) ───────────────────────────────────
  /** Health HTTP port; `0` disables. `ASTER_HEALTH_PORT`, `[health] port`. */
  healthPort?: number;
  /** Health HTTP host. `ASTER_HEALTH_HOST`, `[health] host`. */
  healthHost?: string;
}

function keyHex(value: KeyMaterial | undefined): string | undefined {
  if (value === undefined || value === null) return undefined;
  return typeof value === 'string' ? value : Buffer.from(value).toString('hex');
}

/** Lower an {@link AsterConfig} to the native object shape. @internal */
export function toNativeConfig(config: AsterConfig = {}): NativeAsterConfig {
  const { rootPubkey, secretKey, alpns, ...rest } = config;
  const out: NativeAsterConfig = {};
  for (const [k, v] of Object.entries(rest)) {
    if (v !== undefined && v !== null) (out as Record<string, unknown>)[k] = v;
  }
  const root = keyHex(rootPubkey);
  if (root !== undefined) out.rootPubkey = root;
  const secret = keyHex(secretKey);
  if (secret !== undefined) out.secretKey = secret;
  if (alpns) {
    out.alpns = alpns.map((a) => (typeof a === 'string' ? Buffer.from(a) : Buffer.from(a)));
  }
  return out;
}

function fromNative(native: NativeAsterConfig): AsterConfig {
  const out: AsterConfig = {};
  for (const [k, v] of Object.entries(native)) {
    if (v !== undefined && v !== null) (out as Record<string, unknown>)[k] = v;
  }
  return out;
}

/**
 * Loaders for {@link AsterConfig}, backed by the Rust `aster::AsterConfig`.
 * @group Server and Client
 */
export const AsterConfig = {
  /** Defaults overlaid with `ASTER_*` environment variables. */
  fromEnv(): AsterConfig {
    return fromNative(loadNative().asterConfigFromEnv());
  },
  /** Defaults < the `aster.toml` at `path` < `ASTER_*` environment. */
  fromFile(path: string): AsterConfig {
    return fromNative(loadNative().asterConfigFromFile(path));
  },
  /**
   * Validate `config` in Rust and return it with every default filled in.
   * Reads neither env nor files.
   */
  resolve(config: AsterConfig = {}): AsterConfig {
    return fromNative(loadNative().asterConfigResolve(toNativeConfig(config)));
  },
};

/**
 * The deployment root public key (hex) from `rootPubkey` / `rootPubkeyFile`
 * (Rust `AsterConfig::resolve_root_pubkey`). Both are read and must agree; a
 * missing file falls back to `rootPubkey`; a malformed file throws.
 */
export function resolveRootPubkey(config: AsterConfig): string | undefined {
  return loadNative().asterConfigResolveRootPubkey(toNativeConfig(config)) ?? undefined;
}

// ── .aster-identity credential files (frozen auth, plan D3) ─────────────────

/** A `[[peers]]` entry selected from `.aster-identity`. */
export interface IdentityPeer {
  name?: string;
  role?: string;
  /** Root public key (hex). */
  rootPubkey?: string;
  /** `type` in TOML, e.g. `"policy"`. */
  credentialType?: string;
  expiresAt?: number;
  endpointId?: string;
  signature?: string;
  /** Attributes; `aster.role` / `aster.name` synthesized when absent. */
  attributes: Record<string, string>;
  /** Published-service metadata (service name → fields). */
  publishedServices: Record<string, Record<string, unknown>>;
}

/** `[node] secret_key` and the selected peer from `.aster-identity`. */
export interface AsterIdentity {
  /** The file that was loaded. */
  path: string;
  /** `[node] secret_key` as 64 hex characters, if present. */
  secretKey?: string;
  /** The selected peer, if any. */
  peer?: IdentityPeer;
}

/**
 * Load a `.aster-identity` credential file (Rust `AsterConfig::load_identity`).
 *
 * `path` defaults to `./.aster-identity`. Peer selection: `peerName` wins,
 * else the first peer with `role`, else the first peer. Returns `null` when
 * the file does not exist; a malformed file throws.
 */
export function loadIdentity(
  path?: string,
  peerName?: string,
  role?: string,
): AsterIdentity | null {
  const raw: NativeAsterIdentity | null = loadNative().loadAsterIdentity(path, peerName, role);
  if (!raw) return null;
  const out: AsterIdentity = { path: raw.path };
  if (raw.secretKey) out.secretKey = raw.secretKey;
  if (raw.peer) {
    const p = raw.peer;
    const peer: IdentityPeer = {
      attributes: { ...p.attributes },
      publishedServices: (p.publishedServices ?? {}) as Record<string, Record<string, unknown>>,
    };
    if (p.name != null) peer.name = p.name;
    if (p.role != null) peer.role = p.role;
    if (p.rootPubkey != null) peer.rootPubkey = p.rootPubkey;
    if (p.credentialType != null) peer.credentialType = p.credentialType;
    if (p.expiresAt != null) peer.expiresAt = p.expiresAt;
    if (p.endpointId != null) peer.endpointId = p.endpointId;
    if (p.signature != null) peer.signature = p.signature;
    out.peer = peer;
  }
  return out;
}

/**
 * Parse TOML text with the Rust `toml` crate into plain values. Used by the
 * frozen credential readers, which need the raw `[[peers]]` entries.
 * @internal
 */
export function parseToml(text: string): Record<string, unknown> {
  return loadNative().parseToml(text) as Record<string, unknown>;
}

/**
 * Producer tokens in a peer's published services: service name → entry, for
 * entries that carry a `producer_token`.
 */
export function getProducerTokens(
  peer: Pick<IdentityPeer, 'publishedServices'>,
): Record<string, Record<string, unknown>> {
  const tokens: Record<string, Record<string, unknown>> = {};
  for (const [svcName, entry] of Object.entries(peer.publishedServices ?? {})) {
    if (entry && typeof entry === 'object' && entry.producer_token) {
      tokens[svcName] = entry;
    }
  }
  return tokens;
}

/** Print a resolved config (sensitive values masked). */
export function printConfig(config: AsterConfig): void {
  const mask = (v: unknown) => (v ? '****' : '<not set>');
  const root = keyHex(config.rootPubkey);
  console.log(`  [trust]`);
  console.log(`    root_pubkey             : ${root ? `${root.slice(0, 8)}...` : '<not set>'}`);
  console.log(`    allow_all_consumers     : ${config.allowAllConsumers ?? false}`);
  console.log(`    allow_all_producers     : ${config.allowAllProducers ?? true}`);
  console.log(`  [connect]`);
  console.log(`    endpoint_addr           : ${config.endpointAddr ?? '<not set>'}`);
  console.log(`  [network]`);
  console.log(`    secret_key              : ${mask(config.secretKey)}`);
  console.log(`    secret_key_file         : ${config.secretKeyFile ?? '<not set>'}`);
  console.log(`    relay_mode              : ${config.relayMode ?? '<default>'}`);
  console.log(`    bind_addr               : ${config.bindAddr ?? '<any>'}`);
  console.log(`  [storage]`);
  console.log(`    path                    : ${config.storagePath ?? '<in-memory>'}`);
  console.log(`  [logging]`);
  console.log(`    format                  : ${config.logFormat ?? 'text'}`);
  console.log(`    level                   : ${config.logLevel ?? 'info'}`);
  console.log(`  [health]`);
  console.log(`    port                    : ${config.healthPort || 'disabled'}`);
  console.log(`    host                    : ${config.healthHost ?? '127.0.0.1'}`);
}
