package site.aster.crypto;

import static site.aster.ffi.NativeCall.BYTES;
import static site.aster.ffi.NativeCall.intFn;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import site.aster.ffi.NativeCall;

/**
 * HPKE through the C ABI (3.4): HPKE Base mode, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256,
 * ChaCha20-Poly1305. Envelopes are the Fory {@code aster.hpke.Envelope} bytes every binding
 * produces.
 *
 * <p>Recipient keys can be minted ({@link #generateKeyPair}) or derived from an Aster identity:
 * {@link #x25519SecretFromIdentity} on the node's Ed25519 secret and {@link
 * #x25519PublicFromIdentity} / {@link #x25519PublicFromNodeId} on its public key agree by
 * construction (libsodium-compatible). Binding the encryption key to the identity means the
 * identity secret also unlocks anything sealed to the node.
 *
 * @see site.aster.grants.Grants for the grant-shaped seal / open
 */
public final class Hpke {

  private Hpke() {}

  /** An X25519 keypair. */
  public record KeyPair(byte[] privateKey, byte[] publicKey) {
    @Override
    public String toString() {
      return "Hpke.KeyPair(<redacted>)";
    }
  }

  private static final class H {
    static final MethodHandle GENERATE =
        NativeCall.handle(
            "aster_hpke_generate_keypair", intFn(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    static final MethodHandle PUBLIC_FROM_PRIVATE =
        NativeCall.handle(
            "aster_hpke_public_key_from_private", intFn(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    static final MethodHandle SECRET_FROM_IDENTITY =
        NativeCall.handle(
            "aster_hpke_x25519_secret_from_identity",
            intFn(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    static final MethodHandle PUBLIC_FROM_IDENTITY =
        NativeCall.handle(
            "aster_hpke_x25519_public_from_identity",
            intFn(ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    static final MethodHandle PUBLIC_FROM_NODE_ID =
        NativeCall.handle(
            "aster_hpke_x25519_public_from_node_id", intFn(BYTES, ValueLayout.ADDRESS));
    static final MethodHandle SEAL =
        NativeCall.handle(
            "aster_hpke_seal", intFn(ValueLayout.ADDRESS, BYTES, BYTES, ValueLayout.ADDRESS));
    static final MethodHandle OPEN =
        NativeCall.handle(
            "aster_hpke_open", intFn(ValueLayout.ADDRESS, BYTES, BYTES, ValueLayout.ADDRESS));
  }

  /** Generate a fresh X25519 keypair from the OS CSPRNG. */
  public static KeyPair generateKeyPair() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment sk = NativeCall.keyOut(arena);
      MemorySegment pk = NativeCall.keyOut(arena);
      NativeCall.callChecked(H.GENERATE, "aster_hpke_generate_keypair", sk, pk);
      return new KeyPair(NativeCall.readKey(sk), NativeCall.readKey(pk));
    }
  }

  /** The X25519 public key of a 32-byte private key. */
  public static byte[] publicKeyFromPrivate(byte[] privateKey) {
    return map(H.PUBLIC_FROM_PRIVATE, "aster_hpke_public_key_from_private", privateKey);
  }

  /** The X25519 private key derived from a 32-byte Ed25519 identity secret. */
  public static byte[] x25519SecretFromIdentity(byte[] identitySecret) {
    return map(H.SECRET_FROM_IDENTITY, "aster_hpke_x25519_secret_from_identity", identitySecret);
  }

  /**
   * The X25519 public key derived from a 32-byte Ed25519 identity public key.
   *
   * @throws IllegalArgumentException for an off-curve, non-canonical or small-order key
   */
  public static byte[] x25519PublicFromIdentity(byte[] identityPublic) {
    return map(H.PUBLIC_FROM_IDENTITY, "aster_hpke_x25519_public_from_identity", identityPublic);
  }

  /** {@link #x25519PublicFromIdentity} for a node id in hex. */
  public static byte[] x25519PublicFromNodeId(String nodeIdHex) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = NativeCall.keyOut(arena);
      NativeCall.callChecked(
          H.PUBLIC_FROM_NODE_ID,
          "aster_hpke_x25519_public_from_node_id",
          NativeCall.string(arena, nodeIdHex),
          out);
      return NativeCall.readKey(out);
    }
  }

  /** Seal {@code plaintext} to a 32-byte X25519 public key, binding {@code aad}. Randomized. */
  public static byte[] seal(byte[] recipientPublicKey, byte[] aad, byte[] plaintext) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.SEAL,
          "aster_hpke_seal",
          NativeCall.key(arena, recipientPublicKey, "recipient public key"),
          NativeCall.bytes(arena, aad),
          NativeCall.bytes(arena, plaintext),
          out);
      return NativeCall.takeOwned(out);
    }
  }

  /**
   * Open an envelope with the recipient's 32-byte X25519 private key and the same {@code aad}.
   *
   * @throws site.aster.exception.VerificationFailedException if it does not open
   */
  public static byte[] open(byte[] recipientPrivateKey, byte[] aad, byte[] envelope) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.OPEN,
          "aster_hpke_open",
          NativeCall.key(arena, recipientPrivateKey, "recipient private key"),
          NativeCall.bytes(arena, aad),
          NativeCall.bytes(arena, envelope),
          out);
      return NativeCall.takeOwned(out);
    }
  }

  private static byte[] map(MethodHandle h, String name, byte[] in) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = NativeCall.keyOut(arena);
      NativeCall.callChecked(h, name, NativeCall.key(arena, in, "key"), out);
      return NativeCall.readKey(out);
    }
  }
}
