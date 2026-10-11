import { loadNative } from './native.js';

export interface GeneratedCertificate {
  certificatePem: Uint8Array;
  privateKeyPem: Uint8Array;
  /** SHA-256 of the DER certificate, for explicit certificate pinning. */
  sha256: Uint8Array;
}

/** Create a self-signed certificate, optionally signed by a 32-byte node identity secret. */
export function generateSelfSignedCertificate(sans: string[] = [], nodeSecret?: Uint8Array): GeneratedCertificate {
  return loadNative().generateSelfSignedCertificate(sans, nodeSecret && Buffer.from(nodeSecret));
}

/** Create a P-256 certificate with a validity period shorter than 14 days. */
export function generateWebTransportCertificate(subject: string, sans: string[] = [], nodeSecret?: Uint8Array): GeneratedCertificate {
  return loadNative().generateWebTransportCertificate(subject, sans, nodeSecret && Buffer.from(nodeSecret));
}

/** Verify signed node ownership using a trusted expectedNodeId, not an id read only from the certificate.
 * This proves the signed TLS public-key binding, not the whole certificate, chain, expiry, hostname or PKI trust. */
export function verifyCertificateBinding(certificatePem: Uint8Array, expectedNodeId: string): void {
  loadNative().verifyCertificateBinding(Buffer.from(certificatePem), expectedNodeId);
}
