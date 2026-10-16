using System;
using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations for the C ABI 7.2 – 7.5 additions (ffi/RENAME_MAP.md "New in 7.x").
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
