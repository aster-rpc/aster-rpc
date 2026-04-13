package site.aster.hooks;

import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.ffi.IrohStatus;
import site.aster.handle.IrohRuntime;

/**
 * Static helpers for responding to pending hook invocations surfaced by the runtime event pump.
 *
 * <p>Hook events ({@code IrohEventKind.HOOK_BEFORE_CONNECT} / {@code HOOK_AFTER_CONNECT}) carry an
 * invocation handle in {@code event.related()}. The host inspects the event, decides whether to
 * allow or deny, and calls one of these methods to release the invocation. A second call for the
 * same invocation returns {@code NOT_FOUND}.
 *
 * <p>AsterServer is expected to wire the actual subscribe + dispatch loop on top of these
 * primitives — this class only owns the FFI release path.
 */
public final class IrohHook {

  public enum Decision {
    ALLOW(0),
    DENY(1);

    public final int code;

    Decision(int code) {
      this.code = code;
    }
  }

  private IrohHook() {}

  /** Respond to a pending before_connect hook invocation. */
  public static void respondBeforeConnect(IrohRuntime runtime, long invocation, Decision decision) {
    int r =
        IrohLibrary.getInstance()
            .irohHookBeforeConnectRespond(runtime.nativeHandle(), invocation, decision.code);
    if (r != 0) {
      throw new IrohException(IrohStatus.fromCode(r), "iroh_hook_before_connect_respond: " + r);
    }
  }

  /** Respond to a pending after_connect hook invocation. This always accepts. */
  public static void respondAfterConnect(IrohRuntime runtime, long invocation) {
    int r =
        IrohLibrary.getInstance().irohHookAfterConnectRespond(runtime.nativeHandle(), invocation);
    if (r != 0) {
      throw new IrohException(IrohStatus.fromCode(r), "iroh_hook_after_connect_respond: " + r);
    }
  }
}
