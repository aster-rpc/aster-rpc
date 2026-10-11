using System.Runtime.InteropServices;

namespace Aster;

internal static unsafe partial class Native
{
    [LibraryImport(NativeLib)]
    internal static partial int aster_blobs_custody_new(ulong runtime, ulong node, out ulong scope);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blob_custody_free(ulong runtime, ulong scope);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blob_custody_protect(ulong runtime, ulong scope, Bytes hash, uint format, ulong userData, out ulong op);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blob_custody_add_bytes(ulong runtime, ulong scope, Bytes data, uint format, ulong userData, out ulong op);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blob_custody_add_path(ulong runtime, ulong scope, Bytes path, uint format, ulong userData, out ulong op);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blob_custody_retain(ulong runtime, ulong scope, Bytes hash, uint format, Bytes tag, ulong userData, out ulong op);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blob_custody_read(ulong runtime, ulong scope, Bytes hash, ulong userData, out ulong op);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blob_custody_download_hash_from(ulong runtime, ulong scope, Bytes hash, BytesList providers, uint format, uint strategy, uint flags, ulong userData, out ulong op);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blobs_add_bytes_verified(ulong runtime, ulong node, Bytes data, Bytes expectedHash, uint format, Bytes tag, ulong userData, out ulong op);
    [LibraryImport(NativeLib)]
    internal static partial int aster_blobs_add_path_verified(ulong runtime, ulong node, Bytes path, Bytes expectedHash, uint format, Bytes tag, ulong userData, out ulong op);
}
