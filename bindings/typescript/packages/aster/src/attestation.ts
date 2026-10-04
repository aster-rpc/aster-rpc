/**
 * Ownership attestations (Rust `aster::attestation`).
 *
 * An {@link AttestationChain} is a portable, signed artifact proving that a
 * *root* identity owns a *node* identity. Verification is fully offline:
 *
 * ```ts
 * const chain = attestRootNode(rootSecret, node.exportSecretKey(), { epoch: 1 });
 * const text = chain.toText(); // "aster.attestation.chain.v1:..."
 *
 * const verified = verifyChain(AttestationChain.fromText(text), [rootId], node.id);
 * verified.roleOf(rootId); // 'anchor'
 * ```
 *
 * Signing and verification run in Rust; the chain is opaque bytes here.
 */

import type { KeyMaterial } from './config.js';
import { loadNative } from './native.js';
import type { NodeId } from './node.js';

/** Policy options for a minted attestation edge. @group Trust */
export interface AttestOptions {
  /** Monotonic replay counter (caller-managed; default 0). */
  epoch?: number;
  /** Unix seconds before which the chain is invalid (0 / unset = no bound). */
  notBefore?: number | Date;
  /** Unix seconds after which the chain is invalid (0 / unset = no expiry). */
  notAfter?: number | Date;
}

/** An identity's role within a verified chain. @group Trust */
export type AttestationRole = 'node' | 'anchor' | 'intermediate';

/** Thrown when a chain fails to decode or verify. @group Trust */
export class AttestationError extends Error {
  override readonly name = 'AttestationError';
}

function unixSeconds(v: number | Date | undefined): number | undefined {
  return v instanceof Date ? Math.floor(v.getTime() / 1000) : v;
}

function hexId(id: string | Uint8Array): string {
  return typeof id === 'string' ? id.trim().toLowerCase() : Buffer.from(id).toString('hex');
}

function attestationCall<T>(f: () => T): T {
  try {
    return f();
  } catch (err) {
    throw new AttestationError(err instanceof Error ? err.message : String(err));
  }
}

/**
 * An ownership-attestation chain — the opaque, portable wire artifact.
 * @group Trust
 */
export class AttestationChain {
  private readonly _bytes: Uint8Array;

  private constructor(bytes: Uint8Array) {
    this._bytes = Uint8Array.from(bytes);
  }

  /** Wrap raw chain bytes (e.g. received from a peer or storage). */
  static fromBytes(bytes: Uint8Array): AttestationChain {
    return new AttestationChain(bytes);
  }

  /** Decode `aster.attestation.chain.v1:<base64url>` text. */
  static fromText(text: string): AttestationChain {
    return new AttestationChain(attestationCall(() => loadNative().attestationChainFromText(text)));
  }

  /** The chain's wire bytes (a copy). */
  get bytes(): Uint8Array {
    return Uint8Array.from(this._bytes);
  }

  /** Encode as `aster.attestation.chain.v1:<base64url>` text. */
  toText(): string {
    return loadNative().attestationChainToText(this._bytes);
  }

  /** Byte-wise equality. */
  equals(other: AttestationChain): boolean {
    return Buffer.from(this._bytes).equals(Buffer.from(other._bytes));
  }

  toString(): string {
    return `AttestationChain(${this._bytes.byteLength} bytes)`;
  }

  toJSON(): string {
    return this.toText();
  }
}

/**
 * The outcome of a successful {@link verifyChain}. Ids are node-level
 * identities (Ed25519 transport keys, 64 hex) — not docs authors.
 * @group Trust
 */
export class VerifiedChain {
  /** @internal */
  constructor(
    /** The node the chain binds to (== the expected node). */
    readonly node: NodeId,
    /** The trusted anchor the chain terminates at. */
    readonly anchor: NodeId,
    /** Intermediates between node and anchor, node-ward first. */
    readonly intermediates: readonly NodeId[],
    /** Number of edges. */
    readonly depth: number,
  ) {}

  /** Whether `id` is the leaf node this chain binds. */
  isNode(id: NodeId | string | Uint8Array): boolean {
    return this.node === hexId(id);
  }

  /** Whether `id` is the trusted anchor. */
  isAnchor(id: NodeId | string | Uint8Array): boolean {
    return this.anchor === hexId(id);
  }

  /** Whether `id` is an intermediate delegating identity. */
  isIntermediate(id: NodeId | string | Uint8Array): boolean {
    return this.intermediates.includes(hexId(id) as NodeId);
  }

  /** The role of `id`, or `undefined` if it is not a participant (`node` wins over `anchor`). */
  roleOf(id: NodeId | string | Uint8Array): AttestationRole | undefined {
    if (this.isNode(id)) return 'node';
    if (this.isAnchor(id)) return 'anchor';
    if (this.isIntermediate(id)) return 'intermediate';
    return undefined;
  }
}

/**
 * Mint a single-edge root↔node chain: the root attests it owns the node and
 * the node co-signs. Both secrets are 32-byte Ed25519 seeds (hex or bytes).
 * @group Trust
 */
export function attestRootNode(
  rootSecret: KeyMaterial,
  nodeSecret: KeyMaterial,
  options: AttestOptions = {},
): AttestationChain {
  const bytes = attestationCall(() =>
    loadNative().attestRootNode(rootSecret, nodeSecret, {
      epoch: options.epoch,
      notBefore: unixSeconds(options.notBefore),
      notAfter: unixSeconds(options.notAfter),
    }),
  );
  return AttestationChain.fromBytes(bytes);
}

/**
 * Verify `chain` end-to-end, offline: structure, the leaf binding to
 * `expectedNode`, each edge, the top anchor being in `trustedAnchors`, and
 * every signature. Throws {@link AttestationError} on the first failure.
 * @group Trust
 */
export function verifyChain(
  chain: AttestationChain | Uint8Array,
  trustedAnchors: Iterable<NodeId | string | Uint8Array>,
  expectedNode: NodeId | string | Uint8Array,
): VerifiedChain {
  const bytes = chain instanceof AttestationChain ? chain.bytes : chain;
  const v = attestationCall(() =>
    loadNative().attestationVerifyChain(bytes, [...trustedAnchors], expectedNode),
  );
  return new VerifiedChain(
    v.node as NodeId,
    v.anchor as NodeId,
    Object.freeze(v.intermediates.map((i) => i as NodeId)),
    v.depth,
  );
}

/**
 * The Ed25519 public key (node id form) of a 32-byte secret — the identity
 * an attestation edge signs as.
 * @group Trust
 */
export function attestationPublicKey(secret: KeyMaterial): NodeId {
  return loadNative().attestationPublicKey(secret) as NodeId;
}
