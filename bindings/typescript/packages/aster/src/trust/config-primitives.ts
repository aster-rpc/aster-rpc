/**
 * Simple trust-config primitives.
 *
 * Binary records are encoded by Rust core as Apache Fory XLANG. This module is
 * a typed TypeScript facade over the native helpers; it deliberately does not
 * reimplement the Fory wire shape in JavaScript.
 */

export const HPKE_ENVELOPE_ALG =
  'HPKE-Base-X25519-HKDF-SHA256-ChaCha20Poly1305' as const;

const INSPECT_CUSTOM = Symbol.for('nodejs.util.inspect.custom');

interface NativeDecodedNamespaceCapability {
  kind: 'read' | 'write';
  namespaceId: Uint8Array;
  canWrite: boolean;
  material: Uint8Array;
}

interface NativeHpkeKeyPair {
  privateKey: Uint8Array;
  publicKey: Uint8Array;
}

interface NativeHpkeEnvelope {
  alg: string;
  encappedKey: Uint8Array;
  ciphertext: Uint8Array;
}

interface NativeTrustConfigPrimitives {
  namespaceSecretId(secret: Uint8Array): Uint8Array;
  namespaceCapabilityEncodeRead(namespaceId: Uint8Array): Uint8Array;
  namespaceCapabilityEncodeWrite(namespaceSecret: Uint8Array): Uint8Array;
  namespaceCapabilityDecode(data: Uint8Array): NativeDecodedNamespaceCapability;
  hpkeEnvelopeAlg(): string;
  hpkeGenerateKeypair(): NativeHpkeKeyPair;
  hpkePublicKeyFromPrivate(privateKey: Uint8Array): Uint8Array;
  hpkeSeal(
    recipientPublicKey: Uint8Array,
    associatedData: Uint8Array,
    plaintext: Uint8Array,
  ): NativeHpkeEnvelope;
  hpkeOpen(
    recipientPrivateKey: Uint8Array,
    associatedData: Uint8Array,
    envelope: NativeHpkeEnvelope,
  ): Uint8Array;
  hpkeEnvelopeEncode(envelope: NativeHpkeEnvelope): Uint8Array;
  hpkeEnvelopeDecode(data: Uint8Array): NativeHpkeEnvelope;
}

let _native: NativeTrustConfigPrimitives | undefined;

export function setNativeTrustConfigPrimitives(native: NativeTrustConfigPrimitives): void {
  _native = native;
}

function requireNative(): NativeTrustConfigPrimitives {
  if (_native) return _native;
  try {
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    _native = require('@aster-rpc/transport') as NativeTrustConfigPrimitives;
    return _native;
  } catch (err) {
    throw new Error(
      'Native trust-config primitives are unavailable. Ensure @aster-rpc/transport is installed ' +
      `and loadable. Cause: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}

function copyBytes(bytes: Uint8Array): Uint8Array {
  return Uint8Array.from(bytes);
}

export function namespaceSecretId(secret: Uint8Array): Uint8Array {
  return copyBytes(requireNative().namespaceSecretId(secret));
}

export class NamespaceCapability {
  private constructor(
    readonly kind: 'read' | 'write',
    private readonly _namespaceId: Uint8Array,
    private readonly _material: Uint8Array,
  ) {}

  static read(namespaceId: Uint8Array): NamespaceCapability {
    if (namespaceId.byteLength !== 32) {
      throw new Error('namespace id must be 32 bytes');
    }
    return new NamespaceCapability('read', copyBytes(namespaceId), copyBytes(namespaceId));
  }

  static write(namespaceSecret: Uint8Array): NamespaceCapability {
    if (namespaceSecret.byteLength !== 32) {
      throw new Error('namespace secret must be 32 bytes');
    }
    return new NamespaceCapability(
      'write',
      namespaceSecretId(namespaceSecret),
      copyBytes(namespaceSecret),
    );
  }

  static decodeFory(data: Uint8Array): NamespaceCapability {
    const decoded = requireNative().namespaceCapabilityDecode(data);
    return new NamespaceCapability(
      decoded.kind,
      copyBytes(decoded.namespaceId),
      copyBytes(decoded.material),
    );
  }

  static decodeCanonical(data: Uint8Array): NamespaceCapability {
    return NamespaceCapability.decodeFory(data);
  }

  get namespaceId(): Uint8Array {
    return copyBytes(this._namespaceId);
  }

  get canWrite(): boolean {
    return this.kind === 'write';
  }

  encodeFory(): Uint8Array {
    const native = requireNative();
    return copyBytes(
      this.kind === 'read'
        ? native.namespaceCapabilityEncodeRead(this._material)
        : native.namespaceCapabilityEncodeWrite(this._material),
    );
  }

  encodeCanonical(): Uint8Array {
    return this.encodeFory();
  }

  exposeMaterial(): Uint8Array {
    return copyBytes(this._material);
  }

  exposeSecret(): Uint8Array {
    if (this.kind !== 'write') {
      throw new Error('read namespace capability has no namespace secret');
    }
    return copyBytes(this._material);
  }

  toString(): string {
    return this.kind === 'read'
      ? 'NamespaceCapability.read(<redacted>)'
      : 'NamespaceCapability.write(<redacted>)';
  }

  toJSON(): Record<string, unknown> {
    return {
      kind: this.kind,
      namespaceId: '<redacted>',
      canWrite: this.canWrite,
      material: '<redacted>',
    };
  }

  [INSPECT_CUSTOM](): string {
    return this.toString();
  }
}

export interface HpkeKeyPair {
  privateKey: Uint8Array;
  publicKey: Uint8Array;
}

export class HpkeEnvelope {
  constructor(
    readonly alg: string,
    private readonly _encappedKey: Uint8Array,
    private readonly _ciphertext: Uint8Array,
  ) {
    if (alg !== HPKE_ENVELOPE_ALG) {
      throw new Error(`HPKE envelope alg must be ${HPKE_ENVELOPE_ALG}, got ${alg}`);
    }
    if (_encappedKey.byteLength !== 32) {
      throw new Error('HPKE encapped key must be 32 bytes');
    }
    if (_ciphertext.byteLength < 16) {
      throw new Error('HPKE ciphertext must include a 16-byte authentication tag');
    }
  }

  static seal(
    recipientPublicKey: Uint8Array,
    associatedData: Uint8Array,
    plaintext: Uint8Array,
  ): HpkeEnvelope {
    return HpkeEnvelope.fromNative(
      requireNative().hpkeSeal(recipientPublicKey, associatedData, plaintext),
    );
  }

  static decodeFory(data: Uint8Array): HpkeEnvelope {
    return HpkeEnvelope.fromNative(requireNative().hpkeEnvelopeDecode(data));
  }

  static decodeCanonical(data: Uint8Array): HpkeEnvelope {
    return HpkeEnvelope.decodeFory(data);
  }

  private static fromNative(envelope: NativeHpkeEnvelope): HpkeEnvelope {
    return new HpkeEnvelope(
      envelope.alg,
      copyBytes(envelope.encappedKey),
      copyBytes(envelope.ciphertext),
    );
  }

  get encappedKey(): Uint8Array {
    return copyBytes(this._encappedKey);
  }

  get ciphertext(): Uint8Array {
    return copyBytes(this._ciphertext);
  }

  encodeFory(): Uint8Array {
    return copyBytes(requireNative().hpkeEnvelopeEncode(this.toNative()));
  }

  encodeCanonical(): Uint8Array {
    return this.encodeFory();
  }

  open(recipientPrivateKey: Uint8Array, associatedData: Uint8Array): Uint8Array {
    return hpkeOpen(recipientPrivateKey, associatedData, this);
  }

  toNative(): NativeHpkeEnvelope {
    return {
      alg: this.alg,
      encappedKey: copyBytes(this._encappedKey),
      ciphertext: copyBytes(this._ciphertext),
    };
  }

  toString(): string {
    return `HpkeEnvelope(alg=${this.alg}, encappedKeyLen=${this._encappedKey.byteLength}, ciphertextLen=${this._ciphertext.byteLength})`;
  }

  [INSPECT_CUSTOM](): string {
    return this.toString();
  }
}

export function hpkeGenerateKeypair(): HpkeKeyPair {
  const keypair = requireNative().hpkeGenerateKeypair();
  return {
    privateKey: copyBytes(keypair.privateKey),
    publicKey: copyBytes(keypair.publicKey),
  };
}

export function hpkePublicKeyFromPrivate(privateKey: Uint8Array): Uint8Array {
  return copyBytes(requireNative().hpkePublicKeyFromPrivate(privateKey));
}

export function hpkeSeal(
  recipientPublicKey: Uint8Array,
  associatedData: Uint8Array,
  plaintext: Uint8Array,
): HpkeEnvelope {
  return HpkeEnvelope.seal(recipientPublicKey, associatedData, plaintext);
}

export function hpkeOpen(
  recipientPrivateKey: Uint8Array,
  associatedData: Uint8Array,
  envelope: HpkeEnvelope,
): Uint8Array {
  return copyBytes(
    requireNative().hpkeOpen(recipientPrivateKey, associatedData, envelope.toNative()),
  );
}
