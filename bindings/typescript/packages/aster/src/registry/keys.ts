/**
 * Registry key-schema helpers.
 *
 * All functions return UTF-8 string keys for iroh-docs set/query calls.
 * They match core/src/registry.rs (and bindings/python/aster/registry/keys.py)
 * byte for byte.
 *
 * The keys are **prefix-free**: no key is a byte prefix of another. iroh-docs
 * lets a write at a key (a value or a deletion marker) supersede the same
 * author's older entries whose keys extend it, so with `.../versions/v1` a
 * prefix of `.../versions/v10`, publishing or retracting v1 would remove v10.
 * Every segment ends in `/` and variable segments escape `%` and `/` (`%25`,
 * `%2F`), so a segment never holds a raw `/` and no family's key is an
 * interior path of another.
 */

/** Escape a variable key segment: `%` -> `%25`, `/` -> `%2F`. */
export function keySegment(segment: string): string {
  return segment.replace(/%/g, '%25').replace(/\//g, '%2F');
}

/** Invert {@link keySegment}; `null` for a segment no builder produces. */
export function decodeKeySegment(segment: string): string | null {
  if (segment.includes('/') || segment.replace(/%(25|2F)/g, '').includes('%')) return null;
  return segment.replace(/%(25|2F)/g, (_m, code: string) => (code === '25' ? '%' : '/'));
}

/** Key for a published contract artifact: `contracts/{contractId}/`. */
export function contractKey(contractId: string): string {
  return `contracts/${keySegment(contractId)}/`;
}

/** Key for an inline manifest shortcut: `manifests/{contractId}/`.
 *
 * The ArtifactRef at `contracts/{contractId}/` points at a blob collection;
 * reading `manifest.json` from it needs a round-trip. The server also writes
 * the manifest JSON inline at this key so dynamic consumers can skip the blob
 * download.
 */
export function manifestKey(contractId: string): string {
  return `manifests/${keySegment(contractId)}/`;
}

/** Key for a service version pointer: `services/{name}/versions/v{version}/`. */
export function versionKey(serviceName: string, version: number): string {
  return `services/${keySegment(serviceName)}/versions/v${Math.trunc(version)}/`;
}

/** Prefix selecting every version pointer of one service, and nothing else. */
export function versionPrefix(serviceName: string): string {
  return `services/${keySegment(serviceName)}/versions/`;
}

const VERSION_KEY = /^services\/([^/]*)\/versions\/v(-?(?:0|[1-9][0-9]*))\/$/;

/** `[name, version]` of a {@link versionKey}, or `null` for any other key.
 * Strict: one spelling per pointer (no `+`, no leading zeros, no `-0`). */
export function parseVersionKey(key: string): [string, number] | null {
  const m = VERSION_KEY.exec(key);
  if (!m || m[2] === '-0') return null;
  const name = decodeKeySegment(m[1]);
  const version = Number(m[2]);
  if (name === null || version < -(2 ** 31) || version >= 2 ** 31) return null;
  return [name, version];
}

/** Key for a service channel alias (e.g., "stable", "canary"). */
export function channelKey(serviceName: string, channel: string): string {
  return `services/${keySegment(serviceName)}/channels/${keySegment(channel)}/`;
}

/** Key for a service tag. */
export function tagKey(serviceName: string, tag: string): string {
  return `services/${keySegment(serviceName)}/tags/${keySegment(tag)}/`;
}

/** Key for an endpoint lease. */
export function leaseKey(serviceName: string, contractId: string, endpointId: string): string {
  return `${leasePrefix(serviceName, contractId)}${keySegment(endpointId)}/`;
}

/** Prefix for listing all endpoint leases for a service+contract. */
export function leasePrefix(serviceName: string, contractId: string): string {
  return `services/${keySegment(serviceName)}/contracts/${keySegment(contractId)}/endpoints/`;
}

/** Key for ACL entries. */
export function aclKey(subkey: string): string {
  return `_aster/acl/${keySegment(subkey)}/`;
}

/** Key for internal config. */
export function configKey(subkey: string): string {
  return `_aster/config/${keySegment(subkey)}/`;
}

/** The registry key families a client syncs (a `nothing_except` download
 * policy keeps these). Every family a builder above writes must be covered, or
 * the entry syncs without its value; `endpoints/` and `compatibility/` are
 * spec-reserved families with no builder yet. */
export const REGISTRY_PREFIXES = [
  'contracts/',
  'manifests/',
  'services/',
  'endpoints/',
  'compatibility/',
  '_aster/',
];
