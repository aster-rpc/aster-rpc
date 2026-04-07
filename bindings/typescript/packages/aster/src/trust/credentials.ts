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
  try {
    // @ts-ignore — optional dependency, falls back to Node crypto
    const ed = await import('@noble/ed25519');
    const privateKey = ed.utils.randomPrivateKey();
    const publicKey = await ed.getPublicKeyAsync(privateKey);
    return [privateKey, publicKey];
  } catch {
    // Fallback to Node crypto
    const { generateKeyPairSync } = await import('node:crypto');
    const { publicKey, privateKey } = generateKeyPairSync('ed25519');
    const privBuf = privateKey.export({ type: 'pkcs8', format: 'der' }).subarray(-32);
    const pubBuf = publicKey.export({ type: 'spki', format: 'der' }).subarray(-32);
    return [new Uint8Array(privBuf), new Uint8Array(pubBuf)];
  }
}

/**
 * Sign a message with an ed25519 private key.
 * Returns 64-byte signature.
 */
export async function sign(privateKey: Uint8Array, message: Uint8Array): Promise<Uint8Array> {
  try {
    // @ts-ignore — optional dependency, falls back to Node crypto
    const ed = await import('@noble/ed25519');
    return ed.signAsync(message, privateKey);
  } catch {
    const { sign: cryptoSign, createPrivateKey } = await import('node:crypto');
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
}

/**
 * Verify an ed25519 signature.
 */
export async function verify(
  publicKey: Uint8Array,
  message: Uint8Array,
  signature: Uint8Array,
): Promise<boolean> {
  try {
    // @ts-ignore — optional dependency, falls back to Node crypto
    const ed = await import('@noble/ed25519');
    return ed.verifyAsync(signature, message, publicKey);
  } catch {
    const { verify: cryptoVerify, createPublicKey } = await import('node:crypto');
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
}
