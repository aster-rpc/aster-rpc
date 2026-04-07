/**
 * Registry client — service discovery via Iroh docs + blobs.
 *
 * Reads contract manifests from the registry document (joined via
 * the registry ticket from admission), discovers available services,
 * and downloads contract collections from the blob store.
 */

import type { ContractManifest } from '../contract/manifest.js';
import { manifestFromJson } from '../contract/manifest.js';

/** Registry key encoding: "contracts/{service_name}/v{version}" */
export function registryKey(serviceName: string, version: number): string {
  return `contracts/${serviceName}/v${version}`;
}

/** Artifact reference stored in the registry doc. */
export interface RegistryArtifactRef {
  contractId: string;
  collectionHash: string;
  collectionTicket: string;
}

/** Endpoint lease for a service provider. */
export interface EndpointLease {
  endpointId: string;
  serviceName: string;
  serviceVersion: number;
  contractId: string;
  lastSeen: number; // epoch ms
  status: 'ready' | 'degraded' | 'draining';
}

/**
 * Registry client — discovers services from the registry document.
 *
 * Usage:
 * 1. Join the registry doc using the ticket from admission
 * 2. Read contract entries
 * 3. Download manifests from blob collections
 */
export class RegistryClient {
  private manifests = new Map<string, ContractManifest>();
  private leases = new Map<string, EndpointLease[]>();

  /** Register a discovered manifest. */
  addManifest(manifest: ContractManifest): void {
    const key = `${manifest.service}/v${manifest.version}`;
    this.manifests.set(key, manifest);
  }

  /** Get a manifest by service name and version. */
  getManifest(serviceName: string, version?: number): ContractManifest | undefined {
    if (version !== undefined) {
      return this.manifests.get(`${serviceName}/v${version}`);
    }
    // Find any version
    for (const [key, manifest] of this.manifests) {
      if (key.startsWith(`${serviceName}/`)) return manifest;
    }
    return undefined;
  }

  /** All known manifests. */
  allManifests(): ContractManifest[] {
    return [...this.manifests.values()];
  }

  /** Register an endpoint lease. */
  addLease(lease: EndpointLease): void {
    const key = `${lease.serviceName}/v${lease.serviceVersion}`;
    const leases = this.leases.get(key) ?? [];
    // Update or add
    const idx = leases.findIndex(l => l.endpointId === lease.endpointId);
    if (idx >= 0) {
      leases[idx] = lease;
    } else {
      leases.push(lease);
    }
    this.leases.set(key, leases);
  }

  /** Find ready endpoints for a service. */
  findEndpoints(serviceName: string, version?: number): EndpointLease[] {
    const results: EndpointLease[] = [];
    for (const [key, leases] of this.leases) {
      if (key.startsWith(`${serviceName}/`)) {
        if (version !== undefined && !key.endsWith(`/v${version}`)) continue;
        for (const lease of leases) {
          if (lease.status === 'ready') results.push(lease);
        }
      }
    }
    return results;
  }

  /** Number of known services. */
  get size(): number {
    return this.manifests.size;
  }

  /**
   * Resolve all endpoints for a service, returning leases from all known versions.
   */
  resolveAll(serviceName: string): EndpointLease[] {
    return this.findEndpoints(serviceName);
  }

  /**
   * Fetch a contract manifest by service name and version.
   * Returns undefined if not locally known.
   */
  fetchContract(serviceName: string, version?: number): ContractManifest | undefined {
    return this.getManifest(serviceName, version);
  }

  /**
   * Register a change callback invoked whenever new manifests are added.
   * Returns an unsubscribe function.
   */
  onChange(callback: (manifest: ContractManifest) => void): () => void {
    this._changeCallbacks.push(callback);
    return () => {
      this._changeCallbacks = this._changeCallbacks.filter(cb => cb !== callback);
    };
  }

  private _changeCallbacks: ((manifest: ContractManifest) => void)[] = [];

  /**
   * Parse a manifest from a blob collection download.
   * The collection contains "manifest.json" as its first entry.
   */
  static parseManifestFromCollection(entries: [string, Uint8Array][]): ContractManifest {
    const manifestEntry = entries.find(([name]) => name === 'manifest.json');
    if (!manifestEntry) {
      throw new Error('collection does not contain manifest.json');
    }
    const json = new TextDecoder().decode(manifestEntry[1]);
    return manifestFromJson(json);
  }
}
