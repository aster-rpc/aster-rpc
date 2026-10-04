using System;
using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations for C ABI 4.4 (coordination client + placement manager),
// ffi/RENAME_MAP.md "New in 4.4"; design docs/_internal/bindings/coordination-abi-design.md.
internal static unsafe partial class Native
{
    public const uint CoordinationClientReadOnly = 1;

    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_client_options_init")]
    public static partial int aster_coordination_client_options_init(CoordinationClientOptionsNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_client_open")]
    public static partial int aster_coordination_client_open(ulong runtime, ulong node, CoordinationClientOptionsNative* options, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_client_close")]
    public static partial int aster_coordination_client_close(ulong runtime, ulong client);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_client_free")]
    public static partial int aster_coordination_client_free(ulong runtime, ulong client);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_client_ids")]
    public static partial int aster_coordination_client_ids(ulong runtime, ulong client, byte* out_group_id, byte* out_local_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_client_id")]
    public static partial int aster_coordination_client_id(ulong runtime, ulong client, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_mutate")]
    public static partial int aster_coordination_mutate(ulong runtime, ulong client, Bytes request, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_unacknowledged")]
    public static partial int aster_coordination_unacknowledged(ulong runtime, ulong client, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_resume")]
    public static partial int aster_coordination_resume(ulong runtime, ulong client, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_resolved_acknowledge")]
    public static partial int aster_coordination_resolved_acknowledge(ulong runtime, ulong resolved, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_resolved_free")]
    public static partial int aster_coordination_resolved_free(ulong runtime, ulong resolved);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_retire_other_client")]
    public static partial int aster_coordination_retire_other_client(ulong runtime, ulong client, byte* target_client_id, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_read")]
    public static partial int aster_coordination_read(ulong runtime, ulong client, Bytes request, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_recovery_export_to_file")]
    public static partial int aster_coordination_recovery_export_to_file(ulong runtime, ulong client, Bytes path, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_lease_acquire")]
    public static partial int aster_coordination_lease_acquire(ulong runtime, ulong client, Bytes namespace_, Bytes key,
        ulong ttl_ms, ulong renew_interval_ms, ulong holder_margin_ms, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_lease_fence")]
    public static partial int aster_coordination_lease_fence(ulong runtime, ulong lease, byte* buf, UIntPtr cap, UIntPtr* out_len);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_lease_status")]
    public static partial int aster_coordination_lease_status(ulong runtime, ulong lease, out uint out_status);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_lease_compare_and_set")]
    public static partial int aster_coordination_lease_compare_and_set(ulong runtime, ulong lease, ulong expected_value_revision, Bytes new_value, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_lease_release")]
    public static partial int aster_coordination_lease_release(ulong runtime, ulong lease, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_lease_release_with_value")]
    public static partial int aster_coordination_lease_release_with_value(ulong runtime, ulong lease, ulong expected_value_revision, Bytes final_value, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_lease_free")]
    public static partial int aster_coordination_lease_free(ulong runtime, ulong lease);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_placement_manager_new")]
    public static partial int aster_coordination_placement_manager_new(ulong runtime, ulong client, ulong upcalls, ulong topology_node, out ulong out_manager);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_placement_reconcile")]
    public static partial int aster_coordination_placement_reconcile(ulong runtime, ulong manager, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_coordination_placement_manager_free")]
    public static partial int aster_coordination_placement_manager_free(ulong runtime, ulong manager);
}

/// <summary><c>aster_coordination_client_options_t</c> (48 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct CoordinationClientOptionsNative
{
    public uint struct_size;
    public uint flags;
    public IntPtr group_id;
    public BytesList seed_tickets;
    public Bytes journal_dir;
}
