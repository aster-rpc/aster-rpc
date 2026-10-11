using System.Runtime.InteropServices;

namespace Aster;

internal static partial class Native
{
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_event_receiver_free")]
    public static partial int aster_doc_event_receiver_free(ulong runtime, ulong receiver);

    [LibraryImport(NativeLib, EntryPoint = "aster_docs_join_and_subscribe")]
    public static partial int aster_docs_join_and_subscribe(ulong runtime, ulong node, Bytes ticket, ulong user_data, out ulong operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_del")]
    public static partial int aster_doc_del(ulong runtime, ulong doc, Bytes author, Bytes prefix, ulong user_data, out ulong operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_share_with_addr")]
    public static partial int aster_doc_share_with_addr(ulong runtime, ulong doc, uint mode, ulong user_data, out ulong operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_read_entry_content")]
    public static partial int aster_doc_read_entry_content(ulong runtime, ulong doc, Bytes hash, ulong user_data, out ulong operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_add_path")]
    public static partial int aster_blobs_add_path(ulong runtime, ulong node, Bytes path, ulong user_data, out ulong operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_add_path_with_named_tag")]
    public static partial int aster_blobs_add_path_with_named_tag(ulong runtime, ulong node, Bytes path, Bytes tag, ulong user_data, out ulong operation);
}
