/**
 * Data-plane helpers and type names for the Rust `aster::{Blobs, Docs,
 * Gossip}` facades the NAPI clients wrap (`node.blobs()`, `node.docs()`,
 * `node.gossip()`).
 *
 * Doc and gossip event streams are `AsyncIterable`:
 *
 * ```ts
 * const events = await doc.subscribe();
 * for await (const ev of events) {
 *   if (ev.kind === 'insert_remote') console.log(ev.peer, ev.entry?.key);
 * }
 * ```
 */

import { loadNative } from './native.js';

/** Blob format: a single blob or a HashSeq collection. */
export type BlobFormat = 'raw' | 'hash_seq';
/**
 * How a multi-provider fetch splits a HashSeq. Providers are walked in the
 * given order with failover in both; `'split_children'` (HashSeq only)
 * fetches children concurrently, applying the order per child.
 */
export type FetchStrategy = 'whole' | 'split_children';
/** Doc ticket capability. */
export type ShareMode = 'read' | 'write';
/** The access a namespace grants this node locally. */
export type LocalAccess = 'read' | 'write';
/** Doc download policy mode. */
export type DownloadPolicyMode = 'everything' | 'nothing_except' | 'everything_except';
/** Doc event kinds. */
export type DocEventKind =
  | 'insert_local'
  | 'insert_remote'
  | 'content_ready'
  | 'pending_content_ready'
  | 'neighbor_up'
  | 'neighbor_down'
  | 'sync_finished';
/** Gossip topic event kinds. */
export type GossipEventKind = 'received' | 'neighbor_up' | 'neighbor_down' | 'lagged';

/**
 * The correlation fingerprint (64 hex) of a `(namespace, access)` pair —
 * what an `AuthorityReport` carries, so a caller can derive it from its own
 * grant record and compare. Correlation, not evidence.
 */
export function authorityFingerprint(namespace: string | Uint8Array, access: LocalAccess): string {
  return loadNative().authorityFingerprint(namespace, access);
}

/** The docs default-author id of a node secret key (equals the node id). */
export function defaultAuthorId(secretKey: string | Uint8Array): string {
  return loadNative().defaultAuthorId(secretKey);
}
