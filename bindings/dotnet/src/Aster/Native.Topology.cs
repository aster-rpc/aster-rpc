using System.Runtime.InteropServices;
namespace Aster;

internal static unsafe partial class Native
{
    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_has_monitoring(ulong runtime, ulong node, out uint enabled);

    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_peers(ulong runtime, ulong node, out Bytes output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_peer(ulong runtime, ulong node, Bytes nodeId, out Bytes output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_snapshot(ulong runtime, ulong node, ulong userData, out ulong operation);

    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_snapshot_free(ulong runtime, ulong snapshot);

    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_snapshot_clusters(ulong runtime, ulong snapshot, out Bytes output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_snapshot_cluster_of(ulong runtime, ulong snapshot, Bytes nodeId, out Bytes output);

    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_snapshot_separated(ulong runtime, ulong snapshot, Bytes a, Bytes b, out uint verdict);

    [LibraryImport(NativeLib)]
    internal static partial int aster_topology_enable_shared_with_options(ulong runtime, ulong node, byte* secret, ulong admitted, BytesList bootstrap, Bytes options, ulong userData, out ulong operation);

    [LibraryImport(NativeLib)]
    internal static partial int aster_server_set_topology(ulong builder, uint enabled, Bytes requirement);
}
