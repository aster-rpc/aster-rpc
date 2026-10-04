using System;
using System.Runtime.InteropServices;

namespace Aster;

/// <summary>
/// Low-level P/Invoke declarations for the Aster C ABI (<c>ffi/aster_ffi.h</c>, ABI 4.x).
///
/// Every declaration here mirrors the header prototype exactly: <c>uintptr_t</c> maps to
/// <see cref="UIntPtr"/>, <c>aster_bytes_t</c> / <c>aster_bytes_list_t</c> passed by value map
/// to <see cref="Bytes"/> / <see cref="BytesList"/> (never to separate ptr/len arguments, which
/// only happens to coincide with the struct calling convention on SysV/AAPCS64 and breaks on
/// Windows x64), and enum-typed parameters are carried as <c>uint32_t</c>.
/// </summary>
internal static partial class Native
{
    private const string NativeLib = "libaster_transport_ffi";

    /// <summary>ABI major this binding is written against (<c>ASTER_ABI_VERSION_MAJOR</c>).</summary>
    public const uint AbiVersionMajor = 4;

    // ─── Version / status ───────────────────────────────────────────────────

    /// <summary>Packed <c>(major &lt;&lt; 16) | (minor &lt;&lt; 8) | patch</c>.</summary>
    [LibraryImport(NativeLib, EntryPoint = "aster_abi_version")]
    public static partial uint aster_abi_version();

    [LibraryImport(NativeLib, EntryPoint = "aster_status_name")]
    public static partial IntPtr aster_status_name(int status);

    /// <summary>Returns the full message length (may exceed <paramref name="capacity"/>).</summary>
    [LibraryImport(NativeLib, EntryPoint = "aster_last_error_message")]
    public static unsafe partial UIntPtr aster_last_error_message(byte* buffer, UIntPtr capacity);

    // ─── Runtime ────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_runtime_new")]
    public static partial int aster_runtime_new(uint expected_abi_major, in RuntimeConfig config, out ulong out_runtime);

    [LibraryImport(NativeLib, EntryPoint = "aster_runtime_close")]
    public static partial int aster_runtime_close(ulong runtime);

    [LibraryImport(NativeLib, EntryPoint = "aster_poll_events")]
    public static partial UIntPtr aster_poll_events(ulong runtime, IntPtr out_events, UIntPtr max_events, uint timeout_ms);

    // ─── Buffers / strings ─────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_buffer_release")]
    public static partial int aster_buffer_release(ulong runtime, ulong buffer);

    [LibraryImport(NativeLib, EntryPoint = "aster_string_release")]
    public static partial int aster_string_release(IntPtr ptr, UIntPtr len);

    [LibraryImport(NativeLib, EntryPoint = "aster_operation_cancel")]
    public static partial int aster_operation_cancel(ulong runtime, ulong operation);

    // ─── Node ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_node_id")]
    public static partial int aster_node_id(ulong runtime, ulong node, IntPtr out_buf, UIntPtr capacity, out UIntPtr out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_node_addr_info")]
    public static partial int aster_node_addr_info(ulong runtime, ulong node, IntPtr out_buf, UIntPtr buf_capacity, out NodeAddr out_addr);

    [LibraryImport(NativeLib, EntryPoint = "aster_node_free")]
    public static partial int aster_node_free(ulong runtime, ulong node);

    // ─── Address book ───────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_add_node_addr")]
    public static partial int aster_add_node_addr(ulong runtime, ulong endpoint, NodeAddr addr);

    // ─── Connection ─────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_accept")]
    public static partial int aster_accept(ulong runtime, ulong endpoint_or_node, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_connect")]
    public static partial int aster_connect(ulong runtime, ulong endpoint_or_node, IntPtr config, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_connection_close")]
    public static partial int aster_connection_close(ulong runtime, ulong connection, uint error_code, Bytes reason);

    [LibraryImport(NativeLib, EntryPoint = "aster_connection_free")]
    public static partial int aster_connection_free(ulong runtime, ulong connection);

    [LibraryImport(NativeLib, EntryPoint = "aster_connection_remote_id")]
    public static partial int aster_connection_remote_id(ulong runtime, ulong connection, IntPtr out_buf, UIntPtr capacity, out UIntPtr out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_connection_send_datagram")]
    public static partial int aster_connection_send_datagram(ulong runtime, ulong connection, Bytes data);

    [LibraryImport(NativeLib, EntryPoint = "aster_connection_read_datagram")]
    public static partial int aster_connection_read_datagram(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    // ─── Streams ────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_open_bi")]
    public static partial int aster_open_bi(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_accept_bi")]
    public static partial int aster_accept_bi(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_open_uni")]
    public static partial int aster_open_uni(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_accept_uni")]
    public static partial int aster_accept_uni(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_stream_write")]
    public static partial int aster_stream_write(ulong runtime, ulong send_stream, Bytes data, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_stream_finish")]
    public static partial int aster_stream_finish(ulong runtime, ulong send_stream, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_stream_read")]
    public static partial int aster_stream_read(ulong runtime, ulong recv_stream, UIntPtr max_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_stream_read_to_end")]
    public static partial int aster_stream_read_to_end(ulong runtime, ulong recv_stream, UIntPtr max_size, ulong user_data, out ulong out_operation);

    /// <summary>Stops a <b>recv</b> stream (there is no send-stream reset in the C ABI).</summary>
    [LibraryImport(NativeLib, EntryPoint = "aster_stream_stop")]
    public static partial int aster_stream_stop(ulong runtime, ulong recv_stream, uint error_code);

    [LibraryImport(NativeLib, EntryPoint = "aster_send_stream_free")]
    public static partial int aster_send_stream_free(ulong runtime, ulong send_stream);

    [LibraryImport(NativeLib, EntryPoint = "aster_recv_stream_free")]
    public static partial int aster_recv_stream_free(ulong runtime, ulong recv_stream);

    // ─── Blobs ──────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_add_bytes")]
    public static partial int aster_blobs_add_bytes(ulong runtime, ulong node, Bytes data, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_read")]
    public static partial int aster_blobs_read(ulong runtime, ulong node, Bytes hash_hex, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_download")]
    public static partial int aster_blobs_download(ulong runtime, ulong node, Bytes ticket, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_status")]
    public static partial int aster_blobs_status(ulong runtime, ulong node, IntPtr hash_hex_ptr, UIntPtr hash_hex_len, out uint out_status, out ulong out_size);

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_has")]
    public static partial int aster_blobs_has(ulong runtime, ulong node, IntPtr hash_hex_ptr, UIntPtr hash_hex_len, out uint out_has);

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_create_ticket")]
    public static partial int aster_blobs_create_ticket(ulong runtime, ulong node, Bytes hash_hex, IntPtr out_buf, UIntPtr capacity, out UIntPtr out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_observe_complete")]
    public static partial int aster_blobs_observe_complete(ulong runtime, ulong node, IntPtr hash_hex_ptr, UIntPtr hash_hex_len, ulong user_data, out ulong out_operation);

    // ─── Docs ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_docs_create")]
    public static partial int aster_docs_create(ulong runtime, ulong node, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_docs_create_author")]
    public static partial int aster_docs_create_author(ulong runtime, ulong node, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_docs_join")]
    public static partial int aster_docs_join(ulong runtime, ulong node, Bytes ticket, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_set_bytes")]
    public static partial int aster_doc_set_bytes(ulong runtime, ulong doc, Bytes author_hex, Bytes key, Bytes value, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_get_exact")]
    public static partial int aster_doc_get_exact(ulong runtime, ulong doc, Bytes author_hex, Bytes key, ulong user_data, out ulong out_operation);

    /// <summary><paramref name="mode"/> is an <c>aster_doc_query_mode_t</c>.</summary>
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_query")]
    public static partial int aster_doc_query(ulong runtime, ulong doc, uint mode, Bytes key, ulong limit, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_subscribe")]
    public static partial int aster_doc_subscribe(ulong runtime, ulong doc, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_event_recv")]
    public static partial int aster_doc_event_recv(ulong runtime, ulong receiver, ulong user_data, out ulong out_operation);

    /// <summary><paramref name="mode"/> is an <c>aster_doc_share_mode_t</c> (0 read, 1 write).</summary>
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_share")]
    public static partial int aster_doc_share(ulong runtime, ulong doc, uint mode, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_start_sync")]
    public static partial int aster_doc_start_sync(ulong runtime, ulong doc, BytesList peers, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_leave")]
    public static partial int aster_doc_leave(ulong runtime, ulong doc, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_doc_free")]
    public static partial int aster_doc_free(ulong runtime, ulong doc);

    // ─── Gossip ─────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_gossip_subscribe")]
    public static partial int aster_gossip_subscribe(ulong runtime, ulong node, Bytes topic, BytesList peers, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_gossip_broadcast")]
    public static partial int aster_gossip_broadcast(ulong runtime, ulong topic, Bytes data, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_gossip_recv")]
    public static partial int aster_gossip_recv(ulong runtime, ulong topic, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_gossip_topic_free")]
    public static partial int aster_gossip_topic_free(ulong runtime, ulong topic);

    // ─── Tags ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_tags_set")]
    public static partial int aster_tags_set(ulong runtime, ulong node, IntPtr name_ptr, UIntPtr name_len, IntPtr hash_hex_ptr, UIntPtr hash_hex_len, uint format, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_tags_get")]
    public static partial int aster_tags_get(ulong runtime, ulong node, IntPtr name_ptr, UIntPtr name_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_tags_delete")]
    public static partial int aster_tags_delete(ulong runtime, ulong node, IntPtr name_ptr, UIntPtr name_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_tags_list_prefix")]
    public static partial int aster_tags_list_prefix(ulong runtime, ulong node, IntPtr prefix_ptr, UIntPtr prefix_len, ulong user_data, out ulong out_operation);

    // ─── Contract identity ──────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_contract_id")]
    public static unsafe partial int aster_contract_id(byte* json_ptr, UIntPtr json_len, byte* out_buf, UIntPtr* out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_canonical_bytes")]
    public static unsafe partial int aster_canonical_bytes(byte* type_name_ptr, UIntPtr type_name_len, byte* json_ptr, UIntPtr json_len, byte* out_buf, UIntPtr* out_len);

    // ─── Registry (§11) ─────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_now_epoch_ms")]
    public static partial long aster_registry_now_epoch_ms();

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_is_fresh")]
    public static unsafe partial int aster_registry_is_fresh(byte* lease_json_ptr, UIntPtr lease_json_len, int lease_duration_s);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_is_routable")]
    public static unsafe partial int aster_registry_is_routable(byte* status_ptr, UIntPtr status_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_filter_and_rank")]
    public static unsafe partial int aster_registry_filter_and_rank(
        byte* leases_json_ptr, UIntPtr leases_json_len,
        byte* opts_json_ptr, UIntPtr opts_json_len,
        byte* out_buf, UIntPtr* out_len);

    /// <summary><paramref name="kind"/> is an <c>aster_registry_key_kind_t</c>.</summary>
    [LibraryImport(NativeLib, EntryPoint = "aster_registry_key")]
    public static unsafe partial int aster_registry_key(
        int kind,
        byte* arg1_ptr, UIntPtr arg1_len,
        byte* arg2_ptr, UIntPtr arg2_len,
        byte* arg3_ptr, UIntPtr arg3_len,
        byte* out_buf, UIntPtr* out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_resolve")]
    public static partial int aster_registry_resolve(
        ulong runtime, ulong doc, Bytes opts_json, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_publish")]
    public static partial int aster_registry_publish(
        ulong runtime, ulong doc,
        Bytes author_id, Bytes lease_json, Bytes artifact_json, Bytes service,
        int version, Bytes channel,
        ulong gossip_topic, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_renew_lease")]
    public static partial int aster_registry_renew_lease(
        ulong runtime, ulong doc,
        Bytes author_id, Bytes service, Bytes contract_id, Bytes endpoint_id, Bytes health,
        float load, int lease_duration_s,
        ulong gossip_topic, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_acl_add_writer")]
    public static partial int aster_registry_acl_add_writer(
        ulong runtime, ulong doc, Bytes author_id, Bytes writer_id, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_acl_remove_writer")]
    public static partial int aster_registry_acl_remove_writer(
        ulong runtime, ulong doc, Bytes author_id, Bytes writer_id, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_acl_list_writers")]
    public static partial int aster_registry_acl_list_writers(
        ulong runtime, ulong doc, ulong user_data, out ulong out_operation);

    // ─── Hook responders ────────────────────────────────────────────────────
    // HOOK_BEFORE_CONNECT (70) / HOOK_AFTER_CONNECT (71) events carry an
    // aster_hook_invocation_t in event.related; respond exactly once.

    /// <summary><paramref name="decision"/> is an <c>aster_hook_decision_t</c> (0 allow, 1 deny).</summary>
    [LibraryImport(NativeLib, EntryPoint = "aster_hook_before_connect_respond")]
    public static partial int aster_hook_before_connect_respond(ulong runtime, ulong invocation, uint decision);

    [LibraryImport(NativeLib, EntryPoint = "aster_hook_after_connect_respond")]
    public static partial int aster_hook_after_connect_respond(ulong runtime, ulong invocation);
}
