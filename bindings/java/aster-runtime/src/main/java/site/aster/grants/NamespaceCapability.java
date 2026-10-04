package site.aster.grants;

import java.util.Arrays;
import site.aster.id.Ids;

/**
 * A docs namespace capability: write (carries the 32-byte namespace secret) or read (carries the
 * 32-byte namespace id). In the C ABI this is {@code (aster_local_access_t, key)}.
 */
public final class NamespaceCapability {

  private final boolean canWrite;
  private final byte[] key;

  private NamespaceCapability(boolean canWrite, byte[] key) {
    if (key == null || key.length != 32) {
      throw new IllegalArgumentException("namespace key must be 32 bytes");
    }
    this.canWrite = canWrite;
    this.key = key.clone();
  }

  /** A read capability over a namespace id. */
  public static NamespaceCapability read(byte[] namespaceId) {
    return new NamespaceCapability(false, namespaceId);
  }

  /** A write capability carrying the namespace secret. */
  public static NamespaceCapability write(byte[] namespaceSecret) {
    return new NamespaceCapability(true, namespaceSecret);
  }

  static NamespaceCapability of(int access, byte[] key) {
    return switch (access) {
      case Grants.ACCESS_WRITE -> write(key);
      case Grants.ACCESS_READ -> read(key);
      default -> throw new IllegalStateException("unknown aster_local_access_t " + access);
    };
  }

  int access() {
    return canWrite ? Grants.ACCESS_WRITE : Grants.ACCESS_READ;
  }

  /** Whether this capability carries write authority. */
  public boolean canWrite() {
    return canWrite;
  }

  /** The raw key: the namespace secret for write, the namespace id for read. */
  public byte[] key() {
    return key.clone();
  }

  /** The public namespace id (derived from the secret for a write capability). */
  public byte[] namespaceId() {
    return canWrite ? Ids.namespaceId(key) : key.clone();
  }

  /** The Fory encoding shared with every binding. */
  public byte[] encode() {
    return Grants.encodeCapability(this);
  }

  /** Decode the Fory encoding. */
  public static NamespaceCapability decode(byte[] bytes) {
    return Grants.decodeCapability(bytes);
  }

  @Override
  public boolean equals(Object o) {
    return o instanceof NamespaceCapability c
        && c.canWrite == canWrite
        && Arrays.equals(c.key, key);
  }

  @Override
  public int hashCode() {
    return Arrays.hashCode(key) * 31 + (canWrite ? 1 : 0);
  }

  @Override
  public String toString() {
    return canWrite
        ? "NamespaceCapability.Write(<redacted>)"
        : "NamespaceCapability.Read(<redacted>)";
  }
}
