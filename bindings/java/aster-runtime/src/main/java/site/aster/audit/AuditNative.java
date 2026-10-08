package site.aster.audit;

import java.lang.foreign.FunctionDescriptor;
import java.lang.invoke.MethodHandle;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.handle.IrohRuntime;
import site.aster.upcall.UpcallNative;

/** Method handles only: all storage and delivery policy remains in Rust. */
final class AuditNative {
  private AuditNative() {}

  private static final java.lang.foreign.ValueLayout.OfInt I = UpcallNative.I;
  private static final java.lang.foreign.ValueLayout.OfLong J = UpcallNative.J;
  private static final java.lang.foreign.AddressLayout A = UpcallNative.A;
  private static final java.lang.foreign.MemoryLayout B = IrohLibrary.ASTER_BYTES;

  static final MethodHandle OPEN = h("open", FunctionDescriptor.of(I, J, B, J, A));
  static final MethodHandle SUBMIT = json("submit");
  static final MethodHandle RECONFIGURE = json("reconfigure");
  static final MethodHandle QUARANTINE = json("quarantine");
  static final MethodHandle EXPORT = json("export");
  static final MethodHandle REPLAY = json("replay");
  static final MethodHandle STATUS = h("status", FunctionDescriptor.of(I, J, J, J, A));
  static final MethodHandle CLOSE = h("close", FunctionDescriptor.of(I, J, J, J, A));
  static final MethodHandle FLUSH = h("flush", FunctionDescriptor.of(I, J, J, J, J, A));
  static final MethodHandle CLONE = h("clone", FunctionDescriptor.of(I, J, J, A));
  static final MethodHandle FREE = h("free", FunctionDescriptor.of(I, J, J));

  private static MethodHandle h(String suffix, FunctionDescriptor descriptor) {
    return UpcallNative.handle("aster_audit_outbox_" + suffix, descriptor);
  }

  private static MethodHandle json(String suffix) {
    return h(suffix, FunctionDescriptor.of(I, J, J, B, J, A));
  }

  static CompletableFuture<DataPlane.Result> submit(
      IrohRuntime runtime, String name, UpcallNative.OutCall call) {
    return UpcallNative.submit(runtime, "aster_audit_outbox_" + name, call)
        .handle(
            (result, failure) -> {
              if (failure == null) return result;
              Throwable cause =
                  failure instanceof CompletionException ? failure.getCause() : failure;
              if (cause instanceof IrohException nativeError)
                throw AuditOutboxException.from(nativeError);
              throw new CompletionException(cause);
            });
  }

  static void free(IrohRuntime runtime, long handle) {
    try {
      FREE.invoke(runtime.nativeHandle(), handle);
    } catch (Throwable ignored) {
      /* Runtime shutdown already owns remaining handles. */
    }
  }
}
