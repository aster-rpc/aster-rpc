using System.Runtime.InteropServices;

namespace Aster;

[StructLayout(LayoutKind.Sequential)]
internal struct HttpServerConfigNative
{
    public uint struct_size;
    public uint flags;
    public Bytes bind_addr;
    public Bytes cert_pem;
    public Bytes key_pem;
    public uint max_pending_auth;
    public uint max_auth_payload_bytes;
}

internal static unsafe partial class Native
{
    internal const uint HttpServerWebTransport = 1;

    [LibraryImport(NativeLib)]
    internal static partial int aster_server_http_start(ulong runtime, ulong server, ulong upcalls,
        in HttpServerConfigNative config, out ulong listener);

    [LibraryImport(NativeLib)]
    internal static partial int aster_server_http_start_public(ulong runtime, ulong server,
        in HttpServerConfigNative config, out ulong listener);

    [LibraryImport(NativeLib)]
    internal static partial int aster_http_listener_tcp_addr(ulong runtime, ulong listener,
        out ulong buffer, out IntPtr ptr, out uint len);

    [LibraryImport(NativeLib)]
    internal static partial int aster_http_listener_quic_addr(ulong runtime, ulong listener,
        out ulong buffer, out IntPtr ptr, out uint len);

    [LibraryImport(NativeLib)]
    internal static partial int aster_http_listener_stop(ulong runtime, ulong listener, uint grace_ms);
}
