// STATUS: TBD - possibly retired.
// Aster authentication/authorization will be designed once in Rust and
// exposed to every binding (docs/_internal/bindings/BINDINGS_CATCHUP_PLAN.md,
// D3). Until then this module is frozen: fix bugs, but do not extend it
// or port it to other languages.
/**
 * Ed25519 signing/verification helpers (used by delegated admission).
 *
 * The legacy consumer / producer credential formats that lived here were
 * retired with consumer admission: `AsterServer` admits peers with
 * Enrollment (`docs/_internal/working_ideas/aster-enrollment.md` §15).
 * Ed25519 operations use @noble/ed25519 for portability.
 */

/** Reserved attribute keys. */
export const ATTR_ROLE = 'aster.role';
export const ATTR_NAME = 'aster.name';

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
