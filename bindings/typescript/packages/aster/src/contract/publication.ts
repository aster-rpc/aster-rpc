/**
 * Contract publication — write contracts to registry doc + blob store.
 *
 * Spec reference: Aster-ContractIdentity.md S11.4
 *
 * On server startup, each service's contract is:
 * 1. Serialized to canonical bytes
 * 2. Stored as a blob collection (contract.bin + manifest.json)
 * 3. Referenced in the registry doc via an ArtifactRef
 */

import { type ContractManifest, manifestToJson } from './manifest.js';
import { MAX_COLLECTION_INDEX_ENTRIES } from '../limits.js';

/** Reference to a contract artifact in the blob store. */
export interface ArtifactRef {
  contractId: string;
  collectionHash: string;
  collectionTicket: string;
}

/**
 * Build a collection of contract artifacts for blob storage.
 *
 * Returns a list of [name, data] pairs suitable for upload:
 * - "manifest.json" — human/machine-readable contract metadata
 * - "contract.bin" — canonical XLANG bytes (for hash verification)
 *
 * @param manifest - The contract manifest
 * @param canonicalBytes - The canonical XLANG bytes of the ServiceContract
 */
export function buildCollection(
  manifest: ContractManifest,
  canonicalBytes: Uint8Array,
): [name: string, data: Uint8Array][] {
  const encoder = new TextEncoder();
  const manifestJson = manifestToJson(manifest);

  const entries: [string, Uint8Array][] = [
    ['manifest.json', encoder.encode(manifestJson)],
    ['contract.bin', canonicalBytes],
  ];

  if (entries.length > MAX_COLLECTION_INDEX_ENTRIES) {
    throw new Error(`collection has ${entries.length} entries, max is ${MAX_COLLECTION_INDEX_ENTRIES}`);
  }

  return entries;
}

/**
 * Publish a contract collection to the blob store.
 *
 * This is the high-level function called by AsterServer on startup.
 * It takes a BlobsClient (from NAPI), uploads the collection, and
 * returns an ArtifactRef.
 */
export async function publishContract(
  blobsClient: { addBytesAsCollection(name: string, data: Buffer | Uint8Array): Promise<string>; createCollectionTicket(hash: string): string },
  manifest: ContractManifest,
  _canonicalBytes?: Uint8Array,
): Promise<ArtifactRef> {
  // Upload manifest.json as a collection entry
  const manifestJson = new TextEncoder().encode(manifestToJson(manifest));
  const collectionHash = await blobsClient.addBytesAsCollection('manifest.json', manifestJson);
  const ticket = blobsClient.createCollectionTicket(collectionHash);

  return {
    contractId: manifest.contractId,
    collectionHash,
    collectionTicket: ticket,
  };
}
