package site.aster.server;

import static org.junit.jupiter.api.Assertions.*;

import java.io.ByteArrayInputStream;
import java.security.MessageDigest;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.security.interfaces.ECPublicKey;
import java.util.List;
import org.junit.jupiter.api.Test;
import site.aster.id.Ids;

class TlsCertificatesTest {
  @Test
  void signedCertificatesHaveCorrectHashesAndOwnership() throws Exception {
    byte[] secret = new byte[32];
    secret[0] = 7;
    String node = Ids.nodeId(secret);
    for (var cert :
        List.of(
            TlsCertificates.generateSelfSignedCertificate(List.of("localhost"), secret),
            TlsCertificates.generateWebTransportCertificate("jvm", List.of("127.0.0.1"), secret))) {
      X509Certificate parsed = parse(cert.certificatePem());
      assertArrayEquals(
          MessageDigest.getInstance("SHA-256").digest(parsed.getEncoded()), cert.sha256());
      TlsCertificates.verifyCertificateBinding(cert.certificatePem(), node);
      assertThrows(
          IllegalArgumentException.class,
          () -> TlsCertificates.verifyCertificateBinding(cert.certificatePem(), "00".repeat(32)));
      byte[] changed = cert.sha256();
      changed[0] ^= 1;
      assertFalse(java.util.Arrays.equals(changed, cert.sha256()));
    }
  }

  @Test
  void webTransportUsesShortLivedP256AndUnsignedFailsBinding() throws Exception {
    var cert = TlsCertificates.generateWebTransportCertificate("jvm", List.of());
    X509Certificate parsed = parse(cert.certificatePem());
    assertTrue(parsed.getNotAfter().getTime() - parsed.getNotBefore().getTime() < 14L * 86400000);
    assertEquals(
        256,
        ((ECPublicKey) parsed.getPublicKey()).getParams().getCurve().getField().getFieldSize());
    assertThrows(
        IllegalArgumentException.class,
        () ->
            TlsCertificates.verifyCertificateBinding(
                cert.certificatePem(), Ids.nodeId(new byte[32])));
    assertThrows(
        IllegalArgumentException.class,
        () -> TlsCertificates.verifyCertificateBinding(new byte[] {1}, "00".repeat(32)));
    assertThrows(
        IllegalArgumentException.class,
        () -> TlsCertificates.generateSelfSignedCertificate(List.of(), new byte[0]));
    assertThrows(
        IllegalArgumentException.class,
        () -> TlsCertificates.generateSelfSignedCertificate(List.of(), new byte[31]));
  }

  private static X509Certificate parse(byte[] pem) throws Exception {
    return (X509Certificate)
        CertificateFactory.getInstance("X.509").generateCertificate(new ByteArrayInputStream(pem));
  }
}
