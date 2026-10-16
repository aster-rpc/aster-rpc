package site.aster.codec;

import java.util.Arrays;
import java.util.Objects;

/**
 * The contract type {@code ulid} (Aster-ContractIdentity.md, "Time, UUID, and Decimal"; Rust {@code
 * aster::rpc::Ulid}): 16 bytes, a 48-bit big-endian millisecond timestamp then 80 random bits. On
 * the wire it is the same 16-byte Fory BINARY as {@code uuid}; its own contract identity tells
 * consumers to present it as 26 characters of Crockford base32.
 *
 * <p>Immutable. {@link #toString()} is the canonical upper-case form; {@link #parse} accepts either
 * case and refuses a first character above {@code 7} (the value would exceed 128 bits).
 */
public final class Ulid implements Comparable<Ulid> {

  private static final char[] ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ".toCharArray();
  private static final int[] DECODE = new int[128];

  static {
    Arrays.fill(DECODE, -1);
    for (int i = 0; i < ALPHABET.length; i++) {
      DECODE[ALPHABET[i]] = i;
      DECODE[Character.toLowerCase(ALPHABET[i])] = i;
    }
  }

  /** The all-zero ULID. */
  public static final Ulid ZERO = new Ulid(new byte[16]);

  private final byte[] bytes;

  private Ulid(byte[] bytes) {
    this.bytes = bytes;
  }

  /**
   * The ULID with these 16 bytes (copied).
   *
   * @throws IllegalArgumentException if {@code bytes} is not exactly 16 bytes
   */
  public static Ulid fromBytes(byte[] bytes) {
    Objects.requireNonNull(bytes, "bytes");
    if (bytes.length != 16) {
      throw new IllegalArgumentException("a ulid is 16 bytes, got " + bytes.length);
    }
    return new Ulid(bytes.clone());
  }

  /**
   * Parse 26 characters of Crockford base32 (case-insensitive).
   *
   * @throws IllegalArgumentException on any other length, a character outside the alphabet, or a
   *     first character above {@code 7}
   */
  public static Ulid parse(String text) {
    Objects.requireNonNull(text, "text");
    if (text.length() != 26) {
      throw new IllegalArgumentException("a ulid is 26 characters, got " + text.length());
    }
    int[] digits = new int[26];
    for (int i = 0; i < 26; i++) {
      char c = text.charAt(i);
      int d = c < 128 ? DECODE[c] : -1;
      if (d < 0) {
        throw new IllegalArgumentException("invalid ulid character '" + c + "'");
      }
      digits[i] = d;
    }
    if (digits[0] > 7) {
      throw new IllegalArgumentException("ulid out of range (first character above 7)");
    }
    // 26 * 5 = 130 bits, of which the top 2 are zero: read big-endian into 16 bytes.
    byte[] out = new byte[16];
    int bit = 0;
    for (int i = 0; i < 26; i++) {
      for (int b = 4; b >= 0; b--, bit++) {
        if (bit < 2) continue;
        if (((digits[i] >> b) & 1) != 0) {
          int pos = bit - 2;
          out[pos >> 3] |= (byte) (0x80 >>> (pos & 7));
        }
      }
    }
    return new Ulid(out);
  }

  /** A copy of the 16 bytes. */
  public byte[] toBytes() {
    return bytes.clone();
  }

  /** The 48-bit millisecond timestamp. */
  public long timestampMillis() {
    long ms = 0;
    for (int i = 0; i < 6; i++) {
      ms = (ms << 8) | (bytes[i] & 0xff);
    }
    return ms;
  }

  /** 26 characters of upper-case Crockford base32. */
  @Override
  public String toString() {
    char[] out = new char[26];
    int bit = 0;
    for (int i = 0; i < 26; i++) {
      int v = 0;
      for (int b = 0; b < 5; b++, bit++) {
        int pos = bit - 2;
        int set = pos < 0 ? 0 : (bytes[pos >> 3] >> (7 - (pos & 7))) & 1;
        v = (v << 1) | set;
      }
      out[i] = ALPHABET[v];
    }
    return new String(out);
  }

  @Override
  public boolean equals(Object o) {
    return o instanceof Ulid u && Arrays.equals(bytes, u.bytes);
  }

  @Override
  public int hashCode() {
    return Arrays.hashCode(bytes);
  }

  @Override
  public int compareTo(Ulid o) {
    return Arrays.compareUnsigned(bytes, o.bytes);
  }
}
