package site.aster.attestation;

import static site.aster.ffi.NativeCall.BYTES;
import static site.aster.ffi.NativeCall.intFn;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import site.aster.ffi.NativeCall;

/**
 * Ownership attestations through the C ABI (3.4): a chain proves that a root identity owns a node
 * identity (optionally via intermediates up to a trusted anchor). Verification is offline.
 *
 * <p>C functions: {@code aster_attest_root_node}, {@code aster_attestation_chain_{to,from}_text},
 * {@code aster_attestation_verify_chain}, {@code aster_verified_chain_release}.
 */
public final class Attestation {

  private Attestation() {}

  /** {@code aster_attest_options_t}, 32 bytes. */
  public static final MemoryLayout ATTEST_OPTIONS =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("reserved"), // 4
          ValueLayout.JAVA_LONG.withName("epoch"), // 8
          ValueLayout.JAVA_LONG.withName("not_before"), // 16
          ValueLayout.JAVA_LONG.withName("not_after")); // 24

  /** {@code aster_verified_chain_t}, 96 bytes. */
  public static final MemoryLayout VERIFIED_CHAIN =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("depth"), // 4
          MemoryLayout.sequenceLayout(32, ValueLayout.JAVA_BYTE).withName("node"), // 8
          MemoryLayout.sequenceLayout(32, ValueLayout.JAVA_BYTE).withName("anchor"), // 40
          ValueLayout.ADDRESS.withName("intermediates"), // 72
          ValueLayout.JAVA_LONG.withName("intermediates_count"), // 80
          ValueLayout.ADDRESS.withName("storage")); // 88

  private static long off(MemoryLayout l, String f) {
    return l.byteOffset(MemoryLayout.PathElement.groupElement(f));
  }

  private static final class H {
    static final MethodHandle ATTEST =
        NativeCall.handle(
            "aster_attest_root_node",
            intFn(
                ValueLayout.ADDRESS,
                ValueLayout.ADDRESS,
                ValueLayout.ADDRESS,
                ValueLayout.ADDRESS));
    static final MethodHandle TO_TEXT =
        NativeCall.handle("aster_attestation_chain_to_text", intFn(BYTES, ValueLayout.ADDRESS));
    static final MethodHandle FROM_TEXT =
        NativeCall.handle("aster_attestation_chain_from_text", intFn(BYTES, ValueLayout.ADDRESS));
    static final MethodHandle VERIFY =
        NativeCall.handle(
            "aster_attestation_verify_chain",
            intFn(
                BYTES,
                ValueLayout.ADDRESS,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS,
                ValueLayout.ADDRESS));
    static final MethodHandle RELEASE =
        NativeCall.handle("aster_verified_chain_release", intFn(ValueLayout.ADDRESS));
  }

  /**
   * Mint a single-edge root↔node chain: the root attests it owns the node and the node co-signs.
   * Both secrets are 32-byte Ed25519 seeds. Returns the chain's wire bytes.
   */
  public static byte[] attestRootNode(byte[] rootSecret, byte[] nodeSecret, AttestOptions options) {
    AttestOptions o = options == null ? AttestOptions.DEFAULT : options;
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment opts = arena.allocate(ATTEST_OPTIONS);
      opts.set(ValueLayout.JAVA_INT, 0, (int) ATTEST_OPTIONS.byteSize());
      opts.set(ValueLayout.JAVA_LONG, off(ATTEST_OPTIONS, "epoch"), o.epoch());
      opts.set(ValueLayout.JAVA_LONG, off(ATTEST_OPTIONS, "not_before"), o.notBefore());
      opts.set(ValueLayout.JAVA_LONG, off(ATTEST_OPTIONS, "not_after"), o.notAfter());
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.ATTEST,
          "aster_attest_root_node",
          NativeCall.key(arena, rootSecret, "root secret"),
          NativeCall.key(arena, nodeSecret, "node secret"),
          opts,
          out);
      return NativeCall.takeOwned(out);
    }
  }

  /** {@link #attestRootNode} with {@link AttestOptions#DEFAULT}. */
  public static byte[] attestRootNode(byte[] rootSecret, byte[] nodeSecret) {
    return attestRootNode(rootSecret, nodeSecret, AttestOptions.DEFAULT);
  }

  /** Encode chain bytes as {@code aster.attestation.chain.v1:<base64url>}. */
  public static String toText(byte[] chain) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.TO_TEXT, "aster_attestation_chain_to_text", NativeCall.bytes(arena, chain), out);
      return new String(NativeCall.takeOwned(out), StandardCharsets.UTF_8);
    }
  }

  /**
   * Decode {@code aster.attestation.chain.v1:<base64url>} text to chain bytes (not verified).
   *
   * @throws IllegalArgumentException for malformed text
   */
  public static byte[] fromText(String text) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.FROM_TEXT, "aster_attestation_chain_from_text", NativeCall.string(arena, text), out);
      return NativeCall.takeOwned(out);
    }
  }

  /**
   * Verify a chain end to end, offline: structure, the leaf binding to {@code expectedNode},
   * bridging, the top anchor being one of {@code trustedAnchors}, and every signature.
   *
   * @throws site.aster.exception.VerificationFailedException naming the first failed check
   */
  public static VerifiedChain verifyChain(
      byte[] chain, List<byte[]> trustedAnchors, byte[] expectedNode) {
    try (Arena arena = Arena.ofConfined()) {
      int n = trustedAnchors.size();
      MemorySegment anchors = MemorySegment.NULL;
      if (n > 0) {
        anchors = arena.allocate((long) n * NativeCall.KEY_SIZE);
        for (int i = 0; i < n; i++) {
          MemorySegment.copy(
              NativeCall.key(arena, trustedAnchors.get(i), "trusted anchor"),
              0,
              anchors,
              (long) i * NativeCall.KEY_SIZE,
              NativeCall.KEY_SIZE);
        }
      }
      MemorySegment out = arena.allocate(VERIFIED_CHAIN);
      NativeCall.callChecked(
          H.VERIFY,
          "aster_attestation_verify_chain",
          NativeCall.bytes(arena, chain),
          anchors,
          (long) n,
          NativeCall.key(arena, expectedNode, "expected node"),
          out);
      try {
        long count = out.get(ValueLayout.JAVA_LONG, off(VERIFIED_CHAIN, "intermediates_count"));
        List<byte[]> mids = new ArrayList<>((int) count);
        if (count > 0) {
          MemorySegment p =
              out.get(ValueLayout.ADDRESS, off(VERIFIED_CHAIN, "intermediates"))
                  .reinterpret(count * NativeCall.KEY_SIZE);
          for (long i = 0; i < count; i++) {
            mids.add(
                p.asSlice(i * NativeCall.KEY_SIZE, NativeCall.KEY_SIZE)
                    .toArray(ValueLayout.JAVA_BYTE));
          }
        }
        return new VerifiedChain(
            out.asSlice(off(VERIFIED_CHAIN, "node"), 32).toArray(ValueLayout.JAVA_BYTE),
            out.asSlice(off(VERIFIED_CHAIN, "anchor"), 32).toArray(ValueLayout.JAVA_BYTE),
            mids,
            out.get(ValueLayout.JAVA_INT, off(VERIFIED_CHAIN, "depth")));
      } finally {
        NativeCall.call(H.RELEASE, out);
      }
    }
  }
}
