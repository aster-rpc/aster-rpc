package site.aster.grants;

import static site.aster.ffi.NativeCall.BYTES;
import static site.aster.ffi.NativeCall.intFn;

import java.lang.foreign.AddressLayout;
import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import site.aster.exception.VerificationFailedException;
import site.aster.ffi.NativeCall;

/**
 * Sealed grants through the C ABI (3.4), the same implementation the Rust SDK, Python and
 * TypeScript use ({@code aster_transport_core::grants}): a secret HPKE-sealed to exactly one node,
 * with a {@link GrantContext} bound into the associated data so the envelope cannot be replayed at
 * another node, resource, role or doc path. Golden vectors: {@code
 * conformance/vectors/grants.json}.
 *
 * <p>Typical flow: the granter {@link #seal}s (or {@link #sealNamespace}s) and writes a {@link
 * GrantRecord#active} at {@link #grantKey}; the recipient calls {@link #reconcile} (or {@link
 * #reconcileNamespace}) whenever that key changes and acts on the {@link GrantOutcome}. Revoke by
 * writing {@link GrantRecord#revoked}, never by deleting the key.
 */
public final class Grants {

  private Grants() {}

  static final int ACCESS_WRITE = 1; // ASTER_LOCAL_ACCESS_WRITE
  static final int ACCESS_READ = 2; // ASTER_LOCAL_ACCESS_READ

  /** {@code aster_grant_context_t}, 104 bytes. */
  public static final MemoryLayout GRANT_CONTEXT =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("reserved"), // 4
          BYTES.withName("app"), // 8
          BYTES.withName("granter"), // 24
          BYTES.withName("recipient"), // 40
          BYTES.withName("resource"), // 56
          BYTES.withName("path"), // 72
          BYTES.withName("role")); // 88

  /** {@code aster_grant_record_t}, 72 bytes. */
  public static final MemoryLayout GRANT_RECORD =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("state"), // 4
          ValueLayout.JAVA_INT.withName("context_version"), // 8
          ValueLayout.JAVA_INT.withName("reserved"), // 12
          BYTES.withName("role"), // 16
          BYTES.withName("envelope"), // 32
          BYTES.withName("reason"), // 48
          ValueLayout.ADDRESS.withName("storage")); // 64

  /** {@code aster_grant_outcome_t}, 72 bytes. */
  public static final MemoryLayout GRANT_OUTCOME =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("kind"), // 4
          ValueLayout.JAVA_INT.withName("access"), // 8
          ValueLayout.JAVA_INT.withName("drops_capability"), // 12
          BYTES.withName("payload"), // 16
          BYTES.withName("reason"), // 32
          BYTES.withName("role"), // 48
          ValueLayout.ADDRESS.withName("storage")); // 64

  private static long off(MemoryLayout l, String f) {
    return l.byteOffset(MemoryLayout.PathElement.groupElement(f));
  }

  private static final ValueLayout.OfInt I = ValueLayout.JAVA_INT;
  private static final AddressLayout A = ValueLayout.ADDRESS;

  private static final class H {
    static final MethodHandle AAD = NativeCall.handle("aster_grant_context_aad", intFn(A, A));
    static final MethodHandle KEY = NativeCall.handle("aster_grant_key", intFn(BYTES, BYTES, A));
    static final MethodHandle CHECK_ROLE =
        NativeCall.handle("aster_grant_check_role", intFn(BYTES, I));
    static final MethodHandle SEAL = NativeCall.handle("aster_grant_seal", intFn(A, BYTES, A));
    static final MethodHandle SEAL_TO_KEY =
        NativeCall.handle("aster_grant_seal_to_key", intFn(A, A, BYTES, A));
    static final MethodHandle OPEN = NativeCall.handle("aster_grant_open", intFn(A, A, BYTES, A));
    static final MethodHandle OPEN_WITH_KEY =
        NativeCall.handle("aster_grant_open_with_key", intFn(A, A, BYTES, A));
    static final MethodHandle CAP_ENCODE =
        NativeCall.handle("aster_namespace_capability_encode", intFn(I, A, A));
    static final MethodHandle CAP_DECODE =
        NativeCall.handle("aster_namespace_capability_decode", intFn(BYTES, A, A));
    static final MethodHandle NS_SEAL =
        NativeCall.handle("aster_namespace_grant_seal", intFn(A, I, A, A));
    static final MethodHandle NS_OPEN =
        NativeCall.handle("aster_namespace_grant_open", intFn(A, A, BYTES, A, A));
    static final MethodHandle RECORD_ENCODE =
        NativeCall.handle("aster_grant_record_encode", intFn(A, A));
    static final MethodHandle RECORD_DECODE =
        NativeCall.handle("aster_grant_record_decode", intFn(BYTES, A));
    static final MethodHandle RECORD_RELEASE =
        NativeCall.handle("aster_grant_record_release", intFn(A));
    static final MethodHandle RECONCILE =
        NativeCall.handle("aster_grant_reconcile", intFn(A, A, A, A));
    static final MethodHandle NS_RECONCILE =
        NativeCall.handle("aster_namespace_grant_reconcile", intFn(A, A, A, A));
    static final MethodHandle OUTCOME_RELEASE =
        NativeCall.handle("aster_grant_outcome_release", intFn(A));
  }

  // ─── context, key, role rule ───────────────────────────────────────────────

  /**
   * The canonical grant AAD: the domain {@code aster/sealed-grant/v1}, then app, granter,
   * recipient, resource, path and role, each prefixed with its u32 big-endian byte length.
   */
  public static byte[] aad(GrantContext ctx) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(H.AAD, "aster_grant_context_aad", context(arena, ctx), out);
      return NativeCall.takeOwned(out);
    }
  }

  /** The conventional doc key for a grant record: {@code <prefix>/grants/<recipient>}. */
  public static String grantKey(String prefix, String recipientNodeId) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.KEY,
          "aster_grant_key",
          NativeCall.string(arena, prefix),
          NativeCall.string(arena, recipientNodeId),
          out);
      return new String(NativeCall.takeOwned(out), StandardCharsets.UTF_8);
    }
  }

  /**
   * The role ↔ capability rule: {@code read} needs a read capability, {@code write} a write one;
   * any other role (case-sensitive) carries no implication.
   */
  public static boolean roleMatches(String role, boolean canWrite) {
    try {
      checkRole(role, canWrite);
      return true;
    } catch (VerificationFailedException e) {
      return false;
    }
  }

  /**
   * {@link #roleMatches}, throwing.
   *
   * @throws VerificationFailedException if the role does not match the capability
   */
  public static void checkRole(String role, boolean canWrite) {
    try (Arena arena = Arena.ofConfined()) {
      NativeCall.callChecked(
          H.CHECK_ROLE, "aster_grant_check_role", NativeCall.string(arena, role), canWrite ? 1 : 0);
    }
  }

  // ─── raw seal / open ───────────────────────────────────────────────────────

  /**
   * Seal {@code plaintext} to {@code ctx.recipient()}'s identity-derived key. Randomized.
   *
   * @throws IllegalArgumentException if the recipient is not a valid node id
   */
  public static byte[] seal(GrantContext ctx, byte[] plaintext) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.SEAL, "aster_grant_seal", context(arena, ctx), NativeCall.bytes(arena, plaintext), out);
      return NativeCall.takeOwned(out);
    }
  }

  /** {@link #seal} to an explicit 32-byte X25519 public key. */
  public static byte[] sealToKey(byte[] recipientPublicKey, GrantContext ctx, byte[] plaintext) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.SEAL_TO_KEY,
          "aster_grant_seal_to_key",
          NativeCall.key(arena, recipientPublicKey, "recipient public key"),
          context(arena, ctx),
          NativeCall.bytes(arena, plaintext),
          out);
      return NativeCall.takeOwned(out);
    }
  }

  /**
   * Open with the recipient's 32-byte Ed25519 identity secret under the same context.
   *
   * @throws VerificationFailedException if the envelope is corrupt or any context field or the key
   *     differs
   */
  public static byte[] open(byte[] identitySecret, GrantContext ctx, byte[] envelope) {
    return openWith(H.OPEN, "aster_grant_open", identitySecret, ctx, envelope);
  }

  /** {@link #open} with an explicit 32-byte X25519 private key. */
  public static byte[] openWithKey(byte[] recipientPrivateKey, GrantContext ctx, byte[] envelope) {
    return openWith(
        H.OPEN_WITH_KEY, "aster_grant_open_with_key", recipientPrivateKey, ctx, envelope);
  }

  private static byte[] openWith(
      MethodHandle h, String name, byte[] key, GrantContext ctx, byte[] envelope) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          h,
          name,
          NativeCall.key(arena, key, "key"),
          context(arena, ctx),
          NativeCall.bytes(arena, envelope),
          out);
      return NativeCall.takeOwned(out);
    }
  }

  // ─── namespace capabilities ────────────────────────────────────────────────

  static byte[] encodeCapability(NamespaceCapability cap) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.CAP_ENCODE,
          "aster_namespace_capability_encode",
          cap.access(),
          NativeCall.key(arena, cap.key(), "key"),
          out);
      return NativeCall.takeOwned(out);
    }
  }

  static NamespaceCapability decodeCapability(byte[] bytes) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment access = arena.allocate(ValueLayout.JAVA_INT);
      MemorySegment key = NativeCall.keyOut(arena);
      NativeCall.callChecked(
          H.CAP_DECODE,
          "aster_namespace_capability_decode",
          NativeCall.bytes(arena, bytes),
          access,
          key);
      return NamespaceCapability.of(access.get(ValueLayout.JAVA_INT, 0), NativeCall.readKey(key));
    }
  }

  /**
   * Seal a namespace capability after checking the role rule.
   *
   * @throws VerificationFailedException if {@code ctx.role()} does not match the capability
   */
  public static byte[] sealNamespace(GrantContext ctx, NamespaceCapability capability) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(
          H.NS_SEAL,
          "aster_namespace_grant_seal",
          context(arena, ctx),
          capability.access(),
          NativeCall.key(arena, capability.key(), "key"),
          out);
      return NativeCall.takeOwned(out);
    }
  }

  /**
   * Open a namespace-capability grant and re-check the role rule.
   *
   * @throws VerificationFailedException if it does not open or the role does not match
   */
  public static NamespaceCapability openNamespace(
      byte[] identitySecret, GrantContext ctx, byte[] envelope) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment access = arena.allocate(ValueLayout.JAVA_INT);
      MemorySegment key = NativeCall.keyOut(arena);
      NativeCall.callChecked(
          H.NS_OPEN,
          "aster_namespace_grant_open",
          NativeCall.key(arena, identitySecret, "identity secret"),
          context(arena, ctx),
          NativeCall.bytes(arena, envelope),
          access,
          key);
      return NamespaceCapability.of(access.get(ValueLayout.JAVA_INT, 0), NativeCall.readKey(key));
    }
  }

  // ─── records ───────────────────────────────────────────────────────────────

  static byte[] encodeRecord(GrantRecord r) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment s = arena.allocate(GRANT_RECORD);
      s.set(I, 0, (int) GRANT_RECORD.byteSize());
      s.set(I, off(GRANT_RECORD, "state"), r.state().ordinal());
      s.set(I, off(GRANT_RECORD, "context_version"), r.contextVersion());
      NativeCall.putString(arena, s, off(GRANT_RECORD, "role"), r.role());
      NativeCall.putBytes(arena, s, off(GRANT_RECORD, "envelope"), r.envelope());
      NativeCall.putString(arena, s, off(GRANT_RECORD, "reason"), r.reason());
      MemorySegment out = arena.allocate(BYTES);
      NativeCall.callChecked(H.RECORD_ENCODE, "aster_grant_record_encode", s, out);
      return NativeCall.takeOwned(out);
    }
  }

  static GrantRecord decodeRecord(byte[] bytes) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(GRANT_RECORD);
      NativeCall.callChecked(
          H.RECORD_DECODE, "aster_grant_record_decode", NativeCall.bytes(arena, bytes), out);
      try {
        return new GrantRecord(
            GrantRecord.State.values()[out.get(I, off(GRANT_RECORD, "state"))],
            out.get(I, off(GRANT_RECORD, "context_version")),
            NativeCall.getString(out, off(GRANT_RECORD, "role")),
            NativeCall.getBytes(out, off(GRANT_RECORD, "envelope")),
            NativeCall.getString(out, off(GRANT_RECORD, "reason")));
      } finally {
        NativeCall.call(H.RECORD_RELEASE, out);
      }
    }
  }

  // ─── reconcile ─────────────────────────────────────────────────────────────

  /**
   * Decide what to do with the record at {@code ctx.path()}: {@code record} is the entry's bytes,
   * or {@code null} when the key has no entry. Opens the raw payload with the recipient's identity
   * secret when the grant is active.
   */
  public static GrantOutcome<byte[]> reconcile(
      byte[] identitySecret, GrantContext ctx, byte[] record) {
    return reconcileWith(
        H.RECONCILE, "aster_grant_reconcile", identitySecret, ctx, record, (access, p) -> p);
  }

  /** {@link #reconcile} for namespace-capability grants (role mismatch → unopenable). */
  public static GrantOutcome<NamespaceCapability> reconcileNamespace(
      byte[] identitySecret, GrantContext ctx, byte[] record) {
    return reconcileWith(
        H.NS_RECONCILE,
        "aster_namespace_grant_reconcile",
        identitySecret,
        ctx,
        record,
        NamespaceCapability::of);
  }

  private interface Payload<T> {
    T from(int access, byte[] payload);
  }

  private static <T> GrantOutcome<T> reconcileWith(
      MethodHandle h,
      String name,
      byte[] identitySecret,
      GrantContext ctx,
      byte[] record,
      Payload<T> payload) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment rec = record == null ? MemorySegment.NULL : NativeCall.bytes(arena, record);
      MemorySegment out = arena.allocate(GRANT_OUTCOME);
      NativeCall.callChecked(
          h,
          name,
          NativeCall.key(arena, identitySecret, "identity secret"),
          context(arena, ctx),
          rec,
          out);
      try {
        int kind = out.get(I, off(GRANT_OUTCOME, "kind"));
        return switch (kind) {
          case 0 ->
              new GrantOutcome.Active<>(
                  payload.from(
                      out.get(I, off(GRANT_OUTCOME, "access")),
                      NativeCall.getBytes(out, off(GRANT_OUTCOME, "payload"))));
          case 1 ->
              new GrantOutcome.Revoked<>(
                  NativeCall.getString(out, off(GRANT_OUTCOME, "reason")),
                  NativeCall.getString(out, off(GRANT_OUTCOME, "role")));
          case 2 ->
              new GrantOutcome.Unopenable<>(
                  NativeCall.getString(out, off(GRANT_OUTCOME, "reason")));
          case 3 -> new GrantOutcome.Absent<>();
          default -> throw new IllegalStateException(name + ": unknown outcome kind " + kind);
        };
      } finally {
        NativeCall.call(H.OUTCOME_RELEASE, out);
      }
    }
  }

  // ─── helpers ───────────────────────────────────────────────────────────────

  private static MemorySegment context(Arena arena, GrantContext ctx) {
    MemorySegment s = arena.allocate(GRANT_CONTEXT);
    s.set(I, 0, (int) GRANT_CONTEXT.byteSize());
    NativeCall.putString(arena, s, off(GRANT_CONTEXT, "app"), ctx.app());
    NativeCall.putString(arena, s, off(GRANT_CONTEXT, "granter"), ctx.granter());
    NativeCall.putString(arena, s, off(GRANT_CONTEXT, "recipient"), ctx.recipient());
    NativeCall.putBytes(arena, s, off(GRANT_CONTEXT, "resource"), ctx.resource());
    NativeCall.putString(arena, s, off(GRANT_CONTEXT, "path"), ctx.path());
    NativeCall.putString(arena, s, off(GRANT_CONTEXT, "role"), ctx.role());
    return s;
  }
}
