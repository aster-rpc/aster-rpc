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
 * Returns a list of [name, data] pairs suitable for upload. Layout
 * matches spec `Aster-ContractIdentity.md` §11.4:
 *
 * - `contract.bin` — canonical XLANG bytes of the ServiceContract
 * - `manifest.json` — human/machine-readable ContractManifest
 * - `types/{hex_hash}.bin` — one entry per TypeDef referenced by the
 *   contract, when a non-empty `typeDefs` map is passed
 *
 * Publishing TypeDef blobs lets cross-language dynamic clients decode
 * the canonical type graph directly rather than relying on the flat
 * manifest view. Optional so legacy callers that don't yet track
 * per-type canonical bytes keep working (the resulting collection is
 * spec-valid but missing the type blobs — Python parity requires them).
 *
 * @param manifest - The contract manifest
 * @param canonicalBytes - The canonical XLANG bytes of the ServiceContract
 * @param typeDefs - Optional map of hex BLAKE3 hash → canonical TypeDef bytes
 */
export function buildCollection(
  manifest: ContractManifest,
  canonicalBytes: Uint8Array,
  typeDefs?: ReadonlyMap<string, Uint8Array>,
): [name: string, data: Uint8Array][] {
  const encoder = new TextEncoder();
  const manifestJson = manifestToJson(manifest);

  const entries: [string, Uint8Array][] = [
    ['manifest.json', encoder.encode(manifestJson)],
    ['contract.bin', canonicalBytes],
  ];

  if (typeDefs) {
    // Sort by hash for deterministic collection ordering — matches the
    // Python publisher (bindings/python/aster/contract/publication.py:82).
    const sortedHashes = [...typeDefs.keys()].sort();
    for (const hashHex of sortedHashes) {
      entries.push([`types/${hashHex}.bin`, typeDefs.get(hashHex)!]);
    }
  }

  if (entries.length > MAX_COLLECTION_INDEX_ENTRIES) {
    throw new Error(`collection has ${entries.length} entries, max is ${MAX_COLLECTION_INDEX_ENTRIES}`);
  }

  return entries;
}

/**
 * Upload a pre-built collection of [name, data] entries to the blob store
 * as a native iroh HashSeq collection. GC protection is handled automatically.
 * Returns the collection hash.
 */
export async function uploadCollection(
  blobsClient: { addCollection(entries: [string, Uint8Array][]): Promise<string> },
  entries: [name: string, data: Uint8Array][],
): Promise<string> {
  return blobsClient.addCollection(entries);
}

/**
 * Fetch a collection from the blob store by hash using native HashSeq.
 * Returns [name, data] pairs.
 */
export async function fetchFromCollection(
  blobsClient: { listCollection(hash: string): Promise<Array<{ name: string; hash: string; size: number }>>; read(hash: string): Promise<Uint8Array> },
  collectionHash: string,
): Promise<[name: string, data: Uint8Array][]> {
  const collectionEntries = await blobsClient.listCollection(collectionHash);
  const entries: [string, Uint8Array][] = [];
  for (const { name, hash } of collectionEntries) {
    const data = await blobsClient.read(hash);
    entries.push([name, data]);
  }
  return entries;
}

/**
 * Fetch a contract manifest from the blob store by its collection hash.
 */
export async function fetchContract(
  blobsClient: { listCollection(hash: string): Promise<Array<{ name: string; hash: string; size: number }>>; read(hash: string): Promise<Uint8Array> },
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
  blobsClient: { addCollection(entries: [string, Uint8Array][]): Promise<string>; createCollectionTicket(hash: string): string },
  manifest: ContractManifest,
  canonicalBytes?: Uint8Array,
): Promise<ArtifactRef> {
  // Build collection entries and upload as native HashSeq
  const entries = buildCollection(manifest, canonicalBytes ?? new Uint8Array());
  const collectionHash = await blobsClient.addCollection(entries);
  const ticket = blobsClient.createCollectionTicket(collectionHash);

  return {
    contractId: manifest.contractId,
    collectionHash,
    collectionTicket: ticket,
  };
}
