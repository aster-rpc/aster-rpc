package site.aster.lease;

import java.lang.foreign.FunctionDescriptor;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.atomic.AtomicBoolean;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohLibrary;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.Ucv1;
import site.aster.upcall.UpcallNative;
import site.aster.upcall.Upcalls;

/**
 * A Rust {@code FencedResource} implemented by a Java {@link FencedResource} served on an {@link
 * Upcalls} table ({@code aster_fenced_resource_t}, C ABI 4.1).
 */
public final class NativeFencedResource implements AutoCloseable {

  private static final MethodHandle NEW =
      UpcallNative.handle(
          "aster_fenced_resource_foreign_new",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J, UpcallNative.A));
  private static final MethodHandle FREE =
      UpcallNative.handle(
          "aster_fenced_resource_free",
          FunctionDescriptor.of(UpcallNative.I, UpcallNative.J, UpcallNative.J));
  private static final MethodHandle MUTATE =
      UpcallNative.handle(
          "aster_fenced_mutate",
          FunctionDescriptor.of(
              UpcallNative.I,
              UpcallNative.J,
              UpcallNative.J,
              IrohLibrary.ASTER_BYTES,
              UpcallNative.J,
              IrohLibrary.ASTER_BYTES,
              IrohLibrary.ASTER_BYTES,
              IrohLibrary.ASTER_BYTES,
              UpcallNative.J,
              UpcallNative.A));

  private final IrohRuntime runtime;
  private final long handle;
  private final AtomicBoolean closed = new AtomicBoolean();

  private NativeFencedResource(IrohRuntime runtime, long handle) {
    this.runtime = runtime;
    this.handle = handle;
  }

  public static NativeFencedResource foreign(Upcalls upcalls) {
    IrohRuntime rt = upcalls.runtime();
    long h =
        UpcallNative.callForLong(
            "aster_fenced_resource_foreign_new",
            (a, out) -> (int) NEW.invoke(rt.nativeHandle(), upcalls.nativeHandle(), out));
    return new NativeFencedResource(rt, h);
  }

  /**
   * Apply {@code op} under {@code fence} as {@code writer}. Completes with the applied output or
   * the fence rejection; completes exceptionally when the outcome is unknown (timeout, handler
   * error, closed table).
   */
  public CompletableFuture<MutateResult> mutate(Fence fence, String writer, byte[] op) {
    return UpcallNative.submit(
            runtime,
            "aster_fenced_mutate",
            (a, out) ->
                (int)
                    MUTATE.invoke(
                        runtime.nativeHandle(),
                        handle,
                        DataPlane.utf8(a, fence.resource()),
                        fence.epoch(),
                        DataPlane.utf8(a, fence.holder()),
                        DataPlane.utf8(a, writer),
                        DataPlane.bytes(a, op),
                        0L,
                        out))
        .thenApply(r -> Ucv1.decodeFencedMutateResponse(r.data()));
  }

  @Override
  public void close() {
    if (closed.compareAndSet(false, true)) {
      try {
        FREE.invoke(runtime.nativeHandle(), handle);
      } catch (Throwable ignored) {
        // runtime closed
      }
    }
  }
}
