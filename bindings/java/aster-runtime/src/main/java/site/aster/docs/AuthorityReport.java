package site.aster.docs;

import java.util.Arrays;
import java.util.HexFormat;

/**
 * What this node holds over a namespace, read back from the store ({@code aster::AuthorityReport}).
 * The access is what is stored, never what was requested. The fingerprint is a correlation value
 * over {@code (namespace, access)}, not proof over secret material.
 *
 * @param namespaceId the namespace id (64 hex)
 * @param access the access stored for it
 * @param fingerprint the 32-byte authority fingerprint
 */
public record AuthorityReport(String namespaceId, LocalAccess access, byte[] fingerprint) {

  public AuthorityReport {
    fingerprint = fingerprint.clone();
  }

  @Override
  public byte[] fingerprint() {
    return fingerprint.clone();
  }

  public String fingerprintHex() {
    return HexFormat.of().formatHex(fingerprint);
  }

  static AuthorityReport parse(int accessCode, byte[] payload) {
    if (payload.length != 64) {
      throw new IllegalArgumentException("authority report payload must be 64 bytes");
    }
    HexFormat hex = HexFormat.of();
    return new AuthorityReport(
        hex.formatHex(payload, 0, 32),
        LocalAccess.fromCode(accessCode),
        Arrays.copyOfRange(payload, 32, 64));
  }

  @Override
  public boolean equals(Object o) {
    return o instanceof AuthorityReport r
        && namespaceId.equals(r.namespaceId)
        && access == r.access
        && Arrays.equals(fingerprint, r.fingerprint);
  }

  @Override
  public int hashCode() {
    return (namespaceId.hashCode() * 31 + access.hashCode()) * 31 + Arrays.hashCode(fingerprint);
  }

  @Override
  public String toString() {
    return "AuthorityReport{" + namespaceId + ", " + access + ", " + fingerprintHex() + "}";
  }
}
