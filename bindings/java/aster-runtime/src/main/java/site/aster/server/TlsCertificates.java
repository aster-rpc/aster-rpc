package site.aster.server;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.List;
import java.util.Objects;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.NativeCall;

/** Certificate generation and signed node ownership of TLS keys (ABI 4.12). */
public final class TlsCertificates {
  private TlsCertificates() {}

  /** PEM material and SHA-256 of the DER certificate. Accessors return defensive copies. */
  public record GeneratedCertificate(byte[] certificatePem, byte[] privateKeyPem, byte[] sha256) {
    public GeneratedCertificate {
      certificatePem = certificatePem.clone();
      privateKeyPem = privateKeyPem.clone();
      sha256 = sha256.clone();
    }

    @Override
    public byte[] certificatePem() {
      return certificatePem.clone();
    }

    @Override
    public byte[] privateKeyPem() {
      return privateKeyPem.clone();
    }

    @Override
    public byte[] sha256() {
      return sha256.clone();
    }
  }

  private static final MemoryLayout CERT =
      MemoryLayout.structLayout(
          NativeCall.BYTES,
          NativeCall.BYTES,
          MemoryLayout.sequenceLayout(32, ValueLayout.JAVA_BYTE));
  private static final MethodHandle SELF =
      NativeCall.handle(
          "aster_tls_generate_self_signed",
          NativeCall.intFn(IrohLibrary.ASTER_BYTES_LIST, NativeCall.BYTES, ValueLayout.ADDRESS));
  private static final MethodHandle WT =
      NativeCall.handle(
          "aster_tls_generate_webtransport",
          NativeCall.intFn(
              NativeCall.BYTES,
              IrohLibrary.ASTER_BYTES_LIST,
              NativeCall.BYTES,
              ValueLayout.ADDRESS));
  private static final MethodHandle VERIFY =
      NativeCall.handle(
          "aster_tls_verify_binding", NativeCall.intFn(NativeCall.BYTES, NativeCall.BYTES));

  public static GeneratedCertificate generateSelfSignedCertificate() {
    return generateSelfSignedCertificate(List.of());
  }

  public static GeneratedCertificate generateSelfSignedCertificate(List<String> sans) {
    return generateSelfSignedCertificate(sans, null);
  }

  /**
   * Null secret creates a self-signed certificate without node identity binding; otherwise exactly
   * 32 identity-secret bytes.
   */
  public static GeneratedCertificate generateSelfSignedCertificate(
      List<String> sans, byte[] nodeSecret) {
    return generate(null, sans, nodeSecret);
  }

  public static GeneratedCertificate generateWebTransportCertificate(String subject) {
    return generateWebTransportCertificate(subject, List.of());
  }

  public static GeneratedCertificate generateWebTransportCertificate(
      String subject, List<String> sans) {
    return generateWebTransportCertificate(subject, sans, null);
  }

  /** P-256 certificate valid for less than 14 days, suitable for explicit WebTransport pinning. */
  public static GeneratedCertificate generateWebTransportCertificate(
      String subject, List<String> sans, byte[] nodeSecret) {
    return generate(Objects.requireNonNull(subject, "subject"), sans, nodeSecret);
  }

  private static GeneratedCertificate generate(
      String subject, List<String> sans, byte[] nodeSecret) {
    Objects.requireNonNull(sans, "sans");
    if (nodeSecret != null && nodeSecret.length != 32)
      throw new IllegalArgumentException("node secret must be 32 bytes");
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(CERT);
      MemorySegment names = DataPlane.utf8List(arena, sans);
      MemorySegment secret = NativeCall.bytes(arena, nodeSecret);
      if (subject == null)
        NativeCall.callChecked(SELF, "generate self-signed certificate", names, secret, out);
      else
        NativeCall.callChecked(
            WT,
            "generate WebTransport certificate",
            NativeCall.string(arena, subject),
            names,
            secret,
            out);
      // Release both library-owned outputs even when copying the first one fails.
      byte[] cert;
      byte[] key;
      try {
        cert = NativeCall.takeOwned(out.asSlice(0, 16));
      } finally {
        key = NativeCall.takeOwned(out.asSlice(16, 16));
      }
      return new GeneratedCertificate(
          cert, key, out.asSlice(32, 32).toArray(ValueLayout.JAVA_BYTE));
    }
  }

  /**
   * Verify node ownership of the TLS key. Obtain expectedNodeId from a trusted source, not only
   * from the certificate. This verifies the signed TLS public-key binding, not the whole
   * certificate or chain. Expiry, hostname and PKI trust are separate checks.
   */
  public static void verifyCertificateBinding(byte[] certificatePem, String expectedNodeId) {
    Objects.requireNonNull(certificatePem, "certificatePem");
    Objects.requireNonNull(expectedNodeId, "expectedNodeId");
    try (Arena arena = Arena.ofConfined()) {
      NativeCall.callChecked(
          VERIFY,
          "verify certificate binding",
          NativeCall.bytes(arena, certificatePem),
          NativeCall.string(arena, expectedNodeId));
    }
  }
}
