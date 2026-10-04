using System;
using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations for C ABI 4.2 (expose), ffi/RENAME_MAP.md "New in 4.2". Struct sizes are
// pinned by AbiContractTests.
internal static unsafe partial class Native
{
    // ─── 4.2: expose ────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_expose_new")]
    public static partial int aster_expose_new(ulong runtime, ulong node, out ulong out_expose);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_free")]
    public static partial int aster_expose_free(ulong runtime, ulong expose);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_unexpose")]
    public static partial int aster_expose_unexpose(ulong runtime, ulong expose, Bytes service_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_handle_connection")]
    public static partial int aster_expose_handle_connection(ulong runtime, ulong expose, ulong connection);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_serve")]
    public static partial int aster_expose_serve(ulong runtime, ulong node, ulong expose, BytesList alpns, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_socket")]
    public static partial int aster_expose_socket(ulong runtime, ulong expose, Bytes service_id, Bytes addr, ulong authorizer_upcalls);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_bodies_new")]
    public static partial int aster_http_bodies_new(ulong runtime, out ulong out_bodies);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_bodies_free")]
    public static partial int aster_http_bodies_free(ulong runtime, ulong bodies);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_bodies_open_count")]
    public static partial int aster_http_bodies_open_count(ulong runtime, ulong bodies, out ulong out_count);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_foreign_options_init")]
    public static partial int aster_http_foreign_options_init(HttpForeignOptionsNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_http_foreign")]
    public static partial int aster_expose_http_foreign(ulong runtime, ulong expose, ulong upcalls, ulong bodies,
        Bytes service_id, HttpForeignOptionsNative* opts);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_route_policy_foreign")]
    public static partial int aster_expose_route_policy_foreign(ulong runtime, ulong expose, ulong upcalls);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_body_read")]
    public static partial int aster_http_body_read(ulong runtime, ulong bodies, ulong stream_id, byte* buf, UIntPtr cap,
        uint timeout_ms, UIntPtr* out_len, uint* out_eof);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_body_write")]
    public static partial int aster_http_body_write(ulong runtime, ulong bodies, ulong stream_id, byte* chunk, UIntPtr len, uint timeout_ms);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_body_try_write")]
    public static partial int aster_http_body_try_write(ulong runtime, ulong bodies, ulong stream_id, byte* chunk, UIntPtr len, uint* out_queued);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_body_finish")]
    public static partial int aster_http_body_finish(ulong runtime, ulong bodies, ulong stream_id, byte* trailers, UIntPtr len);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_body_abort")]
    public static partial int aster_http_body_abort(ulong runtime, ulong bodies, ulong stream_id, byte* message, UIntPtr len);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_relay_request")]
    public static partial int aster_expose_relay_request(ulong runtime, ulong connection, Bytes request, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_request_route")]
    public static partial int aster_expose_request_route(ulong runtime, ulong connection, RouteSpecNative* specs, UIntPtr spec_count,
        Bytes metadata, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_route_registration_free")]
    public static partial int aster_route_registration_free(ulong runtime, ulong registration);
    // ─── 4.3: streaming consumer relay ──────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_http_body_status_to_status")]
    public static partial int aster_http_body_status_to_status(int code);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_relay_options_init")]
    public static partial int aster_http_relay_options_init(HttpRelayOptionsNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_expose_relay_stream")]
    public static partial int aster_expose_relay_stream(ulong runtime, ulong connection, Bytes request,
        HttpRelayOptionsNative* opts, ulong* out_upload, ulong* out_response);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_upload_write")]
    public static partial int aster_http_upload_write(ulong runtime, ulong upload, byte* chunk, UIntPtr len, uint timeout_ms);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_upload_try_write")]
    public static partial int aster_http_upload_try_write(ulong runtime, ulong upload, byte* chunk, UIntPtr len, uint* out_queued);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_upload_finish")]
    public static partial int aster_http_upload_finish(ulong runtime, ulong upload);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_upload_abort")]
    public static partial int aster_http_upload_abort(ulong runtime, ulong upload, byte* message, UIntPtr len);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_upload_ready")]
    public static partial int aster_http_upload_ready(ulong runtime, ulong upload, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_upload_free")]
    public static partial int aster_http_upload_free(ulong runtime, ulong upload);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_response_wait")]
    public static partial int aster_http_response_wait(ulong runtime, ulong response, uint timeout_ms, byte* buf, UIntPtr cap, UIntPtr* out_len);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_response_read")]
    public static partial int aster_http_response_read(ulong runtime, ulong response, byte* buf, UIntPtr cap, uint timeout_ms, UIntPtr* out_len, uint* out_eof);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_response_ready")]
    public static partial int aster_http_response_ready(ulong runtime, ulong response, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_response_close")]
    public static partial int aster_http_response_close(ulong runtime, ulong response);
    [LibraryImport(NativeLib, EntryPoint = "aster_http_response_free")]
    public static partial int aster_http_response_free(ulong runtime, ulong response);
}

/// <summary><c>aster_http_foreign_options_t</c> (48 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct HttpForeignOptionsNative
{
    public uint struct_size;
    public uint flags;
    public ulong inline_request_max;
    public ulong max_request_body;
    public ulong max_response_body;
    public ulong max_chunk;
    public uint response_buffer;
    public uint reserved;
}

/// <summary><c>aster_route_spec_t</c> (40 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RouteSpecNative
{
    public Bytes host;
    public Bytes service_id;
    public uint port;
    public uint protocol;
}

/// <summary><c>aster_http_relay_options_t</c> (24 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct HttpRelayOptionsNative
{
    public uint struct_size;
    public uint flags;
    public ulong max_chunk;
    public uint upload_capacity;
    public uint reserved;
}
