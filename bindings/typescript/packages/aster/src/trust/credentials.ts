/**
 * Credential types and ed25519 signing/verification.
 *
 * Spec reference: Aster-trust-spec.md
 *
 * Signing bytes computation delegates to Rust core via NAPI.
 * Ed25519 operations use @noble/ed25519 for portability.
 */

/** Producer enrollment credential. */
export interface EnrollmentCredential {
  endpointId: string;
  rootPubkey: string; // hex
  expiresAt: number; // epoch seconds
  attributes: Record<string, string>;
  signature: string; // hex
}

/** Consumer enrollment credential. */
export interface ConsumerEnrollmentCredential {
  credentialType: 'policy' | 'ott';
  rootPubkey: string;
  expiresAt: number;
  attributes: Record<string, string>;
  endpointId?: string;
  nonce?: string; // hex
  signature: string; // hex
}

/** Reserved attribute keys. */
export const ATTR_ROLE = 'aster.role';
export const ATTR_NAME = 'aster.name';
export const ATTR_IID_PROVIDER = 'aster.iid_provider';
export const ATTR_IID_ACCOUNT = 'aster.iid_account';
export const ATTR_IID_REGION = 'aster.iid_region';
export const ATTR_IID_ROLE_ARN = 'aster.iid_role_arn';

// ── Crypto backend (lazy-loaded once, then cached) ──────────────────────────
//
// We try @noble/ed25519 first (portable, works in browsers/bun/node) and fall
// back to node:crypto. Both modules are loaded once at first use; subsequent
// calls hit the cached references — critical for hot paths like per-RPC
// signature verification.

type EdModule = {
  utils: { randomPrivateKey(): Uint8Array };
  getPublicKeyAsync(privateKey: Uint8Array): Promise<Uint8Array>;
  signAsync(message: Uint8Array, privateKey: Uint8Array): Promise<Uint8Array>;
  verifyAsync(signature: Uint8Array, message: Uint8Array, publicKey: Uint8Array): Promise<boolean>;
};

type NodeCryptoModule = typeof import('node:crypto');

let _edModule: EdModule | null | undefined; // undefined = not tried, null = unavailable
let _nodeCrypto: NodeCryptoModule | null | undefined;

async function getEd(): Promise<EdModule | null> {
  if (_edModule !== undefined) return _edModule;
  try {
    // @ts-ignore — optional dependency
    _edModule = (await import('@noble/ed25519')) as EdModule;
  } catch {
    _edModule = null;
  }
  return _edModule;
}

async function getNodeCrypto(): Promise<NodeCryptoModule> {
  if (_nodeCrypto) return _nodeCrypto;
  _nodeCrypto = await import('node:crypto');
  return _nodeCrypto;
}

/**
 * Generate an ed25519 keypair.
 * Returns [privateKey (32 bytes), publicKey (32 bytes)].
 *
 * Requires @noble/ed25519:
 * ```ts
 * import { utils } from '@noble/ed25519';
 * const privKey = utils.randomPrivateKey();
 * const pubKey = await getPublicKeyAsync(privKey);
 * ```
 */
export async function generateKeypair(): Promise<[Uint8Array, Uint8Array]> {
  const ed = await getEd();
  if (ed) {
    const privateKey = ed.utils.randomPrivateKey();
    const publicKey = await ed.getPublicKeyAsync(privateKey);
    return [privateKey, publicKey];
  }
  const { generateKeyPairSync } = await getNodeCrypto();
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const privBuf = privateKey.export({ type: 'pkcs8', format: 'der' }).subarray(-32);
  const pubBuf = publicKey.export({ type: 'spki', format: 'der' }).subarray(-32);
  return [new Uint8Array(privBuf), new Uint8Array(pubBuf)];
}

/**
 * Sign a message with an ed25519 private key.
 * Returns 64-byte signature.
 */
export async function sign(privateKey: Uint8Array, message: Uint8Array): Promise<Uint8Array> {
  const ed = await getEd();
  if (ed) {
    return ed.signAsync(message, privateKey);
  }
  const { sign: cryptoSign, createPrivateKey } = await getNodeCrypto();
  const key = createPrivateKey({
    key: Buffer.concat([
      Buffer.from('302e020100300506032b657004220420', 'hex'),
      Buffer.from(privateKey),
    ]),
    format: 'der',
    type: 'pkcs8',
  });
  return new Uint8Array(cryptoSign(null, Buffer.from(message), key));
}

/**
 * Verify an ed25519 signature.
 */
export async function verify(
  publicKey: Uint8Array,
  message: Uint8Array,
  signature: Uint8Array,
): Promise<boolean> {
  const ed = await getEd();
  if (ed) {
    return ed.verifyAsync(signature, message, publicKey);
  }
  const { verify: cryptoVerify, createPublicKey } = await getNodeCrypto();
  const key = createPublicKey({
    key: Buffer.concat([
      Buffer.from('302a300506032b6570032100', 'hex'),
      Buffer.from(publicKey),
    ]),
    format: 'der',
    type: 'spki',
  });
  return cryptoVerify(null, Buffer.from(message), key, Buffer.from(signature));
}

// ── Hex helpers ──────────────────────────────────────────────────────────────

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substring(i, i + 2), 16);
  }
  return bytes;
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');
}

// ── Canonical signing bytes ─────────────────────────────────────────────────
// Spec reference: Aster-trust-spec.md §2.2, §2.4
// Authoritative implementation: core/src/signing.rs
// This TS version mirrors the Python helper for credential sign/verify.

/** Canonical JSON: UTF-8, sorted keys, no extra whitespace. */
export function canonicalJson(attributes: Record<string, string>): Uint8Array {
  const sorted = Object.keys(attributes).sort();
  const obj: Record<string, string> = {};
  for (const k of sorted) obj[k] = attributes[k];
  const json = JSON.stringify(obj);
  return new TextEncoder().encode(json);
}

/** u64 big-endian encoding. */
function writeU64BE(value: number): Uint8Array {
  const buf = new ArrayBuffer(8);
  const view = new DataView(buf);
  // JS numbers are safe up to 2^53; for epoch seconds this is fine
  view.setUint32(0, Math.floor(value / 0x100000000));
  view.setUint32(4, value >>> 0);
  return new Uint8Array(buf);
}

/** Signing bytes for EnrollmentCredential (producer). */
export function producerSigningBytes(cred: EnrollmentCredential): Uint8Array {
  const parts: Uint8Array[] = [
    new TextEncoder().encode(cred.endpointId),
    hexToBytes(cred.rootPubkey),       // 32 bytes
    writeU64BE(cred.expiresAt),        // 8 bytes
    canonicalJson(cred.attributes),
  ];
  return concatBytes(parts);
}

/** Signing bytes for ConsumerEnrollmentCredential. */
export function consumerSigningBytes(cred: ConsumerEnrollmentCredential): Uint8Array {
  const typeCode = cred.credentialType === 'ott' ? new Uint8Array([0x01]) : new Uint8Array([0x00]);

  let eidPart: Uint8Array;
  if (cred.endpointId != null) {
    const eidBytes = new TextEncoder().encode(cred.endpointId);
    eidPart = concatBytes([new Uint8Array([0x01]), eidBytes]);
  } else {
    eidPart = new Uint8Array([0x00]);
  }

  let noncePart: Uint8Array;
  if (cred.nonce != null) {
    noncePart = concatBytes([new Uint8Array([0x01]), hexToBytes(cred.nonce)]);
  } else {
    noncePart = new Uint8Array([0x00]);
  }

  return concatBytes([
    typeCode,
    eidPart,
    hexToBytes(cred.rootPubkey),       // 32 bytes
    writeU64BE(cred.expiresAt),        // 8 bytes
    canonicalJson(cred.attributes),
    noncePart,
  ]);
}

/** Compute signing bytes for any credential type. */
export function credentialSigningBytes(
  cred: EnrollmentCredential | ConsumerEnrollmentCredential,
): Uint8Array {
  if ('endpointId' in cred && 'credentialType' in cred) {
    return consumerSigningBytes(cred as ConsumerEnrollmentCredential);
  }
  if ('endpointId' in cred && !('credentialType' in cred)) {
    return producerSigningBytes(cred as EnrollmentCredential);
  }
  throw new Error(`Unsupported credential type`);
}

/**
 * Sign a credential with the root private key.
 * Returns 64-byte signature as hex string.
 */
export async function signCredential(
  cred: EnrollmentCredential | ConsumerEnrollmentCredential,
  rootPrivkeyRaw: Uint8Array,
): Promise<string> {
  const msg = credentialSigningBytes(cred);
  const sig = await sign(rootPrivkeyRaw, msg);
  return bytesToHex(sig);
}

/**
 * Verify a credential's signature.
 * If rootPubkeyHex is provided, it overrides cred.rootPubkey.
 * Returns true on success, false on any failure.
 */
export async function verifyCredentialSignature(
  cred: EnrollmentCredential | ConsumerEnrollmentCredential,
  rootPubkeyHex?: string,
): Promise<boolean> {
  const pubkeyHex = rootPubkeyHex ?? cred.rootPubkey;
  try {
    const msg = credentialSigningBytes(cred);
    return await verify(hexToBytes(pubkeyHex), msg, hexToBytes(cred.signature));
  } catch {
    return false;
  }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

function concatBytes(arrays: Uint8Array[]): Uint8Array {
  const totalLen = arrays.reduce((sum, a) => sum + a.length, 0);
  const result = new Uint8Array(totalLen);
  let offset = 0;
  for (const a of arrays) {
    result.set(a, offset);
    offset += a.length;
  }
  return result;
}

// ── Key utilities ─────────────────────────────────────────────────────────────

/**
 * Generate a root keypair (alias for generateKeypair).
 * Returns [privateKey, publicKey] as raw 32-byte arrays.
 */
export async function generateRootKeypair(): Promise<[Uint8Array, Uint8Array]> {
  return generateKeypair();
}

/**
 * Load a raw 32-byte private key.
 * Returns the key as-is (validates length).
 */
export function loadPrivateKey(privRaw: Uint8Array): Uint8Array {
  if (privRaw.byteLength !== 32) {
    throw new TypeError(`Invalid private key length: expected 32, got ${privRaw.byteLength}`);
  }
  return privRaw;
}

/**
 * Load a raw 32-byte public key.
 * Returns the key as-is (validates length).
 */
export function loadPublicKey(pubRaw: Uint8Array): Uint8Array {
  if (pubRaw.byteLength !== 32) {
    throw new TypeError(`Invalid public key length: expected 32, got ${pubRaw.byteLength}`);
  }
  return pubRaw;
}

/**
 * Verify an ed25519 signature.
 * Alias for the low-level `verify()` with a consistent signature.
 */
export async function verifySignature(
  publicKey: Uint8Array,
  message: Uint8Array,
  signature: Uint8Array,
): Promise<boolean> {
  return verify(publicKey, message, signature);
}

export { hexToBytes, bytesToHex, concatBytes };
