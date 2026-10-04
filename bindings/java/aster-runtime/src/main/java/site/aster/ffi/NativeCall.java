package site.aster.ffi;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import site.aster.exception.VerificationFailedException;

/**
 * Marshalling helpers for the synchronous, pure C ABI functions (ABI 3.4: ids, HPKE, attestation,
 * sealed grants). Used by {@code site.aster.id}, {@code site.aster.crypto}, {@code
 * site.aster.attestation} and {@code site.aster.grants}; not meant for application code.
 *
 * <p>Conventions (see {@code ffi/RENAME_MAP.md} §"New in 3.4"): raw keys are 32-byte buffers,
 * variable inputs are {@code aster_bytes_t} by value, variable outputs are library-owned {@code
 * aster_bytes_t} released with {@code aster_string_release}. Status {@code INVALID_ARGUMENT} maps
 * to {@link IllegalArgumentException}, {@code VERIFICATION_FAILED} to {@link
 * VerificationFailedException}, anything else to {@link IrohException#forStatus}.
 */
public final class NativeCall {

  private NativeCall() {}

  /** Size of every raw key / id. */
  public static final int KEY_SIZE = 32;

  /** {@code aster_bytes_t}. */
  public static final MemoryLayout BYTES = IrohLibrary.ASTER_BYTES;

  private static final MethodHandle STRING_RELEASE =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_string_release",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG));

  /** Resolve a pure function (every ABI 4.x library exports them; the major is checked at load). */
  public static MethodHandle handle(String name, FunctionDescriptor desc) {
    return IrohLibrary.getInstance().getHandle(name, desc);
  }

  /** {@code int f(args...)}. */
  public static FunctionDescriptor intFn(MemoryLayout... args) {
    return FunctionDescriptor.of(ValueLayout.JAVA_INT, args);
  }

  /** Copy a 32-byte key into native memory. */
  public static MemorySegment key(Arena arena, byte[] key, String what) {
    if (key == null || key.length != KEY_SIZE) {
      throw new IllegalArgumentException(
          what + " must be " + KEY_SIZE + " bytes, got " + (key == null ? "null" : key.length));
    }
    MemorySegment seg = arena.allocate(KEY_SIZE);
    MemorySegment.copy(key, 0, seg, ValueLayout.JAVA_BYTE, 0, KEY_SIZE);
    return seg;
  }

  /** A writable 32-byte output buffer. */
  public static MemorySegment keyOut(Arena arena) {
    return arena.allocate(KEY_SIZE);
  }

  /** Read 32 bytes. */
  public static byte[] readKey(MemorySegment seg) {
    return seg.asSlice(0, KEY_SIZE).toArray(ValueLayout.JAVA_BYTE);
  }

  /** An {@code aster_bytes_t} struct (for by-value arguments) over a copy of {@code bytes}. */
  public static MemorySegment bytes(Arena arena, byte[] bytes) {
    MemorySegment s = arena.allocate(BYTES);
    putBytes(arena, s, 0, bytes);
    return s;
  }

  /** An {@code aster_bytes_t} over the UTF-8 of {@code value} ({@code null} = empty). */
  public static MemorySegment string(Arena arena, String value) {
    return bytes(arena, utf8(value));
  }

  /** Write an {@code aster_bytes_t} over a copy of {@code bytes} at {@code offset} of {@code s}. */
  public static void putBytes(Arena arena, MemorySegment s, long offset, byte[] bytes) {
    int n = bytes == null ? 0 : bytes.length;
    MemorySegment ptr = MemorySegment.NULL;
    if (n > 0) {
      ptr = arena.allocate(n);
      MemorySegment.copy(bytes, 0, ptr, ValueLayout.JAVA_BYTE, 0, n);
    }
    s.set(ValueLayout.ADDRESS, offset, ptr);
    s.set(ValueLayout.JAVA_LONG, offset + 8, n);
  }

  /** Write an {@code aster_bytes_t} over the UTF-8 of {@code value}. */
  public static void putString(Arena arena, MemorySegment s, long offset, String value) {
    putBytes(arena, s, offset, utf8(value));
  }

  /** Copy the borrowed {@code aster_bytes_t} at {@code offset} of {@code s} (not released). */
  public static byte[] getBytes(MemorySegment s, long offset) {
    long len = s.get(ValueLayout.JAVA_LONG, offset + 8);
    if (len == 0) {
      return new byte[0];
    }
    return s.get(ValueLayout.ADDRESS, offset).reinterpret(len).toArray(ValueLayout.JAVA_BYTE);
  }

  /** {@link #getBytes} decoded as UTF-8. */
  public static String getString(MemorySegment s, long offset) {
    return new String(getBytes(s, offset), StandardCharsets.UTF_8);
  }

  /** Copy a library-owned {@code aster_bytes_t} out-parameter and release it. */
  public static byte[] takeOwned(MemorySegment out) {
    MemorySegment ptr = out.get(ValueLayout.ADDRESS, 0);
    long len = out.get(ValueLayout.JAVA_LONG, 8);
    if (len == 0 || ptr.equals(MemorySegment.NULL)) {
      return new byte[0];
    }
    try {
      return ptr.reinterpret(len).toArray(ValueLayout.JAVA_BYTE);
    } finally {
      try {
        int ignored = (int) STRING_RELEASE.invokeExact(ptr, len);
      } catch (Throwable t) {
        throw new AssertionError(t);
      }
    }
  }

  /** Invoke a handle that returns a status code. */
  public static int call(MethodHandle h, Object... args) {
    try {
      return (int) h.invokeWithArguments(args);
    } catch (RuntimeException | Error e) {
      throw e;
    } catch (Throwable t) {
      throw new AssertionError(t);
    }
  }

  /** {@link #call} then {@link #check}. */
  public static void callChecked(MethodHandle h, String name, Object... args) {
    check(call(h, args), name);
  }

  /** Throw for a non-OK status, with {@code aster_last_error_message} attached. */
  public static void check(int rc, String name) {
    if (rc == IrohStatus.OK.code) {
      return;
    }
    IrohStatus status = IrohStatus.fromCode(rc);
    String detail = lastError();
    String msg = name + " failed: " + status + (detail.isEmpty() ? "" : ": " + detail);
    if (rc == IrohStatus.INVALID_ARGUMENT.code) {
      throw new IllegalArgumentException(msg);
    }
    throw IrohException.forStatus(status, msg);
  }

  private static byte[] utf8(String value) {
    return (value == null ? "" : value).getBytes(StandardCharsets.UTF_8);
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
