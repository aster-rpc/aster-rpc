/**
 * Registry key-schema helpers.
 *
 * All functions return UTF-8 string keys for iroh-docs set/query calls.
 * Mirrors bindings/python/aster/registry/keys.py.
 */

/** Key for a published contract artifact. */
export function contractKey(contractId: string): string {
  return `contracts/${contractId}`;
}

/** Key for a service version pointer. */
export function versionKey(serviceName: string, version: number): string {
  return `services/${serviceName}/versions/v${version}`;
}

/** Key for a service channel alias (e.g., "stable", "canary"). */
export function channelKey(serviceName: string, channel: string): string {
  return `services/${serviceName}/channels/${channel}`;
}

/** Key for a service tag. */
export function tagKey(serviceName: string, tag: string): string {
  return `services/${serviceName}/tags/${tag}`;
}

/** Key for an endpoint lease. */
export function leaseKey(serviceName: string, contractId: string, endpointId: string): string {
  return `services/${serviceName}/contracts/${contractId}/endpoints/${endpointId}`;
}

/** Prefix for listing all endpoint leases for a service+contract. */
export function leasePrefix(serviceName: string, contractId: string): string {
  return `services/${serviceName}/contracts/${contractId}/endpoints/`;
}

/** Key for ACL entries. */
export function aclKey(subkey: string): string {
  return `_aster/acl/${subkey}`;
}

/** Key for internal config. */
export function configKey(subkey: string): string {
  return `_aster/config/${subkey}`;
}

/** Well-known registry key prefixes. */
export const REGISTRY_PREFIXES = [
  'contracts/',
  'services/',
  'endpoints/',
  'compatibility/',
  '_aster/',
];
