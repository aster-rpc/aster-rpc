/**
 * Node secret keys and **secret key files** — the one on-disk identity format
 * shared by the Rust SDK and every binding: the 32-byte Ed25519 key as 64 hex
 * characters plus a newline (Rust `aster::SecretKey`).
 *
 * Most code just sets `secretKeyFile` in {@link AsterConfig} and lets
 * `Node.start` load-or-create it; these helpers are for tooling and tests.
 */

import type { KeyMaterial } from './config.js';
import { loadNative } from './native.js';
import type { NodeId } from './node.js';

function hex(key: KeyMaterial): string {
  return typeof key === 'string' ? key : Buffer.from(key).toString('hex');
}

/** @group Node */
export const SecretKey = {
  /** A fresh random key (OS CSPRNG), as 64 hex characters. */
  generate(): string {
    return loadNative().secretKeyGenerate();
  },
  /** Read a secret key file; `null` when missing, throws when malformed. */
  readFile(path: string): string | null {
    return loadNative().secretKeyReadFile(path) ?? null;
  },
  /**
   * Write a new secret key file (mode 0600, parent directories created).
   * Refuses to overwrite an existing file.
   */
  writeFile(path: string, key: KeyMaterial): void {
    loadNative().secretKeyWriteFile(path, hex(key));
  },
  /** Load the key file at `path`, creating it with a fresh key if missing. */
  loadOrGenerate(path: string): string {
    return loadNative().secretKeyLoadOrGenerate(path);
  },
  /** The node id (public key) of `key`. */
  nodeId(key: KeyMaterial): NodeId {
    return loadNative().secretKeyPublic(hex(key)) as NodeId;
  },
};
