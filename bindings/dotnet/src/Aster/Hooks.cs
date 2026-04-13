namespace Aster;

/// <summary>
/// Hook decision for before_connect callbacks. Mirrors iroh_hook_decision_t.
/// </summary>
public enum HookDecision
{
    Allow = 0,
    Deny = 1,
}

/// <summary>
/// Static helpers for responding to pending hook invocations surfaced by the
/// runtime event pump. Hook events (kinds 70/71) carry an invocation handle
/// in <see cref="Event.related"/>; the host inspects the event, decides
/// whether to allow or deny, and calls one of these methods to release the
/// invocation. A second call for the same invocation returns NOT_FOUND.
///
/// AsterServer is expected to wire the actual subscribe + dispatch loop on
/// top of these primitives — this class only owns the FFI release path.
/// </summary>
public static class Hooks
{
    /// <summary>Respond to a pending before_connect hook invocation.</summary>
    public static void RespondBeforeConnect(Runtime runtime, ulong invocation, HookDecision decision)
    {
        int r = Native.iroh_hook_before_connect_respond(runtime.Handle, invocation, (int)decision);
        if (r != 0) throw IrohException.FromStatus(r, "iroh_hook_before_connect_respond");
    }

    /// <summary>Respond to a pending after_connect hook invocation. This always accepts.</summary>
    public static void RespondAfterConnect(Runtime runtime, ulong invocation)
    {
        int r = Native.iroh_hook_after_connect_respond(runtime.Handle, invocation);
        if (r != 0) throw IrohException.FromStatus(r, "iroh_hook_after_connect_respond");
    }
}
