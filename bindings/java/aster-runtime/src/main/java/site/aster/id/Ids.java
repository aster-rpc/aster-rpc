package site.aster.id;

import static site.aster.ffi.NativeCall.BYTES;
import static site.aster.ffi.NativeCall.intFn;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import site.aster.ffi.NativeCall;

/**
 * Typed 32-byte ids through the C ABI (3.4): parse / format / validate, plus the two pure
 * derivations every binding needs (secret key → node id, namespace secret → namespace id). The
 * rules are Rust's, so every binding accepts and rejects the same strings.
 *
 * <p>C functions: {@code aster_id_parse}, {@code aster_id_format}, {@code aster_id_validate},
 * {@code aster_secret_key_public_key}, {@code aster_namespace_secret_id}.
 */
public final class Ids {

  private Ids() {}

  private static final class H {
    static final MethodHandle PARSE =
        NativeCall.handle(
            "aster_id_parse", intFn(ValueLayout.JAVA_INT, BYTES, ValueLayout.ADDRESS));
    static final MethodHandle FORMAT =
        NativeCall.handle(
            "aster_id_format",
            intFn(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    static final MethodHandle VALIDATE =
        NativeCall.handle("aster_id_validate", intFn(ValueLayout.JAVA_INT, ValueLayout.ADDRESS));
    static final MethodHandle SECRET_PUBLIC =
        NativeCall.handle(
            "aster_secret_key_public_key", intFn(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    static final MethodHandle NAMESPACE_ID =
        NativeCall.handle(
            "aster_namespace_secret_id", intFn(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
  }

  /**
   * Parse 64 hex characters (either case, no surrounding whitespace) into 32 bytes.
   *
   * @throws IllegalArgumentException if the text is malformed or, for Ed25519 kinds, not a valid
   *     curve point
   */
  public static byte[] parse(IdKind kind, String hex) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = NativeCall.keyOut(arena);
      NativeCall.callChecked(
          H.PARSE, "aster_id_parse", kind.code, NativeCall.string(arena, hex), out);
      return NativeCall.readKey(out);
    }
  }

  /** Format 32 bytes as 64 lowercase hex characters. Does not validate the bytes. */
  public static String format(IdKind kind, byte[] id) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(65);
      NativeCall.callChecked(
          H.FORMAT, "aster_id_format", kind.code, NativeCall.key(arena, id, "id"), out);
      return new String(out.asSlice(0, 64).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8);
    }
  }

  /**
   * Check raw id bytes for {@code kind}.
   *
   * @throws IllegalArgumentException if they are not 32 bytes, or an Ed25519 kind is not a valid
   *     curve point
   */
  public static void validate(IdKind kind, byte[] id) {
    try (Arena arena = Arena.ofConfined()) {
      NativeCall.callChecked(
          H.VALIDATE, "aster_id_validate", kind.code, NativeCall.key(arena, id, "id"));
    }
  }

  /** {@link #validate} as a predicate. */
  public static boolean isValid(IdKind kind, byte[] id) {
    if (id == null || id.length != NativeCall.KEY_SIZE) {
      return false;
    }
    try {
      validate(kind, id);
      return true;
    } catch (IllegalArgumentException e) {
      return false;
    }
  }

  /** Whether {@code hex} parses as {@code kind}. */
  public static boolean isValid(IdKind kind, String hex) {
    try {
      parse(kind, hex);
      return true;
    } catch (IllegalArgumentException e) {
      return false;
    }
  }

  /** The Ed25519 public key (= node id) of a 32-byte node secret key. */
  public static byte[] publicKey(byte[] secretKey) {
    return derive(H.SECRET_PUBLIC, "aster_secret_key_public_key", secretKey, "secret key");
  }

  /** The node id (hex) of a 32-byte node secret key. */
  public static String nodeId(byte[] secretKey) {
    return format(IdKind.NODE_ID, publicKey(secretKey));
  }

  /** The public namespace id of a 32-byte namespace secret. */
  public static byte[] namespaceId(byte[] namespaceSecret) {
    return derive(H.NAMESPACE_ID, "aster_namespace_secret_id", namespaceSecret, "namespace secret");
  }

  private static byte[] derive(MethodHandle h, String name, byte[] in, String what) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = NativeCall.keyOut(arena);
      NativeCall.callChecked(h, name, NativeCall.key(arena, in, what), out);
      return NativeCall.readKey(out);
    }
  }
}
