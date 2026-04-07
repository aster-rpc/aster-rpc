/**
 * Registry data models.
 *
 * Mirrors bindings/python/aster/registry/models.py.
 */

/** Endpoint health status. */
export const HealthStatus = {
  STARTING: 'starting',
  READY: 'ready',
  DEGRADED: 'degraded',
  DRAINING: 'draining',
} as const;

export type HealthStatus = (typeof HealthStatus)[keyof typeof HealthStatus];

/** Gossip event types. */
export const GossipEventType = {
  CONTRACT_PUBLISHED: 0,
  CHANNEL_UPDATED: 1,
  ENDPOINT_LEASE_UPSERTED: 2,
  ENDPOINT_DOWN: 3,
  ACL_CHANGED: 4,
  COMPATIBILITY_PUBLISHED: 5,
} as const;

export type GossipEventType = (typeof GossipEventType)[keyof typeof GossipEventType];

/** Service summary returned in admission and registry. */
export interface ServiceSummary {
  name: string;
  version: number;
  contractId: string;
  channels: Record<string, string>;
}

/** Artifact reference stored in the registry doc. */
export interface ArtifactRef {
  contractId: string;
  collectionHash: string;
  providerEndpointId?: string;
  relayUrl?: string;
  ticket?: string;
  publishedBy: string;
  publishedAtEpochMs: number;
  collectionFormat: 'raw' | 'index';
}

/** Endpoint lease for a service provider. */
export interface EndpointLease {
  endpointId: string;
  contractId: string;
  service: string;
  version: number;
  leaseExpiresEpochMs: number;
  leaseSeq: number;
  alpn: string;
  serializationModes: string[];
  featureFlags: string[];
  relayUrl?: string;
  directAddrs: string[];
  load?: number;
  languageRuntime?: string;
  asterVersion?: string;
  policyRealm?: string;
  healthStatus: HealthStatus;
  tags: string[];
  updatedAtEpochMs: number;
}

/** Check if a lease is fresh (not expired). */
export function isLeaseFresh(lease: EndpointLease, leaseDurationS = 45): boolean {
  return lease.leaseExpiresEpochMs > Date.now() ||
    lease.updatedAtEpochMs + leaseDurationS * 1000 > Date.now();
}

/** Check if a lease is routable (ready + fresh). */
export function isLeaseRoutable(lease: EndpointLease): boolean {
  return lease.healthStatus === HealthStatus.READY && isLeaseFresh(lease);
}

/** Gossip event. */
export interface GossipEvent {
  type: GossipEventType;
  service?: string;
  version?: number;
  channel?: string;
  contractId?: string;
  endpointId?: string;
  keyPrefix?: string;
  timestampMs: number;
}
