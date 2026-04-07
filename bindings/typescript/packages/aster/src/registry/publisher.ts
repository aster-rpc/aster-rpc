/**
 * Registry publisher — publishes contracts and manages endpoint leases.
 *
 * Mirrors bindings/python/aster/registry/publisher.py.
 */

import type { ArtifactRef, EndpointLease, HealthStatus } from './models.js';
import { HealthStatus as HS } from './models.js';
import { contractKey, versionKey, leaseKey } from './keys.js';

/** Doc handle interface (matches NAPI DocHandle). */
interface PublisherDoc {
  setBytes(authorHex: string, key: string, value: Uint8Array): Promise<string>;
}

/** Gossip broadcaster interface. */
interface GossipBroadcaster {
  broadcast(data: Uint8Array): Promise<void>;
}

/** Options for the registry publisher. */
export interface RegistryPublisherOptions {
  doc: PublisherDoc;
  authorId: string;
  endpointId: string;
  gossip?: GossipBroadcaster;
  logger?: { info(...args: any[]): void; error(...args: any[]): void };
}

/**
 * Registry publisher — publishes contracts and manages endpoint leases.
 */
export class RegistryPublisher {
  private doc: PublisherDoc;
  private authorId: string;
  private endpointId: string;
  private gossip?: GossipBroadcaster;
  private logger: { info(...args: any[]): void; error(...args: any[]): void };
  private leases = new Map<string, EndpointLease>();
  private refreshTimer?: ReturnType<typeof setInterval>;

  constructor(opts: RegistryPublisherOptions) {
    this.doc = opts.doc;
    this.authorId = opts.authorId;
    this.endpointId = opts.endpointId;
    this.gossip = opts.gossip;
    this.logger = opts.logger ?? console;
  }

  /**
   * Publish a contract artifact to the registry.
   */
  async publishContract(artifact: ArtifactRef, serviceName: string, version: number): Promise<void> {
    const encoder = new TextEncoder();

    // Write artifact to contracts/{contract_id}
    const artifactJson = JSON.stringify(artifact);
    await this.doc.setBytes(this.authorId, contractKey(artifact.contractId), encoder.encode(artifactJson));

    // Write version pointer
    await this.doc.setBytes(this.authorId, versionKey(serviceName, version), encoder.encode(artifact.contractId));

    this.logger.info('published contract', {
      service: serviceName,
      version,
      contractId: artifact.contractId.slice(0, 16),
    });

    // Broadcast gossip event
    if (this.gossip) {
      const event = {
        type: 0, // CONTRACT_PUBLISHED
        service: serviceName,
        version,
        contractId: artifact.contractId,
        timestampMs: Date.now(),
      };
      await this.gossip.broadcast(encoder.encode(JSON.stringify(event)));
    }
  }

  /**
   * Register an endpoint lease for a service.
   */
  async registerEndpoint(
    serviceName: string,
    version: number,
    contractId: string,
    opts?: {
      alpn?: string;
      serializationModes?: string[];
      leaseDurationMs?: number;
      refreshIntervalMs?: number;
    },
  ): Promise<EndpointLease> {
    const now = Date.now();
    const leaseDurationMs = opts?.leaseDurationMs ?? 45_000;

    const lease: EndpointLease = {
      endpointId: this.endpointId,
      contractId,
      service: serviceName,
      version,
      leaseExpiresEpochMs: now + leaseDurationMs,
      leaseSeq: 0,
      alpn: opts?.alpn ?? 'aster/1',
      serializationModes: opts?.serializationModes ?? ['xlang'],
      featureFlags: [],
      directAddrs: [],
      healthStatus: HS.STARTING,
      tags: [],
      updatedAtEpochMs: now,
    };

    await this.writeLease(lease);
    this.leases.set(`${serviceName}/${contractId}`, lease);

    // Start refresh timer
    const refreshMs = opts?.refreshIntervalMs ?? leaseDurationMs * 0.8;
    this.refreshTimer = setInterval(() => {
      this.refreshLeases().catch(e => {
        this.logger.error('lease refresh error', { error: String(e) });
      });
    }, refreshMs);

    return lease;
  }

  /**
   * Set health status for a registered endpoint.
   */
  async setHealth(serviceName: string, contractId: string, status: HealthStatus): Promise<void> {
    const key = `${serviceName}/${contractId}`;
    const lease = this.leases.get(key);
    if (!lease) throw new Error(`no lease for ${key}`);

    lease.healthStatus = status;
    lease.leaseSeq++;
    lease.updatedAtEpochMs = Date.now();
    lease.leaseExpiresEpochMs = Date.now() + 45_000;

    await this.writeLease(lease);
  }

  /**
   * Withdraw an endpoint (graceful shutdown).
   */
  async withdraw(serviceName: string, contractId: string, graceMs = 5000): Promise<void> {
    const key = `${serviceName}/${contractId}`;
    const lease = this.leases.get(key);
    if (!lease) return;

    // Set draining
    await this.setHealth(serviceName, contractId, HS.DRAINING);

    // Grace period
    await new Promise(r => setTimeout(r, graceMs));

    // Write tombstone
    const encoder = new TextEncoder();
    const docKey = leaseKey(serviceName, contractId, this.endpointId);
    await this.doc.setBytes(this.authorId, docKey, encoder.encode('null'));

    // Broadcast endpoint down
    if (this.gossip) {
      const event = {
        type: 3, // ENDPOINT_DOWN
        service: serviceName,
        endpointId: this.endpointId,
        timestampMs: Date.now(),
      };
      await this.gossip.broadcast(encoder.encode(JSON.stringify(event)));
    }

    this.leases.delete(key);
    this.logger.info('withdrew endpoint', { service: serviceName });
  }

  /** Stop all refresh timers. */
  close(): void {
    if (this.refreshTimer) {
      clearInterval(this.refreshTimer);
      this.refreshTimer = undefined;
    }
  }

  // -- Helpers --

  private async writeLease(lease: EndpointLease): Promise<void> {
    const encoder = new TextEncoder();
    const key = leaseKey(lease.service, lease.contractId, lease.endpointId);
    await this.doc.setBytes(this.authorId, key, encoder.encode(JSON.stringify(lease)));

    // Broadcast gossip event
    if (this.gossip) {
      const event = {
        type: 2, // ENDPOINT_LEASE_UPSERTED
        service: lease.service,
        version: lease.leaseSeq,
        contractId: lease.contractId,
        endpointId: lease.endpointId,
        timestampMs: Date.now(),
      };
      await this.gossip.broadcast(encoder.encode(JSON.stringify(event)));
    }
  }

  private async refreshLeases(): Promise<void> {
    for (const lease of this.leases.values()) {
      if (lease.healthStatus === HS.DRAINING) continue;
      lease.leaseSeq++;
      lease.leaseExpiresEpochMs = Date.now() + 45_000;
      lease.updatedAtEpochMs = Date.now();
      await this.writeLease(lease);
    }
  }
}
