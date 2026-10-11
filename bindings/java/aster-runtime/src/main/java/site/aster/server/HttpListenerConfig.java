package site.aster.server;

import java.util.Objects;

/** Operator-supplied TLS material and binding limits for an authenticated listener. */
public record HttpListenerConfig(
    String bindAddress,
    byte[] certificatePem,
    byte[] privateKeyPem,
    boolean webTransport,
    int maxPendingAuth,
    int maxAuthPayloadBytes) {
  public HttpListenerConfig {
    Objects.requireNonNull(bindAddress, "bindAddress");
    certificatePem = Objects.requireNonNull(certificatePem, "certificatePem").clone();
    privateKeyPem = Objects.requireNonNull(privateKeyPem, "privateKeyPem").clone();
    if (bindAddress.isEmpty() || certificatePem.length == 0 || privateKeyPem.length == 0) {
      throw new IllegalArgumentException("bind address and TLS PEM must be nonempty");
    }
    if (maxPendingAuth < 0 || maxAuthPayloadBytes < 0) {
      throw new IllegalArgumentException("auth limits must be nonnegative (zero selects defaults)");
    }
  }

  /** Defaults to HTTPS only, 256 pending auth calls and 64 KiB auth payloads. */
  public HttpListenerConfig(String bindAddress, byte[] certificatePem, byte[] privateKeyPem) {
    this(bindAddress, certificatePem, privateKeyPem, false, 0, 0);
  }

  @Override
  public byte[] certificatePem() {
    return certificatePem.clone();
  }

  @Override
  public byte[] privateKeyPem() {
    return privateKeyPem.clone();
  }
}
