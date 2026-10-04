package site.aster.upcall;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;

/** Method handles and async submission for the ABI 4.1 upcall functions ({@code upcall.rs}). */
public final class UpcallNative {

  private UpcallNative() {}

  public static final ValueLayout.OfInt I = ValueLayout.JAVA_INT;
  public static final ValueLayout.OfLong J = ValueLayout.JAVA_LONG;
  public static final java.lang.foreign.AddressLayout A = ValueLayout.ADDRESS;

  private static final ConcurrentHashMap<String, MethodHandle> HANDLES = new ConcurrentHashMap<>();

  public static MethodHandle handle(String name, FunctionDescriptor desc) {
    return HANDLES.computeIfAbsent(name, n -> IrohLibrary.getInstance().getHandle(n, desc));
  }

  /** Throw an {@link IrohException} for a non-zero status, with the C ABI's last error. */
  public static void check(int status, String name) {
    if (status != 0) {
      throw new IrohException(
          IrohStatus.fromCode(status), name + " failed: " + DataPlane.lastError());
    }
  }

  public static RuntimeException rethrow(Throwable t) {
    if (t instanceof RuntimeException r) {
      return r;
    }
    if (t instanceof Error e) {
      throw e;
    }
    return new IrohException("native call threw: " + t);
  }

  /** One native call that writes a {@code uint64_t} out-parameter (a handle). */
  @FunctionalInterface
  public interface OutCall {
    int invoke(Arena arena, MemorySegment out) throws Throwable;
  }

  public static long callForLong(String name, OutCall call) {
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment out = arena.allocate(J);
      int status = call.invoke(arena, out);
      check(status, name);
      return out.get(J, 0);
    } catch (Throwable t) {
      throw rethrow(t);
    }
  }

  /**
   * Invoke an async C function (its last argument is {@code aster_operation_t*}) and return the
   * completion with a copy of its payload. The arena closes when the call returns: every 4.1
   * function copies its inputs first.
   */
  public static CompletableFuture<DataPlane.Result> submit(
      IrohRuntime runtime, String name, OutCall call) {
    long opId;
    try (Arena arena = Arena.ofConfined()) {
      MemorySegment opSeg = arena.allocate(J);
      int status = call.invoke(arena, opSeg);
      if (status != 0) {
        return CompletableFuture.failedFuture(
            new IrohException(
                IrohStatus.fromCode(status), name + " failed: " + DataPlane.lastError()));
      }
      opId = opSeg.get(J, 0);
    } catch (Throwable t) {
      return CompletableFuture.failedFuture(new IrohException(name + " threw: " + t));
    }
    return runtime
        .registry()
        .register(opId)
        .thenApply(
            ev -> {
              byte[] data =
                  ev.dataLen() > 0 && ev.data() != null && ev.data().address() != 0
                      ? ev.data().asSlice(0, ev.dataLen()).toArray(ValueLayout.JAVA_BYTE)
                      : new byte[0];
              if (ev.hasBuffer()) {
                try {
                  runtime.releaseBuffer(ev.buffer());
                } catch (RuntimeException ignored) {
                  // already released or runtime closing; the payload is copied
                }
              }
              return new DataPlane.Result(ev, data);
            });
  }
}
