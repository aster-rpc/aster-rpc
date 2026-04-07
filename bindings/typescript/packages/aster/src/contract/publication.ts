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
 * Upload a pre-built collection of [name, data] entries to the blob store.
 * Returns the collection hash.
 */
export async function uploadCollection(
  blobsClient: { addBytes(data: Uint8Array): Promise<string> },
  entries: [name: string, data: Uint8Array][],
): Promise<string> {
  // Combine all entries into a single manifest+data blob (simplified)
  const index = entries.map(([name, data]) => ({ name, size: data.byteLength }));
  const header = new TextEncoder().encode(JSON.stringify(index) + '\n');
  const chunks: Uint8Array[] = [header];
  for (const [, data] of entries) chunks.push(data);
  const total = chunks.reduce((s, c) => s + c.byteLength, 0);
  const combined = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) { combined.set(c, offset); offset += c.byteLength; }
  return blobsClient.addBytes(combined);
}

/**
 * Fetch a collection from the blob store by hash and parse the entries.
 * Returns [name, data] pairs.
 */
export async function fetchFromCollection(
  blobsClient: { read(hash: string): Promise<Uint8Array> },
  collectionHash: string,
): Promise<[name: string, data: Uint8Array][]> {
  const combined = await blobsClient.read(collectionHash);
  // Find the newline after the JSON header
  let headerEnd = 0;
  for (let i = 0; i < combined.byteLength; i++) {
    if (combined[i] === 0x0a) { headerEnd = i + 1; break; }
  }
  const headerText = new TextDecoder().decode(combined.subarray(0, headerEnd));
  const index: Array<{ name: string; size: number }> = JSON.parse(headerText);
  const entries: [string, Uint8Array][] = [];
  let pos = headerEnd;
  for (const { name, size } of index) {
    entries.push([name, combined.subarray(pos, pos + size)]);
    pos += size;
  }
  return entries;
}

/**
 * Fetch a contract manifest from the blob store by its collection hash.
 */
export async function fetchContract(
  blobsClient: { read(hash: string): Promise<Uint8Array> },
  collectionHash: string,
): Promise<ContractManifest> {
  const { manifestFromJson } = await import('./manifest.js');
  const entries = await fetchFromCollection(blobsClient, collectionHash);
  const manifestEntry = entries.find(([name]) => name === 'manifest.json');
  if (!manifestEntry) throw new Error('collection does not contain manifest.json');
  return manifestFromJson(new TextDecoder().decode(manifestEntry[1]));
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
