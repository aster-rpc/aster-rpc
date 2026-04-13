using System;
using System.Runtime.InteropServices;

namespace Aster;

/// <summary>
/// Low-level FFI declarations for the iroh C ABI, generated via source-generated P/Invoke.
/// All native symbols are resolved from the native library at load time.
/// </summary>
internal static partial class Native
{
    private const string NativeLib = "libaster_transport_ffi";

    // ─── Version ────────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_abi_version_major")]
    public static partial int iroh_abi_version_major();

    [LibraryImport(NativeLib, EntryPoint = "iroh_abi_version_minor")]
    public static partial int iroh_abi_version_minor();

    [LibraryImport(NativeLib, EntryPoint = "iroh_abi_version_patch")]
    public static partial int iroh_abi_version_patch();

    // ─── Status ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_status_name")]
    public static partial IntPtr iroh_status_name(uint code);

    [LibraryImport(NativeLib, EntryPoint = "iroh_last_error_message", StringMarshalling = StringMarshalling.Utf8)]
    public static partial int iroh_last_error_message(byte[] buf, int buf_len);

    // ─── Runtime ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_runtime_new")]
    public static partial int iroh_runtime_new(ref RuntimeConfig config, out ulong out_runtime);

    [LibraryImport(NativeLib, EntryPoint = "iroh_runtime_close")]
    public static partial int iroh_runtime_close(ulong runtime);

    [LibraryImport(NativeLib, EntryPoint = "iroh_poll_events")]
    public static partial int iroh_poll_events(ulong runtime, IntPtr events, int max_events, int timeout_ms);

    // ─── Buffers ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_buffer_release")]
    public static partial int iroh_buffer_release(ulong runtime, ulong buffer);

    // ─── Strings ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_string_release")]
    public static partial int iroh_string_release(IntPtr data, UIntPtr len);

    // ─── Operation cancellation ───────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_operation_cancel")]
    public static partial int iroh_operation_cancel(ulong runtime, ulong operation);

    // ─── Node ────────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_node_memory")]
    public static partial int iroh_node_memory(ulong runtime, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_node_memory_with_alpns")]
    public static unsafe partial int iroh_node_memory_with_alpns(
        ulong runtime, byte** alpns, UIntPtr* alpn_lens, UIntPtr alpn_count,
        ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_node_id")]
    public static partial int iroh_node_id(ulong runtime, ulong node, IntPtr out_buf, UIntPtr capacity, out UIntPtr out_len);

    [LibraryImport(NativeLib, EntryPoint = "iroh_node_addr_info")]
    public static partial int iroh_node_addr_info(ulong runtime, ulong node, IntPtr out_buf, UIntPtr buf_capacity, out NodeAddr out_addr);

    [LibraryImport(NativeLib, EntryPoint = "iroh_node_free")]
    public static partial int iroh_node_free(ulong runtime, ulong node);

    // ─── Endpoint ────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_endpoint_create")]
    public static partial int iroh_endpoint_create(ulong runtime, IntPtr config, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_endpoint_close")]
    public static partial int iroh_endpoint_close(ulong runtime, ulong endpoint, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_endpoint_id")]
    public static partial int iroh_endpoint_id(ulong runtime, ulong endpoint, IntPtr out_buf, UIntPtr capacity, out UIntPtr out_len);

    // ─── Connection ──────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_accept")]
    public static partial int iroh_accept(ulong runtime, ulong endpoint, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_connect")]
    public static partial int iroh_connect(ulong runtime, ulong endpoint_or_node, IntPtr config, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_connection_close")]
    public static partial int iroh_connection_close(ulong runtime, ulong connection, uint error_code, Bytes reason);

    [LibraryImport(NativeLib, EntryPoint = "iroh_connection_remote_id")]
    public static partial int iroh_connection_remote_id(ulong runtime, ulong connection, IntPtr out_buf, UIntPtr capacity, out UIntPtr out_len);

    [LibraryImport(NativeLib, EntryPoint = "iroh_connection_send_datagram")]
    public static partial int iroh_connection_send_datagram(ulong runtime, ulong connection, Bytes data);

    [LibraryImport(NativeLib, EntryPoint = "iroh_connection_read_datagram")]
    public static partial int iroh_connection_read_datagram(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    // ─── Streams ─────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_open_bi")]
    public static partial int iroh_open_bi(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_accept_bi")]
    public static partial int iroh_accept_bi(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_open_uni")]
    public static partial int iroh_open_uni(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_accept_uni")]
    public static partial int iroh_accept_uni(ulong runtime, ulong connection, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_stream_write")]
    public static partial int iroh_stream_write(ulong runtime, ulong send_stream, Bytes data, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_stream_finish")]
    public static partial int iroh_stream_finish(ulong runtime, ulong send_stream, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_stream_read")]
    public static partial int iroh_stream_read(ulong runtime, ulong recv_stream, UIntPtr max_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_stream_read_to_end")]
    public static partial int iroh_stream_read_to_end(ulong runtime, ulong recv_stream, UIntPtr max_size, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_stream_stop")]
    public static partial int iroh_stream_stop(ulong runtime, ulong stream, uint error_code);

    [LibraryImport(NativeLib, EntryPoint = "iroh_send_stream_free")]
    public static partial int iroh_send_stream_free(ulong runtime, ulong send_stream);

    [LibraryImport(NativeLib, EntryPoint = "iroh_recv_stream_free")]
    public static partial int iroh_recv_stream_free(ulong runtime, ulong recv_stream);

    // ─── Reactor ─────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_reactor_create")]
    public static partial int aster_reactor_create(ulong runtime, ulong node, uint ring_capacity, out ulong out_reactor);

    [LibraryImport(NativeLib, EntryPoint = "aster_reactor_destroy")]
    public static partial int aster_reactor_destroy(ulong runtime, ulong reactor);

    [LibraryImport(NativeLib, EntryPoint = "aster_reactor_poll")]
    public static partial uint aster_reactor_poll(ulong runtime, ulong reactor, IntPtr out_calls, uint max_calls, uint timeout_ms);

    [LibraryImport(NativeLib, EntryPoint = "aster_reactor_submit")]
    public static partial int aster_reactor_submit(ulong runtime, ulong reactor, ulong call_id,
        IntPtr response_ptr, uint response_len, IntPtr trailer_ptr, uint trailer_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_reactor_buffer_release")]
    public static partial int aster_reactor_buffer_release(ulong runtime, ulong reactor, ulong buffer);

    // ─── Add node addr ───────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_add_node_addr")]
    public static partial int iroh_add_node_addr(ulong runtime, ulong endpoint, NodeAddr addr);

    // ─── Blobs ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_blobs_add_bytes")]
    public static partial int iroh_blobs_add_bytes(ulong runtime, ulong node, IntPtr data, UIntPtr data_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_blobs_read")]
    public static partial int iroh_blobs_read(ulong runtime, ulong node, IntPtr hash, UIntPtr hash_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_blobs_download")]
    public static partial int iroh_blobs_download(ulong runtime, ulong node, IntPtr ticket, UIntPtr ticket_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_blobs_status")]
    public static partial int iroh_blobs_status(ulong runtime, ulong node, IntPtr hash, UIntPtr hash_len, out uint out_status, out ulong out_size);

    [LibraryImport(NativeLib, EntryPoint = "iroh_blobs_has")]
    public static partial int iroh_blobs_has(ulong runtime, ulong node, IntPtr hash, UIntPtr hash_len, out uint out_has);

    [LibraryImport(NativeLib, EntryPoint = "iroh_blobs_create_ticket")]
    public static partial int iroh_blobs_create_ticket(ulong runtime, ulong node, IntPtr hash, UIntPtr hash_len, uint format, IntPtr out_buf, UIntPtr buf_capacity, out UIntPtr out_len);

    [LibraryImport(NativeLib, EntryPoint = "iroh_blobs_observe_complete")]
    public static partial int iroh_blobs_observe_complete(ulong runtime, ulong node, IntPtr hash, UIntPtr hash_len, ulong user_data, out ulong out_operation);

    // ─── Docs ────────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_docs_create")]
    public static partial int iroh_docs_create(ulong runtime, ulong node, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_docs_create_author")]
    public static partial int iroh_docs_create_author(ulong runtime, ulong node, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_docs_join")]
    public static partial int iroh_docs_join(ulong runtime, ulong node, IntPtr ticket, UIntPtr ticket_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_set_bytes")]
    public static partial int iroh_doc_set_bytes(ulong runtime, ulong doc, IntPtr author_id, UIntPtr author_len, IntPtr key, UIntPtr key_len, IntPtr value, UIntPtr value_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_get_exact")]
    public static partial int iroh_doc_get_exact(ulong runtime, ulong doc, IntPtr author_id, UIntPtr author_len, IntPtr key, UIntPtr key_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_query")]
    public static partial int iroh_doc_query(ulong runtime, ulong doc, uint mode, IntPtr prefix, UIntPtr prefix_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_subscribe")]
    public static partial int iroh_doc_subscribe(ulong runtime, ulong doc, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_event_recv")]
    public static partial int iroh_doc_event_recv(ulong runtime, ulong subscription, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_share")]
    public static partial int iroh_doc_share(ulong runtime, ulong doc, uint mode, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_start_sync")]
    public static partial int iroh_doc_start_sync(ulong runtime, ulong doc, Bytes peers_json, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_leave")]
    public static partial int iroh_doc_leave(ulong runtime, ulong doc, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_doc_free")]
    public static partial int iroh_doc_free(ulong runtime, ulong doc);

    // ─── Gossip ──────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_gossip_subscribe")]
    public static partial int iroh_gossip_subscribe(ulong runtime, ulong node, Bytes topic, BytesList peers, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_gossip_broadcast")]
    public static partial int iroh_gossip_broadcast(ulong runtime, ulong topic, Bytes data, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_gossip_recv")]
    public static partial int iroh_gossip_recv(ulong runtime, ulong topic, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_gossip_topic_free")]
    public static partial int iroh_gossip_topic_free(ulong runtime, ulong topic);

    // ─── Tags ────────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "iroh_tags_set")]
    public static partial int iroh_tags_set(ulong runtime, ulong node, IntPtr name, UIntPtr name_len, IntPtr hash, UIntPtr hash_len, uint format, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_tags_get")]
    public static partial int iroh_tags_get(ulong runtime, ulong node, IntPtr name, UIntPtr name_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_tags_delete")]
    public static partial int iroh_tags_delete(ulong runtime, ulong node, IntPtr name, UIntPtr name_len, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "iroh_tags_list_prefix")]
    public static partial int iroh_tags_list_prefix(ulong runtime, ulong node, IntPtr prefix, UIntPtr prefix_len, ulong user_data, out ulong out_operation);

    // ─── Aster Contract Identity ─────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_contract_id")]
    public static unsafe partial int aster_contract_id(byte* json_ptr, UIntPtr json_len, byte* out_buf, UIntPtr* out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_canonical_bytes")]
    public static unsafe partial int aster_canonical_bytes(byte* type_name_ptr, UIntPtr type_name_len, byte* json_ptr, UIntPtr json_len, byte* out_buf, UIntPtr* out_len);

    // ─── Aster Registry (§11) ────────────────────────────────────────────────

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

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_key")]
    public static unsafe partial int aster_registry_key(
        int kind,
        byte* arg1_ptr, UIntPtr arg1_len,
        byte* arg2_ptr, UIntPtr arg2_len,
        byte* arg3_ptr, UIntPtr arg3_len,
        byte* out_buf, UIntPtr* out_len);

    // Registry async doc-backed ops (event kinds 80-84).

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_resolve")]
    public static unsafe partial int aster_registry_resolve(
        ulong runtime, ulong doc,
        byte* opts_json_ptr, UIntPtr opts_json_len,
        ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_publish")]
    public static unsafe partial int aster_registry_publish(
        ulong runtime, ulong doc,
        byte* author_id_ptr, UIntPtr author_id_len,
        byte* lease_json_ptr, UIntPtr lease_json_len,
        byte* artifact_json_ptr, UIntPtr artifact_json_len,
        byte* service_ptr, UIntPtr service_len,
        int version,
        byte* channel_ptr, UIntPtr channel_len,
        ulong gossip_topic, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_renew_lease")]
    public static unsafe partial int aster_registry_renew_lease(
        ulong runtime, ulong doc,
        byte* author_id_ptr, UIntPtr author_id_len,
        byte* service_ptr, UIntPtr service_len,
        byte* contract_id_ptr, UIntPtr contract_id_len,
        byte* endpoint_id_ptr, UIntPtr endpoint_id_len,
        byte* health_ptr, UIntPtr health_len,
        float load,
        int lease_duration_s,
        ulong gossip_topic, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_acl_add_writer")]
    public static unsafe partial int aster_registry_acl_add_writer(
        ulong runtime, ulong doc,
        byte* author_id_ptr, UIntPtr author_id_len,
        byte* writer_id_ptr, UIntPtr writer_id_len,
        ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_acl_remove_writer")]
    public static unsafe partial int aster_registry_acl_remove_writer(
        ulong runtime, ulong doc,
        byte* author_id_ptr, UIntPtr author_id_len,
        byte* writer_id_ptr, UIntPtr writer_id_len,
        ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_registry_acl_list_writers")]
    public static partial int aster_registry_acl_list_writers(
        ulong runtime, ulong doc,
        ulong user_data, out ulong out_operation);

    // ─── Hook responders (Phase 1b) ──────────────────────────────────────────
    // The hook FFI is event-driven: BEFORE_CONNECT (70) / AFTER_CONNECT (71)
    // events arrive on the runtime event pump carrying an iroh_hook_invocation_t
    // handle in event.related. The host then calls one of these respond
    // functions to allow/deny and release the invocation. Calling respond
    // twice for the same invocation returns NOT_FOUND.

    [LibraryImport(NativeLib, EntryPoint = "iroh_hook_before_connect_respond")]
    public static partial int iroh_hook_before_connect_respond(
        ulong runtime, ulong invocation, int decision);

    [LibraryImport(NativeLib, EntryPoint = "iroh_hook_after_connect_respond")]
    public static partial int iroh_hook_after_connect_respond(
        ulong runtime, ulong invocation);
}
