using System;
using System.Runtime.InteropServices;

namespace Aster;

// Blittable mirrors of the ABI 3.1 – 3.5 structs in ffi/aster_ffi.h. Sequential layout
// reproduces the C padding on LP64; sizes are pinned by AbiContractTests (and on the
// native side by ffi/src/layout.rs).

/// <summary><c>aster_stream_header_t</c> (96 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct StreamHeaderNative
{
    public uint struct_size;
    public int version;
    public int call_id;
    public int session_id;
    public short deadline;
    public sbyte serialization_mode;
    public byte reserved0;
    public uint reserved1;
    public Bytes service;
    public Bytes method;
    public BytesList metadata_keys;
    public BytesList metadata_values;
    public IntPtr storage;
}

/// <summary><c>aster_call_header_t</c> (72 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct CallHeaderNative
{
    public uint struct_size;
    public int call_id;
    public short deadline;
    public ushort reserved0;
    public uint reserved1;
    public Bytes method;
    public BytesList metadata_keys;
    public BytesList metadata_values;
    public IntPtr storage;
}

/// <summary><c>aster_rpc_status_t</c> (64 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct RpcStatusNative
{
    public uint struct_size;
    public int code;
    public Bytes message;
    public BytesList detail_keys;
    public BytesList detail_values;
    public IntPtr storage;
}

/// <summary><c>aster_config_t</c> (288 bytes; C ABI 6.0 appended <c>blob_zstd_level</c> and <c>reuse_last_port</c>).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct ConfigNative
{
    public uint struct_size;
    public uint relay_mode;
    public BytesList relay_urls;
    public Bytes secret_key;
    public Bytes secret_key_file;
    public Bytes storage_path;
    public ulong gc_interval_ms;
    public Bytes bind_addr;
    public BytesList alpns;
    public Bytes identity_file;
    public Bytes proxy_url;
    public ulong hook_timeout_ms;
    public uint hook_failure_mode;
    public uint enable_hooks;
    public uint enable_monitoring;
    public uint local_discovery;
    public uint clear_ip_transports;
    public uint clear_relay_transports;
    public uint portmapper;
    public uint proxy_from_env;
    public uint transport_set;
    public uint transport_initial_mtu;
    public uint transport_send_fairness;
    public uint transport_enable_segmentation_offload;
    public ulong transport_max_concurrent_bidi_streams;
    public ulong transport_max_concurrent_uni_streams;
    public ulong transport_stream_receive_window;
    public ulong transport_receive_window;
    public ulong transport_send_window;
    public ulong transport_max_idle_timeout_ms;
    public ulong transport_keep_alive_interval_ms;
    public ulong transport_datagram_receive_buffer_size;
    public ulong transport_datagram_send_buffer_size;
    public IntPtr storage;
    /// <summary>0 = default (3), -1 = off, 1..=19 = level.</summary>
    public int blob_zstd_level;
    /// <summary><c>aster_reuse_last_port_t</c>: 0 = default (on), 1 = enabled, 2 = disabled.</summary>
    public uint reuse_last_port;
}

/// <summary><c>aster_grant_context_t</c> (104 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct GrantContextNative
{
    public uint struct_size;
    public uint reserved;
    public Bytes app;
    public Bytes granter;
    public Bytes recipient;
    public Bytes resource;
    public Bytes path;
    public Bytes role;
}

/// <summary><c>aster_grant_record_t</c> (72 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct GrantRecordNative
{
    public uint struct_size;
    public uint state;
    public int context_version;
    public uint reserved;
    public Bytes role;
    public Bytes envelope;
    public Bytes reason;
    public IntPtr storage;
}

/// <summary><c>aster_grant_outcome_t</c> (72 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct GrantOutcomeNative
{
    public uint struct_size;
    public uint kind;
    public uint access;
    public uint drops_capability;
    public Bytes payload;
    public Bytes reason;
    public Bytes role;
    public IntPtr storage;
}

/// <summary><c>aster_path_info_t</c> (72 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct PathInfoNative
{
    public uint struct_size;
    public uint path_kind;
    public uint is_selected;
    public ushort peer_port;
    public ushort _pad;
    public Bytes peer_host;
    public Bytes relay_url;
    public Bytes other;
    public ulong rtt_micros;
}

/// <summary><c>aster_path_list_t</c> (32 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct PathListNative
{
    public uint struct_size;
    public uint _pad;
    public IntPtr items;
    public UIntPtr len;
    public IntPtr storage;
}

/// <summary>
/// <c>aster_dispatch_event_t</c> (104 bytes, ABI 3.5): one event from <c>aster_server_poll</c>.
/// <c>request</c> and <c>peer</c> live in one lease, <c>buffer</c>.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
internal struct DispatchEventNative
{
    public byte kind;
    public byte pattern;
    public byte reason;
    public byte _p0;
    public uint flags;
    public ulong call_id;
    public uint service_idx;
    public uint method_idx;
    public ulong session_key;
    public ulong connection_id;
    public ulong connection_handle;
    public long deadline_unix_ms;
    public int wire_call_id;
    public uint session_id;
    public IntPtr request_ptr;
    public uint request_len;
    public uint _p2;
    public IntPtr peer_ptr;
    public uint peer_len;
    public uint _p3;
    public ulong buffer;
}

/// <summary><c>aster_call_options_t</c> (72 bytes, ABI 3.5): per-call options of the payload client.</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct CallOptionsNative
{
    public uint struct_size;
    public int version;
    public Bytes service;
    public Bytes method;
    public Bytes metadata;
    public uint session_id;
    public uint deadline_ms;
    public int serialization_mode;
    public int wire_call_id;
}
