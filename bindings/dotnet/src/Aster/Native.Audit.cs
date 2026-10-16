using System.Runtime.InteropServices;
namespace Aster;
internal static partial class Native
{
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_open(ulong rt, Bytes json, ulong userData, out ulong op);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_submit(ulong rt, ulong h, Bytes json, ulong userData, out ulong op);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_status(ulong rt, ulong h, ulong userData, out ulong op);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_flush(ulong rt, ulong h, ulong timeoutMs, ulong userData, out ulong op);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_close(ulong rt, ulong h, ulong userData, out ulong op);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_reconfigure(ulong rt, ulong h, Bytes json, ulong userData, out ulong op);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_quarantine(ulong rt, ulong h, Bytes json, ulong userData, out ulong op);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_export(ulong rt, ulong h, Bytes json, ulong userData, out ulong op);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_replay(ulong rt, ulong h, Bytes json, ulong userData, out ulong op);
    /// <summary>ABI 7.1: read the acknowledgement log (JSON v1 <c>{version, after, limit, wait_ms}</c>).</summary>
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_acknowledgements(ulong rt, ulong h, Bytes json, ulong userData, out ulong op);
    /// <summary>Mirrors <c>ASTER_AUDIT_OUTBOX_MAX_ACK_WAIT_MS</c> (one hour).</summary>
    internal const ulong AuditOutboxMaxAckWaitMs = 3_600_000;
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_clone(ulong rt, ulong h, out ulong clone);
    [LibraryImport(NativeLib)] internal static partial int aster_audit_outbox_free(ulong rt, ulong h);
}
