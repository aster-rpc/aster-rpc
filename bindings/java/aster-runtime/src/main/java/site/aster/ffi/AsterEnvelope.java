package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import site.aster.server.wire.CallHeader;
import site.aster.server.wire.RpcStatus;
import site.aster.server.wire.StreamHeader;

/**
 * AEv1 RPC envelope codec and zstd payload compression, through the C ABI (3.1+).
 *
 * <p>{@link StreamHeader}, {@link CallHeader} and {@link RpcStatus} are encoded by Rust core
 * ({@code aster_transport_core::envelope}, Aster-SPEC §6.9) rather than by Fory, so every binding
 * produces the same bytes. Payload compression follows Aster-SPEC §5.6 ({@code
 * aster_zstd_maybe_compress} / {@code aster_zstd_decompress}).
 *
 * <p>C functions: {@code aster_{stream_header,call_header,rpc_status}_{encode,decode,release}},
 * {@code aster_zstd_maybe_compress}, {@code aster_zstd_decompress}, plus {@code
 * aster_string_release} for every library-owned output buffer. Struct layouts mirror {@code
 * ffi/aster_ffi.h} and are pinned by {@code ffi/src/layout.rs}.
 *
 * <p>Every method is thread-safe; each call uses its own confined {@link Arena}.
 */
public final class AsterEnvelope {

  private AsterEnvelope() {}

  /**
   * Payloads strictly larger than this are compressed on send ({@code
   * ASTER_COMPRESSION_THRESHOLD}).
   */
  public static final int COMPRESSION_THRESHOLD = 4096;

  /** Decompression refuses output larger than this ({@code ASTER_MAX_DECOMPRESSED_SIZE}). */
  public static final int MAX_DECOMPRESSED_SIZE = 16 * 1024 * 1024;

  /** First byte of an encoded StreamHeader. */
  public static final byte TAG_STREAM_HEADER = (byte) 0xA1;

  /** First byte of an encoded CallHeader. */
  public static final byte TAG_CALL_HEADER = (byte) 0xA2;

  /** First byte of an encoded RpcStatus. */
  public static final byte TAG_RPC_STATUS = (byte) 0xA3;

  /** Minimum ABI minor (with major 3) that exports these functions. */
  public static final int REQUIRED_ABI_MINOR = 1;

  // ─── Layouts (ffi/aster_ffi.h) ─────────────────────────────────────────────

  private static final MemoryLayout BYTES = IrohLibrary.ASTER_BYTES;
  private static final MemoryLayout BYTES_LIST = IrohLibrary.ASTER_BYTES_LIST;

  /** {@code aster_stream_header_t}, 96 bytes. */
  public static final MemoryLayout STREAM_HEADER =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("version"), // 4
          ValueLayout.JAVA_INT.withName("call_id"), // 8
          ValueLayout.JAVA_INT.withName("session_id"), // 12
          ValueLayout.JAVA_SHORT.withName("deadline"), // 16
          ValueLayout.JAVA_BYTE.withName("serialization_mode"), // 18
          ValueLayout.JAVA_BYTE.withName("reserved0"), // 19
          ValueLayout.JAVA_INT.withName("reserved1"), // 20
          BYTES.withName("service"), // 24
          BYTES.withName("method"), // 40
          BYTES_LIST.withName("metadata_keys"), // 56
          BYTES_LIST.withName("metadata_values"), // 72
          ValueLayout.ADDRESS.withName("storage") // 88
          );

  /** {@code aster_call_header_t}, 72 bytes. */
  public static final MemoryLayout CALL_HEADER =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("call_id"), // 4
          ValueLayout.JAVA_SHORT.withName("deadline"), // 8
          ValueLayout.JAVA_SHORT.withName("reserved0"), // 10
          ValueLayout.JAVA_INT.withName("reserved1"), // 12
          BYTES.withName("method"), // 16
          BYTES_LIST.withName("metadata_keys"), // 32
          BYTES_LIST.withName("metadata_values"), // 48
          ValueLayout.ADDRESS.withName("storage") // 64
          );

  /** {@code aster_rpc_status_t}, 64 bytes. */
  public static final MemoryLayout RPC_STATUS =
      MemoryLayout.structLayout(
          ValueLayout.JAVA_INT.withName("struct_size"), // 0
          ValueLayout.JAVA_INT.withName("code"), // 4
          BYTES.withName("message"), // 8
          BYTES_LIST.withName("detail_keys"), // 24
          BYTES_LIST.withName("detail_values"), // 40
          ValueLayout.ADDRESS.withName("storage") // 56
          );

  private static long off(MemoryLayout layout, String field) {
    return layout.byteOffset(MemoryLayout.PathElement.groupElement(field));
  }

  // Field offsets, computed from the layouts above (checked in AsterEnvelopeTest).
  private static final long SH_STRUCT_SIZE = off(STREAM_HEADER, "struct_size");
  private static final long SH_VERSION = off(STREAM_HEADER, "version");
  private static final long SH_CALL_ID = off(STREAM_HEADER, "call_id");
  private static final long SH_SESSION_ID = off(STREAM_HEADER, "session_id");
  private static final long SH_DEADLINE = off(STREAM_HEADER, "deadline");
  private static final long SH_MODE = off(STREAM_HEADER, "serialization_mode");
  private static final long SH_SERVICE = off(STREAM_HEADER, "service");
  private static final long SH_METHOD = off(STREAM_HEADER, "method");
  private static final long SH_KEYS = off(STREAM_HEADER, "metadata_keys");
  private static final long SH_VALUES = off(STREAM_HEADER, "metadata_values");

  private static final long CH_STRUCT_SIZE = off(CALL_HEADER, "struct_size");
  private static final long CH_CALL_ID = off(CALL_HEADER, "call_id");
  private static final long CH_DEADLINE = off(CALL_HEADER, "deadline");
  private static final long CH_METHOD = off(CALL_HEADER, "method");
  private static final long CH_KEYS = off(CALL_HEADER, "metadata_keys");
  private static final long CH_VALUES = off(CALL_HEADER, "metadata_values");

  private static final long RS_STRUCT_SIZE = off(RPC_STATUS, "struct_size");
  private static final long RS_CODE = off(RPC_STATUS, "code");
  private static final long RS_MESSAGE = off(RPC_STATUS, "message");
  private static final long RS_KEYS = off(RPC_STATUS, "detail_keys");
  private static final long RS_VALUES = off(RPC_STATUS, "detail_values");

  // ─── Method handles (resolved once) ────────────────────────────────────────

  private static final class Handles {
    static final MethodHandle STREAM_HEADER_ENCODE = encode("aster_stream_header_encode");
    static final MethodHandle STREAM_HEADER_DECODE = decode("aster_stream_header_decode");
    static final MethodHandle STREAM_HEADER_RELEASE = release("aster_stream_header_release");
    static final MethodHandle CALL_HEADER_ENCODE = encode("aster_call_header_encode");
    static final MethodHandle CALL_HEADER_DECODE = decode("aster_call_header_decode");
    static final MethodHandle CALL_HEADER_RELEASE = release("aster_call_header_release");
    static final MethodHandle RPC_STATUS_ENCODE = encode("aster_rpc_status_encode");
    static final MethodHandle RPC_STATUS_DECODE = decode("aster_rpc_status_decode");
    static final MethodHandle RPC_STATUS_RELEASE = release("aster_rpc_status_release");
    static final MethodHandle ZSTD_MAYBE_COMPRESS =
        lib()
            .getHandle(
                "aster_zstd_maybe_compress",
                FunctionDescriptor.of(
                    ValueLayout.JAVA_INT,
                    ValueLayout.ADDRESS,
                    ValueLayout.JAVA_LONG,
                    ValueLayout.JAVA_LONG,
                    ValueLayout.ADDRESS));
    static final MethodHandle ZSTD_DECOMPRESS =
        lib()
            .getHandle(
                "aster_zstd_decompress",
                FunctionDescriptor.of(
                    ValueLayout.JAVA_INT,
                    ValueLayout.ADDRESS,
                    ValueLayout.JAVA_LONG,
                    ValueLayout.ADDRESS));
    static final MethodHandle STRING_RELEASE =
        lib()
            .getHandle(
                "aster_string_release",
                FunctionDescriptor.of(
                    ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG));

    private static IrohLibrary lib() {
      IrohLibrary lib = IrohLibrary.getInstance();
      if (lib.abiVersionMinor() < REQUIRED_ABI_MINOR) {
        throw new UnsatisfiedLinkError(
            "aster native library ABI "
                + lib.abiVersionMajor()
                + "."
                + lib.abiVersionMinor()
                + " lacks the AEv1 envelope functions (need 3."
                + REQUIRED_ABI_MINOR
                + "+)");
      }
      return lib;
    }

    private static MethodHandle encode(String name) {
      return lib()
          .getHandle(
              name,
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
    }

    private static MethodHandle decode(String name) {
      return lib()
          .getHandle(
              name,
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT,
                  ValueLayout.ADDRESS,
                  ValueLayout.JAVA_LONG,
                  ValueLayout.ADDRESS));
    }

    private static MethodHandle release(String name) {
      return lib()
          .getHandle(name, FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS));
    }
  }

  // ─── StreamHeader ──────────────────────────────────────────────────────────

  /** Encode a {@link StreamHeader} as the payload of a {@code HEADER} frame. */
  public static byte[] encodeStreamHeader(StreamHeader h) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment s = arena.allocate(STREAM_HEADER);
      s.set(ValueLayout.JAVA_INT, SH_STRUCT_SIZE, (int) STREAM_HEADER.byteSize());
      s.set(ValueLayout.JAVA_INT, SH_VERSION, h.version());
      s.set(ValueLayout.JAVA_INT, SH_CALL_ID, h.callId());
      s.set(ValueLayout.JAVA_INT, SH_SESSION_ID, h.sessionId());
      s.set(ValueLayout.JAVA_SHORT, SH_DEADLINE, h.deadline());
      s.set(ValueLayout.JAVA_BYTE, SH_MODE, h.serializationMode());
      putString(arena, s, SH_SERVICE, h.service());
      putString(arena, s, SH_METHOD, h.method());
      putList(arena, s, SH_KEYS, h.metadataKeys());
      putList(arena, s, SH_VALUES, h.metadataValues());
      return callEncode(Handles.STREAM_HEADER_ENCODE, arena, s, "aster_stream_header_encode");
    }
  }

  /**
   * Decode the payload of a {@code HEADER} frame.
   *
   * @throws IllegalArgumentException if the bytes are not a valid AEv1 StreamHeader
   */
  public static StreamHeader decodeStreamHeader(byte[] bytes) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(STREAM_HEADER);
      callDecode(Handles.STREAM_HEADER_DECODE, arena, bytes, out, "aster_stream_header_decode");
      try {
        return new StreamHeader(
            getString(out, SH_SERVICE),
            getString(out, SH_METHOD),
            out.get(ValueLayout.JAVA_INT, SH_VERSION),
            out.get(ValueLayout.JAVA_INT, SH_CALL_ID),
            out.get(ValueLayout.JAVA_SHORT, SH_DEADLINE),
            out.get(ValueLayout.JAVA_BYTE, SH_MODE),
            getList(out, SH_KEYS),
            getList(out, SH_VALUES),
            out.get(ValueLayout.JAVA_INT, SH_SESSION_ID));
      } finally {
        callRelease(Handles.STREAM_HEADER_RELEASE, out);
      }
    }
  }

  // ─── CallHeader ────────────────────────────────────────────────────────────

  /** Encode a {@link CallHeader} as the payload of a {@code CALL} frame. */
  public static byte[] encodeCallHeader(CallHeader h) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment s = arena.allocate(CALL_HEADER);
      s.set(ValueLayout.JAVA_INT, CH_STRUCT_SIZE, (int) CALL_HEADER.byteSize());
      s.set(ValueLayout.JAVA_INT, CH_CALL_ID, h.callId());
      s.set(ValueLayout.JAVA_SHORT, CH_DEADLINE, h.deadline());
      putString(arena, s, CH_METHOD, h.method());
      putList(arena, s, CH_KEYS, h.metadataKeys());
      putList(arena, s, CH_VALUES, h.metadataValues());
      return callEncode(Handles.CALL_HEADER_ENCODE, arena, s, "aster_call_header_encode");
    }
  }

  /**
   * Decode the payload of a {@code CALL} frame.
   *
   * @throws IllegalArgumentException if the bytes are not a valid AEv1 CallHeader
   */
  public static CallHeader decodeCallHeader(byte[] bytes) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(CALL_HEADER);
      callDecode(Handles.CALL_HEADER_DECODE, arena, bytes, out, "aster_call_header_decode");
      try {
        return new CallHeader(
            getString(out, CH_METHOD),
            out.get(ValueLayout.JAVA_INT, CH_CALL_ID),
            out.get(ValueLayout.JAVA_SHORT, CH_DEADLINE),
            getList(out, CH_KEYS),
            getList(out, CH_VALUES));
      } finally {
        callRelease(Handles.CALL_HEADER_RELEASE, out);
      }
    }
  }

  // ─── RpcStatus ─────────────────────────────────────────────────────────────

  /** Encode an {@link RpcStatus} as the payload of a {@code TRAILER} frame. */
  public static byte[] encodeRpcStatus(RpcStatus st) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment s = arena.allocate(RPC_STATUS);
      s.set(ValueLayout.JAVA_INT, RS_STRUCT_SIZE, (int) RPC_STATUS.byteSize());
      s.set(ValueLayout.JAVA_INT, RS_CODE, st.code());
      putString(arena, s, RS_MESSAGE, st.message());
      putList(arena, s, RS_KEYS, st.detailKeys());
      putList(arena, s, RS_VALUES, st.detailValues());
      return callEncode(Handles.RPC_STATUS_ENCODE, arena, s, "aster_rpc_status_encode");
    }
  }

  /**
   * Decode the payload of a {@code TRAILER} frame.
   *
   * @throws IllegalArgumentException if the bytes are not a valid AEv1 RpcStatus
   */
  public static RpcStatus decodeRpcStatus(byte[] bytes) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(RPC_STATUS);
      callDecode(Handles.RPC_STATUS_DECODE, arena, bytes, out, "aster_rpc_status_decode");
      try {
        return new RpcStatus(
            out.get(ValueLayout.JAVA_INT, RS_CODE),
            getString(out, RS_MESSAGE),
            getList(out, RS_KEYS),
            getList(out, RS_VALUES));
      } finally {
        callRelease(Handles.RPC_STATUS_RELEASE, out);
      }
    }
  }

  // ─── zstd (Aster-SPEC §5.6) ────────────────────────────────────────────────

  /**
   * Compress {@code payload} if it is longer than {@code threshold} bytes and compression makes it
   * smaller.
   *
   * @return the compressed bytes (send with {@code FLAG_COMPRESSED}), or {@code null} to send
   *     {@code payload} unchanged
   */
  public static byte[] maybeCompress(byte[] payload, int threshold) {
    if (payload.length <= threshold) {
      return null; // same rule as core; skips the FFI hop for small payloads
    }
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment in = copyIn(arena, payload);
      MemorySegment out = arena.allocate(BYTES);
      int rc;
      try {
        rc =
            (int)
                Handles.ZSTD_MAYBE_COMPRESS.invokeExact(
                    in, (long) payload.length, (long) threshold, out);
      } catch (Throwable t) {
        throw new AssertionError(t);
      }
      check(rc, "aster_zstd_maybe_compress");
      MemorySegment ptr = out.get(ValueLayout.ADDRESS, 0);
      if (ptr.equals(MemorySegment.NULL)) {
        return null;
      }
      return takeOwned(ptr, out.get(ValueLayout.JAVA_LONG, 8));
    }
  }

  /**
   * Decompress a {@code FLAG_COMPRESSED} payload (at most {@link #MAX_DECOMPRESSED_SIZE} bytes).
   *
   * @throws IllegalArgumentException for corrupt zstd data or output over the limit
   */
  public static byte[] decompress(byte[] compressed) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment in = copyIn(arena, compressed);
      MemorySegment out = arena.allocate(BYTES);
      int rc;
      try {
        rc = (int) Handles.ZSTD_DECOMPRESS.invokeExact(in, (long) compressed.length, out);
      } catch (Throwable t) {
        throw new AssertionError(t);
      }
      check(rc, "aster_zstd_decompress");
      return takeOwned(out.get(ValueLayout.ADDRESS, 0), out.get(ValueLayout.JAVA_LONG, 8));
    }
  }

  // ─── helpers ───────────────────────────────────────────────────────────────

  private static MemorySegment copyIn(Arena arena, byte[] bytes) {
    if (bytes.length == 0) {
      return MemorySegment.NULL;
    }
    MemorySegment seg = arena.allocate(bytes.length);
    MemorySegment.copy(bytes, 0, seg, ValueLayout.JAVA_BYTE, 0, bytes.length);
    return seg;
  }

  private static void putBytes(MemorySegment at, long offset, MemorySegment ptr, long len) {
    at.set(ValueLayout.ADDRESS, offset, ptr);
    at.set(ValueLayout.JAVA_LONG, offset + 8, len);
  }

  private static void putString(Arena arena, MemorySegment s, long offset, String value) {
    byte[] utf8 = (value == null ? "" : value).getBytes(StandardCharsets.UTF_8);
    putBytes(s, offset, copyIn(arena, utf8), utf8.length);
  }

  private static void putList(Arena arena, MemorySegment s, long offset, List<String> values) {
    int n = values == null ? 0 : values.size();
    if (n == 0) {
      putBytes(s, offset, MemorySegment.NULL, 0);
      return;
    }
    MemorySegment items = arena.allocate(BYTES, n);
    for (int i = 0; i < n; i++) {
      putString(arena, items, i * BYTES.byteSize(), values.get(i));
    }
    putBytes(s, offset, items, n);
  }

  private static String getString(MemorySegment s, long offset) {
    long len = s.get(ValueLayout.JAVA_LONG, offset + 8);
    if (len == 0) {
      return "";
    }
    MemorySegment ptr = s.get(ValueLayout.ADDRESS, offset).reinterpret(len);
    return new String(ptr.toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8);
  }

  private static List<String> getList(MemorySegment s, long offset) {
    long n = s.get(ValueLayout.JAVA_LONG, offset + 8);
    if (n == 0) {
      return List.of();
    }
    MemorySegment items = s.get(ValueLayout.ADDRESS, offset).reinterpret(n * BYTES.byteSize());
    List<String> out = new ArrayList<>((int) n);
    for (long i = 0; i < n; i++) {
      out.add(getString(items, i * BYTES.byteSize()));
    }
    return out;
  }

  private static byte[] callEncode(MethodHandle h, Arena arena, MemorySegment struct, String name) {
    MemorySegment out = arena.allocate(BYTES);
    int rc;
    try {
      rc = (int) h.invokeExact(struct, out);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
    check(rc, name);
    return takeOwned(out.get(ValueLayout.ADDRESS, 0), out.get(ValueLayout.JAVA_LONG, 8));
  }

  private static void callDecode(
      MethodHandle h, Arena arena, byte[] bytes, MemorySegment out, String name) {
    MemorySegment in = copyIn(arena, bytes == null ? new byte[0] : bytes);
    int rc;
    try {
      rc = (int) h.invokeExact(in, (long) (bytes == null ? 0 : bytes.length), out);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
    check(rc, name);
  }

  private static void callRelease(MethodHandle h, MemorySegment struct) {
    try {
      int ignored = (int) h.invokeExact(struct);
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /** Copy a library-owned buffer into the heap and release it with {@code aster_string_release}. */
  private static byte[] takeOwned(MemorySegment ptr, long len) {
    if (len == 0 || ptr.equals(MemorySegment.NULL)) {
      return new byte[0];
    }
    try {
      return ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE);
    } finally {
      try {
        int ignored = (int) Handles.STRING_RELEASE.invokeExact(ptr, len);
      } catch (Throwable t) {
        throw new AssertionError(t);
      }
    }
  }

  private static void check(int rc, String name) {
    if (rc == 0) {
      return;
    }
    String detail = lastError();
    String msg =
        name + " failed: " + IrohStatus.fromCode(rc) + (detail.isEmpty() ? "" : ": " + detail);
    if (rc == IrohStatus.INVALID_ARGUMENT.code) {
      throw new IllegalArgumentException(msg);
    }
    throw IrohException.forStatus(IrohStatus.fromCode(rc), msg);
  }

  private static String lastError() {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment buf = arena.allocate(1024);
      long n = IrohLibrary.getInstance().lastErrorMessage(buf, 1023);
      if (n <= 0) {
        return "";
      }
      return new String(
          buf.asSlice(0, Math.min(n, 1023)).toArray(ValueLayout.JAVA_BYTE), StandardCharsets.UTF_8);
    }
  }
}
