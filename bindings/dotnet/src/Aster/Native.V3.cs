using System;
using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations for the C ABI 3.1 – 3.5 additions (ffi/RENAME_MAP.md "New in 3.x"),
// all part of ABI 4.0.
// Struct mirrors live in NativeStructs.cs; their sizes are pinned by AbiContractTests.
internal static unsafe partial class Native
{
    /// <summary>Minimum ABI minor (of <see cref="AbiVersionMajor"/>) for everything this binding declares.</summary>
    public const uint RequiredAbiMinor = 17; // 4.17: owned raw WebTransport and media controls.

    // ─── 3.1: AEv1 envelope + zstd ──────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_stream_header_encode")]
    public static partial int aster_stream_header_encode(StreamHeaderNative* header, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_stream_header_decode")]
    public static partial int aster_stream_header_decode(byte* buf, UIntPtr len, StreamHeaderNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_stream_header_release")]
    public static partial int aster_stream_header_release(StreamHeaderNative* header);

    [LibraryImport(NativeLib, EntryPoint = "aster_call_header_encode")]
    public static partial int aster_call_header_encode(CallHeaderNative* header, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_call_header_decode")]
    public static partial int aster_call_header_decode(byte* buf, UIntPtr len, CallHeaderNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_call_header_release")]
    public static partial int aster_call_header_release(CallHeaderNative* header);

    [LibraryImport(NativeLib, EntryPoint = "aster_rpc_status_encode")]
    public static partial int aster_rpc_status_encode(RpcStatusNative* status, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_rpc_status_decode")]
    public static partial int aster_rpc_status_decode(byte* buf, UIntPtr len, RpcStatusNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_rpc_status_release")]
    public static partial int aster_rpc_status_release(RpcStatusNative* status);

    [LibraryImport(NativeLib, EntryPoint = "aster_zstd_maybe_compress")]
    public static partial int aster_zstd_maybe_compress(byte* data, UIntPtr len, UIntPtr threshold, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_zstd_decompress")]
    public static partial int aster_zstd_decompress(byte* data, UIntPtr len, Bytes* @out);

    // ─── 3.2: config, node start, Gate 0, admission ─────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_config_init")]
    public static partial int aster_config_init(ConfigNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_config_from_env")]
    public static partial int aster_config_from_env(ConfigNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_config_from_toml")]
    public static partial int aster_config_from_toml(Bytes path, ConfigNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_config_release")]
    public static partial int aster_config_release(ConfigNative* config);

    [LibraryImport(NativeLib, EntryPoint = "aster_node_start")]
    public static partial int aster_node_start(ulong runtime, ConfigNative* config, ulong gate, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_node_set_gate")]
    public static partial int aster_node_set_gate(ulong runtime, ulong node, ulong gate);
    [LibraryImport(NativeLib, EntryPoint = "aster_node_close")]
    public static partial int aster_node_close(ulong runtime, ulong node, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_gate_new")]
    public static partial int aster_gate_new(ulong runtime, out ulong out_gate);
    [LibraryImport(NativeLib, EntryPoint = "aster_gate_free")]
    public static partial int aster_gate_free(ulong runtime, ulong gate);
    [LibraryImport(NativeLib, EntryPoint = "aster_gate_set_trust_mode")]
    public static partial int aster_gate_set_trust_mode(ulong runtime, ulong gate, uint mode);
    [LibraryImport(NativeLib, EntryPoint = "aster_gate_set_admission_alpns")]
    public static partial int aster_gate_set_admission_alpns(ulong runtime, ulong gate, BytesList alpns);
    [LibraryImport(NativeLib, EntryPoint = "aster_gate_set_reject")]
    public static partial int aster_gate_set_reject(ulong runtime, ulong gate, uint code, Bytes reason);
    [LibraryImport(NativeLib, EntryPoint = "aster_gate_admit")]
    public static partial int aster_gate_admit(ulong runtime, ulong gate, Bytes peer);
    [LibraryImport(NativeLib, EntryPoint = "aster_gate_revoke")]
    public static partial int aster_gate_revoke(ulong runtime, ulong gate, Bytes peer);
    [LibraryImport(NativeLib, EntryPoint = "aster_gate_is_admitted")]
    public static partial int aster_gate_is_admitted(ulong runtime, ulong gate, Bytes peer, out uint out_admitted);
    [LibraryImport(NativeLib, EntryPoint = "aster_gate_should_allow")]
    public static partial int aster_gate_should_allow(ulong runtime, ulong gate, Bytes peer, Bytes alpn, out uint out_decision);
    [LibraryImport(NativeLib, EntryPoint = "aster_admission_respond")]
    public static partial int aster_admission_respond(ulong runtime, ulong invocation, uint decision, uint reject_code, Bytes reason);

    // ─── tickets ────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_ticket_encode")]
    public static partial int aster_ticket_encode(
        byte* endpoint_id_hex_ptr, UIntPtr endpoint_id_hex_len,
        byte* relay_addr_ptr, UIntPtr relay_addr_len,
        byte* direct_addrs_json_ptr, UIntPtr direct_addrs_json_len,
        byte* credential_type_ptr, UIntPtr credential_type_len,
        byte* credential_data_ptr, UIntPtr credential_data_len,
        byte* out_buf, UIntPtr* out_len);
    [LibraryImport(NativeLib, EntryPoint = "aster_ticket_decode")]
    public static partial int aster_ticket_decode(byte* ticket_ptr, UIntPtr ticket_len, byte* out_buf, UIntPtr* out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_blake3_hex")]
    public static partial int aster_blake3_hex(byte* bytes_ptr, UIntPtr bytes_len, byte* out_buf, UIntPtr* out_len);

    // ─── RPC call handles (shared by the 3.5 payload client) ─────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_call_release")]
    public static partial int aster_call_release(ulong runtime, ulong call);
    [LibraryImport(NativeLib, EntryPoint = "aster_call_discard")]
    public static partial int aster_call_discard(ulong runtime, ulong call);
    [LibraryImport(NativeLib, EntryPoint = "aster_call_buffer_release")]
    public static partial int aster_call_buffer_release(ulong runtime, ulong buffer);

    // ─── 3.3: data plane ────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_download_hash_from")]
    public static partial int aster_blobs_download_hash_from(ulong runtime, ulong node, Bytes hash_hex, BytesList providers,
        uint format, uint strategy, uint flags, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_download_hash")]
    public static partial int aster_blobs_download_hash(ulong runtime, ulong node, Bytes hash_hex, Bytes provider,
        uint format, uint flags, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_read_range")]
    public static partial int aster_blobs_read_range(ulong runtime, ulong node, Bytes hash_hex, ulong offset, ulong len, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_tags_delete_prefix")]
    public static partial int aster_tags_delete_prefix(ulong runtime, ulong node, Bytes prefix, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_gc_run_once")]
    public static partial int aster_blobs_gc_run_once(ulong runtime, ulong node, ulong user_data, out ulong out_operation);

    [LibraryImport(NativeLib, EntryPoint = "aster_docs_authority_report")]
    public static partial int aster_docs_authority_report(ulong runtime, ulong node, Bytes namespace_id, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_authority_report")]
    public static partial int aster_doc_authority_report(ulong runtime, ulong node, ulong doc, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_authority_fingerprint")]
    public static partial int aster_doc_authority_fingerprint(Bytes namespace_id, uint access, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_docs_retire_local_write_authority")]
    public static partial int aster_docs_retire_local_write_authority(ulong runtime, ulong node, Bytes namespace_id, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_shutdown_for_role_swap")]
    public static partial int aster_doc_shutdown_for_role_swap(ulong runtime, ulong doc, ulong budget_ms, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_docs_open")]
    public static partial int aster_docs_open(ulong runtime, ulong node, Bytes namespace_id, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_docs_import_write_namespace")]
    public static partial int aster_docs_import_write_namespace(ulong runtime, ulong node, Bytes secret, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_docs_import_read_namespace")]
    public static partial int aster_docs_import_read_namespace(ulong runtime, ulong node, Bytes namespace_id, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_set_bytes_durable")]
    public static partial int aster_doc_set_bytes_durable(ulong runtime, ulong doc, Bytes author_hex, Bytes key, Bytes value, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_entry_exists")]
    public static partial int aster_doc_entry_exists(ulong runtime, ulong doc, Bytes author_hex, Bytes key, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_entry_content_status")]
    public static partial int aster_doc_entry_content_status(ulong runtime, ulong doc, Bytes content_hash_hex, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_doc_id")]
    public static partial int aster_doc_id(ulong runtime, ulong doc, byte* out_buf, UIntPtr* out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_connection_paths")]
    public static partial int aster_connection_paths(ulong runtime, ulong connection, PathListNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_connection_selected_path")]
    public static partial int aster_connection_selected_path(ulong runtime, ulong connection, PathListNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_path_list_release")]
    public static partial int aster_path_list_release(PathListNative* list);
    [LibraryImport(NativeLib, EntryPoint = "aster_send_stream_set_priority")]
    public static partial int aster_send_stream_set_priority(ulong runtime, ulong stream, int priority, ulong user_data, out ulong out_operation);
    [LibraryImport(NativeLib, EntryPoint = "aster_send_stream_priority")]
    public static partial int aster_send_stream_priority(ulong runtime, ulong stream, ulong user_data, out ulong out_operation);

    // ─── 3.4: ids, HPKE, attestation, grants (pure, no runtime) ─────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_id_parse")]
    public static partial int aster_id_parse(uint kind, Bytes text, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_id_format")]
    public static partial int aster_id_format(uint kind, byte* id, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_id_validate")]
    public static partial int aster_id_validate(uint kind, byte* id);
    [LibraryImport(NativeLib, EntryPoint = "aster_secret_key_public_key")]
    public static partial int aster_secret_key_public_key(byte* secret_key, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_namespace_secret_id")]
    public static partial int aster_namespace_secret_id(byte* namespace_secret, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_secret_key_generate")]
    public static partial int aster_secret_key_generate(byte* out_key_ptr, UIntPtr out_key_capacity, UIntPtr* out_len);

    [LibraryImport(NativeLib, EntryPoint = "aster_hpke_generate_keypair")]
    public static partial int aster_hpke_generate_keypair(byte* out_private_key, byte* out_public_key);
    [LibraryImport(NativeLib, EntryPoint = "aster_hpke_public_key_from_private")]
    public static partial int aster_hpke_public_key_from_private(byte* private_key, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_hpke_x25519_secret_from_identity")]
    public static partial int aster_hpke_x25519_secret_from_identity(byte* identity_secret, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_hpke_x25519_public_from_identity")]
    public static partial int aster_hpke_x25519_public_from_identity(byte* identity_public, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_hpke_x25519_public_from_node_id")]
    public static partial int aster_hpke_x25519_public_from_node_id(Bytes node_id, byte* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_hpke_seal")]
    public static partial int aster_hpke_seal(byte* recipient_public_key, Bytes aad, Bytes plaintext, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_hpke_open")]
    public static partial int aster_hpke_open(byte* recipient_private_key, Bytes aad, Bytes envelope, Bytes* @out);

    [LibraryImport(NativeLib, EntryPoint = "aster_attest_root_node")]
    public static partial int aster_attest_root_node(byte* root_secret, byte* node_secret, AttestOptionsNative* options, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_attestation_chain_to_text")]
    public static partial int aster_attestation_chain_to_text(Bytes chain, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_attestation_chain_from_text")]
    public static partial int aster_attestation_chain_from_text(Bytes text, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_attestation_verify_chain")]
    public static partial int aster_attestation_verify_chain(Bytes chain, byte* trusted_anchors, UIntPtr anchor_count, byte* expected_node, VerifiedChainNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_verified_chain_release")]
    public static partial int aster_verified_chain_release(VerifiedChainNative* chain);
    [LibraryImport(NativeLib, EntryPoint = "aster_verified_chain_role_of")]
    public static partial int aster_verified_chain_role_of(VerifiedChainNative* chain, byte* id, uint* out_role);

    [LibraryImport(NativeLib, EntryPoint = "aster_grant_context_aad")]
    public static partial int aster_grant_context_aad(GrantContextNative* context, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_key")]
    public static partial int aster_grant_key(Bytes prefix, Bytes recipient, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_check_role")]
    public static partial int aster_grant_check_role(Bytes role, uint can_write);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_seal")]
    public static partial int aster_grant_seal(GrantContextNative* context, Bytes plaintext, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_seal_to_key")]
    public static partial int aster_grant_seal_to_key(byte* recipient_public_key, GrantContextNative* context, Bytes plaintext, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_open")]
    public static partial int aster_grant_open(byte* identity_secret, GrantContextNative* context, Bytes envelope, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_open_with_key")]
    public static partial int aster_grant_open_with_key(byte* recipient_private_key, GrantContextNative* context, Bytes envelope, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_namespace_capability_encode")]
    public static partial int aster_namespace_capability_encode(uint access, byte* key, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_namespace_capability_decode")]
    public static partial int aster_namespace_capability_decode(Bytes bytes, uint* out_access, byte* out_key);
    [LibraryImport(NativeLib, EntryPoint = "aster_namespace_grant_seal")]
    public static partial int aster_namespace_grant_seal(GrantContextNative* context, uint access, byte* key, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_namespace_grant_open")]
    public static partial int aster_namespace_grant_open(byte* identity_secret, GrantContextNative* context, Bytes envelope, uint* out_access, byte* out_key);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_record_encode")]
    public static partial int aster_grant_record_encode(GrantRecordNative* record, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_record_decode")]
    public static partial int aster_grant_record_decode(Bytes bytes, GrantRecordNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_record_release")]
    public static partial int aster_grant_record_release(GrantRecordNative* record);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_reconcile")]
    public static partial int aster_grant_reconcile(byte* identity_secret, GrantContextNative* context, Bytes* record, GrantOutcomeNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_namespace_grant_reconcile")]
    public static partial int aster_namespace_grant_reconcile(byte* identity_secret, GrantContextNative* context, Bytes* record, GrantOutcomeNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_grant_outcome_release")]
    public static partial int aster_grant_outcome_release(GrantOutcomeNative* outcome);

    // ─── helpers ────────────────────────────────────────────────────────────

    /// <summary>Throws <see cref="IrohException"/> for a non-zero status.</summary>
    public static void Check(int status, string what)
    {
        if (status != 0) throw IrohException.FromStatus(status, what);
    }
}
