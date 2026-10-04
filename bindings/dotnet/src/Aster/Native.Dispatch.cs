using System;
using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations for C ABI 3.5 (ffi/RENAME_MAP.md "New in 3.5"): the dynamic-dispatch
// server (aster_server_* / aster_dispatch_*, ffi/src/server.rs) and the payload-level RPC client
// over the pooled connection (aster_rpc_* / aster_call_*_payload, ffi/src/rpc.rs).
internal static unsafe partial class Native
{
    /// <summary><c>ASTER_TIMEOUT_INFINITE</c>.</summary>
    public const uint TimeoutInfinite = uint.MaxValue;

    // ─── Server ─────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_server_builder_new")]
    public static partial int aster_server_builder_new(ulong runtime, ulong node, out ulong out_builder);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_add_service_json")]
    public static partial int aster_server_add_service_json(ulong builder, byte* json, uint json_len, out uint out_idx);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_set_option")]
    public static partial int aster_server_set_option(ulong builder, uint key, ulong value);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_start")]
    public static partial int aster_server_start(ulong runtime, ulong builder, uint ring_capacity, out ulong out_server);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_builder_free")]
    public static partial int aster_server_builder_free(ulong builder);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_destroy")]
    public static partial int aster_server_destroy(ulong runtime, ulong server);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_set_peer_attributes")]
    public static partial int aster_server_set_peer_attributes(ulong server, byte* peer, uint peer_len, byte* kv, uint kv_len);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_poll")]
    public static partial uint aster_server_poll(ulong runtime, ulong server, DispatchEventNative* events, uint max, uint timeout_ms);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_live_calls")]
    public static partial ulong aster_server_live_calls(ulong server);
    [LibraryImport(NativeLib, EntryPoint = "aster_dynamic_service_contract_id")]
    public static partial int aster_dynamic_service_contract_id(byte* json, uint json_len, Bytes* @out);

    // Response operations never block (QUEUE_FULL instead).
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_respond")]
    public static partial int aster_dispatch_respond(ulong runtime, ulong server, ulong call_id,
        byte* payload, uint payload_len, byte* status_aev1, uint status_len, ulong* out_required_len);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_send")]
    public static partial int aster_dispatch_send(ulong runtime, ulong server, ulong call_id,
        byte* payload, uint payload_len, ulong* out_required_len);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_finish")]
    public static partial int aster_dispatch_finish(ulong runtime, ulong server, ulong call_id,
        byte* status_aev1, uint status_len, ulong* out_required_len);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_reserve")]
    public static partial int aster_dispatch_reserve(ulong runtime, ulong server, ulong call_id, ulong required_len);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_cancel_reservation")]
    public static partial int aster_dispatch_cancel_reservation(ulong runtime, ulong server, ulong call_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_recv")]
    public static partial int aster_dispatch_recv(ulong runtime, ulong server, ulong call_id, uint timeout_ms,
        byte** out_ptr, uint* out_len, ulong* out_buffer);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_is_cancelled")]
    public static partial int aster_dispatch_is_cancelled(ulong runtime, ulong server, ulong call_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_metadata")]
    public static partial int aster_dispatch_metadata(ulong runtime, ulong server, ulong call_id,
        byte** out_ptr, uint* out_len, ulong* out_buffer);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_attributes")]
    public static partial int aster_dispatch_attributes(ulong runtime, ulong server, ulong call_id,
        byte** out_ptr, uint* out_len, ulong* out_buffer);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_abandon")]
    public static partial int aster_dispatch_abandon(ulong runtime, ulong server, ulong call_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_buffer_release")]
    public static partial int aster_dispatch_buffer_release(ulong runtime, ulong server, ulong buffer);

    // ─── Client ─────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_rpc_unary")]
    public static partial int aster_rpc_unary(ulong runtime, ulong connection, CallOptionsNative* options,
        byte* request, uint request_len,
        byte** out_resp_ptr, uint* out_resp_len, ulong* out_resp_buffer,
        int* out_status_code, byte** out_status_ptr, uint* out_status_len, ulong* out_status_buffer);
    [LibraryImport(NativeLib, EntryPoint = "aster_rpc_open")]
    public static partial int aster_rpc_open(ulong runtime, ulong connection, byte pattern, CallOptionsNative* options, out ulong out_call);
    [LibraryImport(NativeLib, EntryPoint = "aster_call_send_payload")]
    public static partial int aster_call_send_payload(ulong runtime, ulong call, byte* payload, uint payload_len, byte end_stream);
    [LibraryImport(NativeLib, EntryPoint = "aster_call_send_payloads")]
    public static partial int aster_call_send_payloads(ulong runtime, ulong call, Bytes* payloads, uint count, byte end_stream);
    [LibraryImport(NativeLib, EntryPoint = "aster_call_recv_payload")]
    public static partial int aster_call_recv_payload(ulong runtime, ulong call, uint timeout_ms,
        byte** out_ptr, uint* out_len, ulong* out_buffer, byte* out_is_trailer);
    [LibraryImport(NativeLib, EntryPoint = "aster_call_cancel")]
    public static partial int aster_call_cancel(ulong runtime, ulong call);
    [LibraryImport(NativeLib, EntryPoint = "aster_connection_next_session_id")]
    public static partial int aster_connection_next_session_id(ulong runtime, ulong connection, out uint out_session_id);
}
