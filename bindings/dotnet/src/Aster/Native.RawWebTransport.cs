using System.Runtime.InteropServices;
namespace Aster;

[StructLayout(LayoutKind.Sequential)]
internal struct RawWTConfigNative
{
    public uint struct_size, flags;
    public Bytes bind_addr, certificate_pem, private_key_pem, path;
    public uint max_sessions, max_streams, max_datagrams, max_datagram_bytes, max_read_bytes, max_pending_auth, max_auth_payload_bytes, reserved;
}
internal static partial class Native
{
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_listener_start(ulong runtime, ulong upcalls, in RawWTConfigNative config, out ulong listener);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_listener_start_public(ulong runtime, in RawWTConfigNative config, out ulong listener);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_listener_addr(ulong runtime, ulong listener, out Bytes address);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_listener_stop(ulong runtime, ulong listener, uint graceMilliseconds);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_listener_accept(ulong runtime, ulong listener, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_close(ulong runtime, ulong session, uint code, Bytes reason);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_free(ulong runtime, ulong session);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_free(ulong runtime, ulong stream);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_send_datagram(ulong runtime, ulong session, Bytes data);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_max_datagram_size(ulong runtime, ulong session, out ulong size, out uint available);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_read(ulong runtime, ulong stream, uint maxBytes, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_write(ulong runtime, ulong stream, Bytes data, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_set_priority(ulong runtime, ulong stream, int priority, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_open_bi(ulong runtime, ulong session, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_open_uni(ulong runtime, ulong session, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_accept_bi(ulong runtime, ulong session, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_accept_uni(ulong runtime, ulong session, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_closed(ulong runtime, ulong session, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_receive_datagram(ulong runtime, ulong session, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_info(ulong runtime, ulong session, out Bytes snapshot);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_session_stats(ulong runtime, ulong session, out Bytes snapshot);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_finish(ulong runtime, ulong stream, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_priority(ulong runtime, ulong stream, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_transport_acked(ulong runtime, ulong stream, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_reset(ulong runtime, ulong stream, uint code, ulong userData, out ulong operation);
    [LibraryImport(NativeLib)]
    internal static partial int aster_raw_wt_stream_stop(ulong runtime, ulong stream, uint code, ulong userData, out ulong operation);
}
