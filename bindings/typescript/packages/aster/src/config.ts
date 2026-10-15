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
  /**
   * File with the root public key (hex or `{"public_key": "<hex>"}`).
   * `ASTER_ROOT_PUBKEY_FILE`, `[trust] root_pubkey_file`. A configured file
   * that does not exist fails an enrollment start.
   */
  rootPubkeyFile?: string;
  /**
   * Pre-rotation commitment to the next root key (64 hex, as
   * `aster trust show` prints it). Without it every rotation statement is
   * refused. `ASTER_NEXT_ROOT_COMMITMENT`, `[trust] next_root_commitment`.
   */
  nextRootCommitment?: string;
  /** Rotations applied to reach the root (default 0). `ASTER_ROOT_SEQ`, `[trust] root_seq`. */
  rootSeq?: number | bigint;
  /**
   * Applied rotation statements, oldest first; needs `nextRootCommitment`.
   * `ASTER_ROOT_ROTATIONS` (comma-separated), `[trust] root_rotations`.
   */
  rootRotations?: string[];
  /**
   * This node's enrollment credential: the chain file `aster enroll node
   * --out` writes. Setting it enables enrollment.
   * `ASTER_ENROLLMENT_CREDENTIAL`, `[trust] enrollment_credential`.
   */
  enrollmentCredentialFile?: string;
  /**
   * This node's enrollment credential chain inline; wins over the file. A
   * value that is not a chain (a path, say) is refused.
   * `ASTER_ENROLLMENT_CREDENTIAL_CHAIN`, `[trust] enrollment_credential_chain`.
   */
  enrollmentCredentialChain?: string;
  /**
   * Enrollment (the `[enrollment]` section): which peers `AsterServer` admits
   * and how their credential claims become the roles `requires` checks. See
   * {@link EnrollmentConfig}.
   */
  enrollment?: EnrollmentConfig;

  // ── Connect ────────────────────────────────────────────────────────────
  /** Producer address for consumer-side dialing. `ASTER_ENDPOINT_ADDR`. */
  endpointAddr?: string;

  // ── Storage ────────────────────────────────────────────────────────────
  /** Persistent node directory; in-memory when unset. `ASTER_STORAGE_PATH`. */
  storagePath?: string;
  /** Blob GC interval (ms); `0` / unset = no GC. `ASTER_GC_INTERVAL_MS`. */
  gcIntervalMs?: number | bigint;
  /**
   * zstd level for blob transport: unset = the default (3), `0` = off (the
   * node serves and fetches blobs on the plain ALPN only), `1`–`19` = that
   * level (larger values are clamped to 19). `ASTER_BLOB_ZSTD_LEVEL` (also
   * `off`), `[storage] blob_zstd_level`.
   */
  blobZstdLevel?: number;

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
  /**
   * Re-bind a persistent node's last UDP ports on restart (default `true`);
   * `false` binds fresh ports and records none in `bind-ports.json`.
   * `ASTER_REUSE_LAST_PORT`, `[network] reuse_last_port`.
   */
  reuseLastPort?: boolean;
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

  // ── Health (Rust listener, `/healthz` `/readyz` `/metrics`) ────────────
  /** Health HTTP port; `0` disables unless `healthEnabled`. `ASTER_HEALTH_PORT`, `[health] port`. */
  healthPort?: number;
  /** Health HTTP host (default `127.0.0.1`; pods need `0.0.0.0`). `ASTER_HEALTH_HOST`, `[health] host`. */
  healthHost?: string;
  /**
   * Force the listener on (an ephemeral port when `healthPort` is 0) or off.
   * Unset: on exactly when `healthPort` is non-zero.
   * `ASTER_HEALTH_ENABLED`, `[health] enabled`.
   */
  healthEnabled?: boolean;
}


/**
 * The admission policy, as the externally tagged `PolicyConfig` tree (the
 * same JSON as `ASTER_ENROLLMENT_POLICY` and the TOML `[enrollment] policy`).
 * Unknown nodes are refused when the node starts.
 *
 * @example
 * ```ts
 * const policy: PolicyConfig = { all_of: [{ scope: 'ops' }, 'tolerate_stale_policy'] };
 * ```
 * @group Enrollment
 */
export type PolicyConfig =
  | 'tolerate_stale_policy'
  | 'require_registration'
  | { all_of: PolicyConfig[] }
  | { any_of: PolicyConfig[] }
  | { none_of: PolicyConfig[] }
  | { source_cidr: string }
  | { audience: string }
  | { scope: string }
  | { tag: string }
  | {
      attribute_equals: {
        key: string;
        value: { type: 'bool' | 'int' | 'float' | 'string'; value: boolean | number | string };
      };
    }
  | { issued_by: string }
  | { max_credential_age_secs: number }
  | { policy_synchronized_within: { max_age_secs: number; require_root?: boolean } };

/**
 * `[enrollment.limits]`: overrides of the Enrollment defaults
 * (`ASTER_ENROLLMENT_<FIELD>` upper-cased, e.g.
 * `ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS`).
 * @group Enrollment
 */
export interface EnrollmentLimits {
  handshakeTimeoutMs?: number | bigint;
  stepTimeoutMs?: number | bigint;
  policyTimeoutMs?: number | bigint;
  reconciliationIntervalMs?: number | bigint;
  maxAdmittedPeers?: number | bigint;
  /** Bound on enrolling with and synchronizing from a replica (default 15 000 ms). */
  syncTimeoutMs?: number | bigint;
}

/**
 * `[enrollment.roles]`: how a peer's verified claims become the attributes
 * `requires` is checked against. A peer's roles are the comma-separated
 * values of the claim attribute `attribute`, plus every verified scope when
 * `scopesAsRoles`, plus the roles `scopes` / `tags` map its scopes and tags
 * to. Evaluated on every call.
 * @group Enrollment
 */
export interface EnrollmentRoles {
  /** Claim attribute listing roles (default `"aster.role"`; `""` disables it). `ASTER_ENROLLMENT_ROLE_ATTRIBUTE`. */
  attribute?: string;
  /** Each verified scope is also a role (default false). `ASTER_ENROLLMENT_SCOPES_AS_ROLES`. */
  scopesAsRoles?: boolean;
  /** Scope → roles. `[enrollment.roles.scopes]`. */
  scopes?: Record<string, string[]>;
  /** Tag → roles. `[enrollment.roles.tags]`. */
  tags?: Record<string, string[]>;
  /** Pass the other claim attributes to handlers (default true). `ASTER_ENROLLMENT_INCLUDE_ATTRIBUTES`. */
  includeAttributes?: boolean;
}

/**
 * The `[enrollment]` section, field for field the Rust `EnrollmentConfig`.
 *
 * Enrollment is on when `enabled` is `true`, or, with `enabled` unset, when
 * an `audience`, `fleetNamespace`, `issuerDelegationFile` or a credential
 * (`enrollmentCredentialFile` / `enrollmentCredentialChain`) is set. Once on,
 * every value is validated when the server or client starts and any missing
 * or malformed value fails the start with an error naming it: enrollment
 * never degrades to an open node.
 * @group Enrollment
 */
export interface EnrollmentConfig {
  /** Force enrollment on or off. `ASTER_ENROLLMENT_ENABLED`. */
  enabled?: boolean;
  /** Fleet audience. Required. `ASTER_ENROLLMENT_AUDIENCE`. */
  audience?: string;
  /** Fleet policy namespace (64 hex, from `aster enroll namespace create`). Required. `ASTER_ENROLLMENT_FLEET_NAMESPACE`. */
  fleetNamespace?: string;
  /**
   * Durable denials and watermarks. Default `<storagePath>/enrollment.json`;
   * a server with neither refuses to start, a client uses a temporary
   * directory. `ASTER_ENROLLMENT_STATE_PATH`.
   */
  statePath?: string;
  /** Policy replicas (`aster1…` tickets or 64-hex node ids). `ASTER_ENROLLMENT_REPLICAS`. */
  replicas?: string[];
  /** Admission policy (default `"tolerate_stale_policy"`). `ASTER_ENROLLMENT_POLICY` (JSON). */
  policy?: PolicyConfig;
  /** Refuse to start on an unsynchronized clock (Linux). `ASTER_ENROLLMENT_REQUIRE_SYNCHRONIZED_CLOCK`. */
  requireSynchronizedClock?: boolean;
  /** `iat`/`nbf` leeway, 0–300 s (default 60). `ASTER_ENROLLMENT_ACTIVATION_LEEWAY_SECS`. */
  activationLeewaySecs?: number | bigint;
  /** `[enrollment.limits]`. */
  limits?: EnrollmentLimits;
  /** `[enrollment.roles]`. */
  roles?: EnrollmentRoles;
  /**
   * Server only: run a delegated issuer holding this delegation chain
   * (`aster enroll delegate --out`) and serve `aster.enrollment.IssuerV1`.
   * `[enrollment.issuer] delegation_file`, `ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE`.
   */
  issuerDelegationFile?: string;
  /** Lifetime of issued credentials in seconds (default 24 h). `[enrollment.issuer] lifetime`, `ASTER_ENROLLMENT_ISSUER_LIFETIME`. */
  issuerLifetimeSecs?: number | bigint;
  /** Issuer to renew this node's credential from (ticket or node id). `ASTER_ENROLLMENT_RENEW_FROM`. */
  renewFrom?: string;
  /** `false` disables renewal. `ASTER_ENROLLMENT_RENEW`. */
  renew?: boolean;
  /** Client: synchronize the fleet namespace the server offers (default true). `ASTER_ENROLLMENT_SYNCHRONIZE`. */
  synchronize?: boolean;
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
 * Overlay `override` on `base` the way `AsterServer` / `AsterClient` resolve
 * their configuration (`{ ...AsterConfig.fromEnv(), ...opts.config }`), but
 * merging the `enrollment` section (and its `limits` / `roles`) field by
 * field, so setting one enrollment value in code keeps the rest from the
 * environment or file.
 */
export function mergeConfig(base: AsterConfig, override: Partial<AsterConfig> = {}): AsterConfig {
  const out: AsterConfig = { ...base, ...override };
  if (base.enrollment && override.enrollment) {
    const b = base.enrollment;
    const o = override.enrollment;
    out.enrollment = {
      ...b,
      ...o,
      ...(b.limits || o.limits ? { limits: { ...b.limits, ...o.limits } } : {}),
      ...(b.roles || o.roles ? { roles: { ...b.roles, ...o.roles } } : {}),
    };
  }
  return out;
}

/**
 * Whether `config` enables enrollment (Rust
 * `AsterConfig::is_enrollment_enabled`): `enrollment.enabled` when set,
 * otherwise when an audience, fleet namespace, issuer delegation or
 * credential is configured. The root key alone does not enable it.
 * @group Enrollment
 */
export function isEnrollmentEnabled(config: AsterConfig): boolean {
  return loadNative().asterConfigIsEnrollmentEnabled(toNativeConfig(config));
}

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
  console.log(`    enrollment_credential   : ${config.enrollmentCredentialFile ?? (config.enrollmentCredentialChain ? '<inline chain>' : '<not set>')}`);
  const e = config.enrollment ?? {};
  console.log(`  [enrollment]`);
  console.log(`    enabled                 : ${isEnrollmentEnabled(config)}`);
  console.log(`    audience                : ${e.audience ?? '<not set>'}`);
  console.log(`    fleet_namespace         : ${e.fleetNamespace ? `${e.fleetNamespace.slice(0, 8)}...` : '<not set>'}`);
  console.log(`    state_path              : ${e.statePath ?? '<default>'}`);
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
  console.log(`    port                    : ${config.healthPort || (config.healthEnabled ? 'ephemeral' : 'disabled')}`);
  console.log(`    host                    : ${config.healthHost ?? '127.0.0.1'}`);
}
