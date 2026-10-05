using System;
using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations for C ABI 4.1 (upcalls, lease, fenced resource, topology),
// ffi/RENAME_MAP.md "New in 4.1". Struct sizes are pinned by
// AbiContractTests. Placement (upcall kinds 17/32/33) is not in the C ABI and is not declared.
internal static unsafe partial class Native
{
    // ─── 4.1: upcall table ──────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_upcalls_new")]
    public static partial int aster_upcalls_new(ulong runtime, uint capacity, out ulong out_upcalls);
    [LibraryImport(NativeLib, EntryPoint = "aster_upcalls_close")]
    public static partial int aster_upcalls_close(ulong runtime, ulong upcalls);
    [LibraryImport(NativeLib, EntryPoint = "aster_upcalls_free")]
    public static partial int aster_upcalls_free(ulong runtime, ulong upcalls);
    [LibraryImport(NativeLib, EntryPoint = "aster_upcall_set_policy")]
    public static partial int aster_upcall_set_policy(ulong runtime, ulong upcalls, uint kind, uint timeout_ms, uint failure_mode);
    [LibraryImport(NativeLib, EntryPoint = "aster_upcall_set_default_policy")]
    public static partial int aster_upcall_set_default_policy(ulong runtime, ulong upcalls, uint timeout_ms, uint failure_mode);
    [LibraryImport(NativeLib, EntryPoint = "aster_upcall_poll")]
    public static partial int aster_upcall_poll(ulong runtime, ulong upcalls, UpcallEventNative* @out, uint max, uint timeout_ms, uint* out_count);
    [LibraryImport(NativeLib, EntryPoint = "aster_upcall_buffer_release")]
    public static partial int aster_upcall_buffer_release(ulong runtime, ulong upcalls, ulong buffer);
    [LibraryImport(NativeLib, EntryPoint = "aster_upcall_respond")]
    public static partial int aster_upcall_respond(ulong runtime, ulong upcalls, ulong invocation_id, uint status, byte* payload, uint payload_len);

    // ─── 4.1: leases ────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_lease_serializer_foreign_new")]
    public static partial int aster_lease_serializer_foreign_new(ulong runtime, ulong upcalls, out ulong out_serializer);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_serializer_memory_new")]
    public static partial int aster_lease_serializer_memory_new(ulong runtime, ulong authorizer_upcalls, out ulong out_serializer);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_serializer_free")]
    public static partial int aster_lease_serializer_free(ulong runtime, ulong serializer);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_acquire")]
    public static partial int aster_lease_acquire(ulong runtime, ulong serializer, Bytes resource, Bytes holder,
        ulong ttl_ms, ulong renew_interval_ms, ulong holder_margin_ms, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_fence")]
    public static partial int aster_lease_fence(ulong runtime, ulong lease, out ulong out_epoch);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_status")]
    public static partial int aster_lease_status(ulong runtime, ulong lease, out uint out_status);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_watch")]
    public static partial int aster_lease_watch(ulong runtime, ulong lease, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_release")]
    public static partial int aster_lease_release(ulong runtime, ulong lease, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_free")]
    public static partial int aster_lease_free(ulong runtime, ulong lease);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_snapshot")]
    public static partial int aster_lease_snapshot(ulong runtime, ulong serializer, Bytes resource, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_lease_revoke")]
    public static partial int aster_lease_revoke(ulong runtime, ulong serializer, Bytes resource, Bytes authority, ulong user_data, out ulong out_operation);

    // ─── 4.1: fenced resource ───────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_fenced_resource_foreign_new")]
    public static partial int aster_fenced_resource_foreign_new(ulong runtime, ulong upcalls, out ulong out_resource);
    [LibraryImport(NativeLib, EntryPoint = "aster_fenced_resource_free")]
    public static partial int aster_fenced_resource_free(ulong runtime, ulong resource);
    [LibraryImport(NativeLib, EntryPoint = "aster_fenced_mutate")]
    public static partial int aster_fenced_mutate(ulong runtime, ulong resource, Bytes fence_resource, ulong epoch,
        Bytes fence_holder, Bytes writer, Bytes op, ulong user_data, out ulong out_operation);

    // ─── 4.1: topology admitted filter ──────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_topology_admitted_new")]
    public static partial int aster_topology_admitted_new(ulong runtime, ulong upcalls, uint ttl_ms, uint failure_retry_ms,
        uint max_batch, out ulong out_admitted);
    [LibraryImport(NativeLib, EntryPoint = "aster_topology_admitted_free")]
    public static partial int aster_topology_admitted_free(ulong runtime, ulong admitted);
    [LibraryImport(NativeLib, EntryPoint = "aster_topology_admitted_check")]
    public static partial int aster_topology_admitted_check(ulong runtime, ulong admitted, Bytes node_id, out uint out_admitted);
    [LibraryImport(NativeLib, EntryPoint = "aster_topology_admitted_set")]
    public static partial int aster_topology_admitted_set(ulong runtime, ulong admitted, Bytes node_id, uint is_admitted);
    [LibraryImport(NativeLib, EntryPoint = "aster_topology_admitted_invalidate")]
    public static partial int aster_topology_admitted_invalidate(ulong runtime, ulong admitted, Bytes node_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_topology_admitted_prefetch")]
    public static partial int aster_topology_admitted_prefetch(ulong runtime, ulong admitted, BytesList node_ids, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_topology_admitted_stats")]
    public static partial int aster_topology_admitted_stats(ulong runtime, ulong admitted, TopologyAdmittedStatsNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_topology_enable_shared")]
    public static partial int aster_topology_enable_shared(ulong runtime, ulong node, byte* secret, ulong admitted,
        BytesList bootstrap, uint position_refresh_ms, ulong user_data, out ulong out_operation);
}

/// <summary><c>aster_upcall_event_t</c> (40 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct UpcallEventNative
{
    public ulong invocation_id;
    public ushort kind;
    public byte cancel_reason;
    public byte _p0;
    public uint payload_len;
    public ulong deadline_unix_ms;
    public IntPtr payload_ptr;
    public ulong buffer;
}

/// <summary><c>aster_topology_admitted_stats_t</c> (32 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct TopologyAdmittedStatsNative
{
    public ulong hits;
    public ulong stale;
    public ulong misses;
    public ulong upcalls;
}
