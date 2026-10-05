using System;
using System.Runtime.InteropServices;

namespace Aster;

// P/Invoke declarations and struct mirrors for the C ABI 4.6 observability surface
// (ffi/RENAME_MAP.md "New in 4.6"; design docs/_internal/bindings/OBSERVABILITY_HEALTH_DESIGN.md §7):
// the pipeline (aster_observability_*), the log bridge (aster_log_*), metrics (aster_instrument_*,
// aster_attrset_*, aster_metric_*), spans (aster_span_*, aster_trace_context_*), the ops
// listener (aster_health_*) and the dispatch trace context. Sizes are pinned by AbiContractTests.

/// <summary><c>aster_observability_config_t</c> (128 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct ObservabilityConfigNative
{
    public uint struct_size;
    public uint flags;
    public Bytes service_name;
    public Bytes otlp_endpoint;
    public Bytes otlp_headers;
    public Bytes resource_attrs;
    public Bytes log_filter;
    public Bytes extra_toml;
    public double trace_sample_ratio;
    public uint protocol;
    public uint log_queue_capacity;
    public uint binding;
    public uint _reserved;
}

/// <summary><c>aster_log_record_t</c> (96 bytes). Pointers are valid until the next poll.</summary>
[StructLayout(LayoutKind.Sequential)]
internal unsafe struct LogRecordNative
{
    public byte level;
    public byte flags;
    public ushort _p0;
    public uint _p1;
    public long timestamp_unix_ns;
    public fixed byte trace_id[16];
    public fixed byte span_id[8];
    public ulong call_id;
    public byte* target_ptr;
    public uint target_len;
    public uint _p2;
    public byte* message_ptr;
    public uint message_len;
    public uint _p3;
    public byte* fields_ptr;
    public uint fields_len;
    public uint _p4;
}

/// <summary><c>aster_span_context_t</c> (32 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal unsafe struct SpanContextNative
{
    public fixed byte trace_id[16];
    public fixed byte span_id[8];
    public byte trace_flags;
    public byte is_remote;
    public ushort _p0;
    public uint _p1;
}

/// <summary><c>aster_log_emit_t</c> (72 bytes): an application log record for <c>aster_log_emit</c>.</summary>
[StructLayout(LayoutKind.Sequential)]
internal unsafe struct LogEmitNative
{
    public uint struct_size;
    public byte level;
    public byte _p0;
    public byte _p1;
    public byte _p2;
    public long timestamp_unix_ns;
    public Bytes logger;
    public Bytes message;
    public Bytes fields;
    public SpanContextNative* context;
}

/// <summary><c>aster_span_record_t</c> (152 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal unsafe struct SpanRecordNative
{
    public SpanContextNative context;
    public SpanContextNative parent;
    public ulong start_unix_nanos;
    public ulong end_unix_nanos;
    public uint kind;
    public uint status_code;
    public Bytes name;
    public Bytes status_message;
    public byte* kv;
    public uint kv_len;
    public uint _p0;
    public byte* events;
    public uint events_len;
    public uint _p1;
}

/// <summary><c>aster_instrument_desc_t</c> (96 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal unsafe struct InstrumentDescNative
{
    public uint struct_size;
    public uint kind;
    public Bytes name;
    public Bytes description;
    public Bytes unit;
    public Bytes auto_attrs;
    public uint cardinality_limit;
    public uint _p;
    public double* boundaries;
    public uint boundaries_len;
    public uint _p1;
}

/// <summary><c>aster_health_config_t</c> (24 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct HealthConfigNative
{
    public uint struct_size;
    public ushort port;
    public ushort _p;
    public Bytes host;
}

internal static unsafe partial class Native
{
    // ─── Pipeline ───────────────────────────────────────────────────────────

    public const uint ObservabilityFromEnv = 1, ObservabilityTraces = 2, ObservabilityMetrics = 4,
        ObservabilityOtlpLogs = 8, ObservabilityLogBridge = 16;

    /// <summary><c>ASTER_BINDING_DOTNET</c>.</summary>
    public const uint BindingDotnet = 5;

    /// <summary><c>ASTER_DISPATCH_FLAG_HAS_TRACE</c> / <c>ASTER_DISPATCH_FLAG_HAS_INBOUND_TRACE</c>.</summary>
    public const uint DispatchFlagHasTrace = 1024, DispatchFlagHasInboundTrace = 2048;

    public const byte LogFlagHasTrace = 1, LogFlagInCall = 2, LogFlagSynthetic = 4, LogFlagAudit = 8;

    [LibraryImport(NativeLib, EntryPoint = "aster_observability_init")]
    public static partial int aster_observability_init(ObservabilityConfigNative* cfg);
    [LibraryImport(NativeLib, EntryPoint = "aster_observability_flush")]
    public static partial int aster_observability_flush(uint timeout_ms);
    [LibraryImport(NativeLib, EntryPoint = "aster_observability_shutdown")]
    public static partial int aster_observability_shutdown(uint timeout_ms);

    // ─── Logs ───────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_log_poll")]
    public static partial int aster_log_poll(LogRecordNative* @out, uint record_size, uint max, uint timeout_ms, uint* out_count);
    [LibraryImport(NativeLib, EntryPoint = "aster_log_close")]
    public static partial int aster_log_close();
    [LibraryImport(NativeLib, EntryPoint = "aster_log_dropped")]
    public static partial int aster_log_dropped(ulong* out_total);
    [LibraryImport(NativeLib, EntryPoint = "aster_log_set_filter")]
    public static partial int aster_log_set_filter(byte* directives, uint len);
    [LibraryImport(NativeLib, EntryPoint = "aster_log_emit")]
    public static partial int aster_log_emit(LogEmitNative* rec);

    // ─── Metrics ────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_instrument_new")]
    public static partial int aster_instrument_new(InstrumentDescNative* d, ulong* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_instrument_free")]
    public static partial int aster_instrument_free(ulong h);
    [LibraryImport(NativeLib, EntryPoint = "aster_attrset_new")]
    public static partial int aster_attrset_new(byte* kv, uint kv_len, ulong* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_attrset_free")]
    public static partial int aster_attrset_free(ulong h);
    [LibraryImport(NativeLib, EntryPoint = "aster_metric_add_u64")]
    public static partial int aster_metric_add_u64(ulong h, ulong v, ulong set, byte* kv, uint kv_len, ulong call_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_metric_add_i64")]
    public static partial int aster_metric_add_i64(ulong h, long v, ulong set, byte* kv, uint kv_len, ulong call_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_metric_record_f64")]
    public static partial int aster_metric_record_f64(ulong h, double v, ulong set, byte* kv, uint kv_len, ulong call_id);
    [LibraryImport(NativeLib, EntryPoint = "aster_metrics_prometheus")]
    public static partial int aster_metrics_prometheus(Bytes* @out);

    // ─── Spans and trace context ────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_span_record")]
    public static partial int aster_span_record(SpanRecordNative* rec);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_trace_context")]
    public static partial int aster_dispatch_trace_context(ulong runtime, ulong server, ulong call_id, SpanContextNative* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_dispatch_inbound_trace")]
    public static partial int aster_dispatch_inbound_trace(ulong runtime, ulong server, ulong call_id,
        byte** out_ptr, uint* out_len, ulong* out_buffer);

    // ─── Health ─────────────────────────────────────────────────────────────

    [LibraryImport(NativeLib, EntryPoint = "aster_health_start")]
    public static partial int aster_health_start(ulong runtime, ulong node, HealthConfigNative* cfg, ulong* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_server_attach_health")]
    public static partial int aster_server_attach_health(ulong builder, ulong health);
    [LibraryImport(NativeLib, EntryPoint = "aster_health_set_ready")]
    public static partial int aster_health_set_ready(ulong h, [MarshalAs(UnmanagedType.U1)] bool ready);
    [LibraryImport(NativeLib, EntryPoint = "aster_health_set_healthy")]
    public static partial int aster_health_set_healthy(ulong h, [MarshalAs(UnmanagedType.U1)] bool healthy);
    [LibraryImport(NativeLib, EntryPoint = "aster_health_check_set")]
    public static partial int aster_health_check_set(ulong h, Bytes name, [MarshalAs(UnmanagedType.U1)] bool ok);
    [LibraryImport(NativeLib, EntryPoint = "aster_health_addr")]
    public static partial int aster_health_addr(ulong h, Bytes* @out);
    [LibraryImport(NativeLib, EntryPoint = "aster_health_stop")]
    public static partial int aster_health_stop(ulong runtime, ulong h);
}
