using System;
using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations for the C ABI 7.2 – 7.7 additions (ffi/RENAME_MAP.md "New in 7.x").
// 7.1 (audit acknowledgements) lives in Native.Audit.cs. Struct sizes are pinned by AbiContractTests.
internal static unsafe partial class Native
{
    // ─── 7.2: server drain ──────────────────────────────────────────────────

    /// <summary><c>ASTER_DRAIN_CLOSE_CODE</c>: the QUIC close code of a draining server.</summary>
    public const ulong DrainCloseCode = 503;

    [LibraryImport(NativeLib, EntryPoint = "aster_server_drain")]
    public static partial int aster_server_drain(ulong runtime, ulong server, uint grace_ms, DrainReportNative* out_report);

    // ─── 7.3 / 7.4: network and metrics settings beside aster_config_t ──────

    [LibraryImport(NativeLib, EntryPoint = "aster_config_set_network")]
    public static partial int aster_config_set_network(ConfigNative* config, NetworkConfigNative* network);
    [LibraryImport(NativeLib, EntryPoint = "aster_config_network")]
    public static partial int aster_config_network(ConfigNative* config, NetworkConfigNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_config_set_metrics")]
    public static partial int aster_config_set_metrics(ConfigNative* config, MetricsConfigNative* metrics);
    [LibraryImport(NativeLib, EntryPoint = "aster_config_metrics")]
    public static partial int aster_config_metrics(ConfigNative* config, MetricsConfigNative* @out);

    /// <summary><c>ASTER_METRICS_PEER_LABEL_CAP_DEFAULT</c>.</summary>
    public const uint MetricsPeerLabelCapDefault = 64;

    // ─── 7.4: transfer metrics ──────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_blobs_transfer_stats")]
    public static partial int aster_blobs_transfer_stats(ulong runtime, ulong node, BlobTransferStatsNative* @out);

    /// <summary><c>ASTER_FETCH_REPORT_V2</c>: fetch flag selecting fetch report v2.</summary>
    public const uint FetchReportV2 = 2;

    // ─── 7.5: dispatch identity ─────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_identity")]
    public static partial int aster_dispatch_identity(ulong runtime, ulong server, ulong call_id, DispatchIdentityNative* @out);

    // ─── 7.6 / 7.7: gateway identity ────────────────────────────────────────

    /// <summary><c>ASTER_DISPATCH_IDENTITY_ACTOR</c> (7.6): <c>actor_ptr</c> is the forwarding gateway.</summary>
    public const uint DispatchIdentityActor = 2;

    /// <summary><c>ASTER_IDENTITY_SET_*</c>: which limits of <see cref="IdentityConfigNative"/> are set.</summary>
    public const uint IdentitySetEdgeTokenMaxLifetime = 1, IdentitySetClockLeeway = 2, IdentitySetMaxKeySetAge = 4;

    /// <summary><c>aster_node_config_method_t</c>.</summary>
    public const uint NodeConfigGet = 0, NodeConfigSet = 1, NodeConfigClear = 2, NodeConfigAck = 3;

    [LibraryImport(NativeLib, EntryPoint = "aster_config_set_identity")]
    public static partial int aster_config_set_identity(ConfigNative* config, IdentityConfigNative* identity);
    [LibraryImport(NativeLib, EntryPoint = "aster_config_identity")]
    public static partial int aster_config_identity(ConfigNative* config, IdentityConfigNative* @out);

    [LibraryImport(NativeLib, EntryPoint = "aster_server_set_audit_outbox")]
    public static partial int aster_server_set_audit_outbox(ulong runtime, ulong builder, ulong outbox);

    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_on_behalf_of")]
    public static partial int aster_dispatch_on_behalf_of(ulong runtime, ulong server, ulong call_id, byte** out_ptr, uint* out_len,
        ulong* out_buffer);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_node_properties")]
    public static partial int aster_server_node_properties(ulong server, Bytes* out_json);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_node_config_generation")]
    public static partial ulong aster_server_node_config_generation(ulong server);

    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_mint_user_token")]
    public static partial int aster_enrollment_mint_user_token(ulong runtime, ulong enrollment, Bytes request_json, Bytes* out_token);
    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_publisher_notice")]
    public static partial int aster_enrollment_publisher_notice(ulong runtime, ulong enrollment, Bytes iss, Bytes kid);
    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_publisher_notice_token")]
    public static partial int aster_enrollment_publisher_notice_token(ulong runtime, ulong enrollment, Bytes token);
    [LibraryImport(NativeLib, EntryPoint = "aster_enrollment_publisher_status")]
    public static partial int aster_enrollment_publisher_status(ulong runtime, ulong enrollment, Bytes* out_json);

    [LibraryImport(NativeLib, EntryPoint = "aster_node_config_call")]
    public static partial int aster_node_config_call(ulong runtime, ulong connection, uint method, Bytes request_json, ulong user_data,
        out ulong out_operation);
}

/// <summary><c>aster_drain_report_t</c> (64 bytes, ABI 7.2).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct DrainReportNative
{
    public ulong calls_in_flight;
    public ulong calls_completed;
    public ulong calls_cancelled;
    public ulong calls_refused;
    public ulong connections_closed;
    public ulong connections_refused;
    public ulong elapsed_ms;
    public uint deadline_reached;
    public uint reserved;
}

/// <summary><c>aster_network_config_t</c> (96 bytes, ABI 7.3).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct NetworkConfigNative
{
    public uint struct_size;
    public uint discovery;
    public Bytes bind_addr_v6;
    public BytesList advertise_addrs;
    public BytesList relay_ca_certs;
    public uint relay_ca_only;
    public uint reserved;
    public Bytes discovery_dns_origin;
    public BytesList discovery_pkarr_relays;
}

/// <summary><c>aster_metrics_config_t</c> (16 bytes, ABI 7.4).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct MetricsConfigNative
{
    public uint struct_size;
    public uint peer_labels;
    public uint peer_label_cap;
    public uint reserved;
}

/// <summary><c>aster_blob_transfer_stats_t</c> (168 bytes, ABI 7.4).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct BlobTransferStatsNative
{
    public uint struct_size;
    public uint reserved;
    public ulong fetched_payload_bytes;
    public ulong fetched_zstd_payload_bytes;
    public ulong fetched_wire_bytes;
    public ulong fetched_plain_bytes;
    public ulong served_payload_bytes;
    public ulong served_zstd_payload_bytes;
    public ulong served_wire_bytes;
    public ulong served_plain_bytes;
    public ulong plain_fallbacks;
    public ulong plain_serves;
    public ulong fetch_plain_ok;
    public ulong fetch_plain_failed;
    public ulong fetch_zstd_ok;
    public ulong fetch_zstd_failed;
    public ulong serve_plain_ok;
    public ulong serve_plain_failed;
    public ulong serve_zstd_ok;
    public ulong serve_zstd_failed;
    public ulong fetch_decode_errors;
    public ulong serve_decode_errors;
}

/// <summary><c>aster_dispatch_identity_t</c> (64 bytes, ABI 7.5): who a live call is from.</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct DispatchIdentityNative
{
    public uint struct_size;
    public uint flags;
    public IntPtr peer_ptr;
    public uint peer_len;
    public uint _p0;
    public IntPtr principal_ptr;
    public uint principal_len;
    public uint _p1;
    public IntPtr actor_ptr;
    public uint actor_len;
    public uint reserved;
    public ulong buffer;
}

/// <summary>
/// <c>aster_identity_config_t</c> (96 bytes, ABI 7.7): <c>[identity]</c>, <c>[identity.publisher]</c> and
/// <c>[remote_config]</c> beside <c>aster_config_t</c>.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
internal unsafe struct IdentityConfigNative
{
    public uint struct_size;
    public uint set;
    public uint accept_on_behalf_of;
    public uint remote_config_disabled;
    public ulong edge_token_max_lifetime_secs;
    public ulong clock_leeway_secs;
    public ulong max_key_set_age_secs;
    public Bytes trusted_issuers_json;
    public Bytes publisher_json;
    public BytesList remote_config_lock;
    /// <summary><c>aster_config_source_t</c> per <c>ASTER_IDENTITY_SETTING_*</c> index.</summary>
    public fixed byte sources[8];
}
