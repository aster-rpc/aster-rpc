using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using System.Threading;

namespace Aster.Observability;

/// <summary>
/// Options of <see cref="AsterObservability.Init"/>. Precedence, highest first: these fields, then
/// <c>OTEL_*</c> / <c>ASTER_*</c> environment variables (<see cref="FromEnv"/>), then the
/// <see cref="ConfigToml"/> table, then defaults.
/// </summary>
public sealed record ObservabilityOptions
{
    /// <summary><c>service.name</c> resource attribute.</summary>
    public string? ServiceName { get; init; }

    /// <summary>OTLP/HTTP collector base URL (<c>/v1/&lt;signal&gt;</c> is appended).</summary>
    public string? OtlpEndpoint { get; init; }

    /// <summary>OTLP request headers (often a bearer token). Never logged.</summary>
    public IReadOnlyDictionary<string, string>? OtlpHeaders { get; init; }

    /// <summary>Extra resource attributes.</summary>
    public IReadOnlyDictionary<string, string>? ResourceAttributes { get; init; }

    /// <summary>
    /// <c>true</c> exports that signal over OTLP; <c>null</c> (or <c>false</c>) lets
    /// <c>OTEL_*</c> decide when <see cref="FromEnv"/> is set and leaves it off otherwise. The C ABI
    /// cannot force a signal off against the environment.
    /// </summary>
    public bool? Traces { get; init; }

    /// <inheritdoc cref="Traces"/>
    public bool? Metrics { get; init; }

    /// <inheritdoc cref="Traces"/>
    public bool? OtlpLogs { get; init; }

    /// <summary>Parent-based trace-id ratio sampler in [0, 1]; null = the sampler from env / default.</summary>
    public double? SampleRatio { get; init; }

    /// <summary>
    /// <c>EnvFilter</c> directives for Rust events (<c>ASTER_LOG</c>), e.g. <c>warn,aster=debug</c>.
    /// Pins the Rust filter; without it Rust follows the logger factory (<see cref="AsterLogging.SyncLevel"/>).
    /// </summary>
    public string? LogFilter { get; init; }

    /// <summary>Bound of the Rust log queue (0 = default, 8192 records).</summary>
    public uint LogQueue { get; init; }

    /// <summary>Deliver Rust log records to <c>ILogger</c> (default true).</summary>
    public bool LogBridge { get; init; } = true;

    /// <summary>Register the <c>Activity</c> bridge for source <c>Aster</c> (default true).</summary>
    public bool ActivityBridge { get; init; } = true;

    /// <summary>Apply <c>OTEL_*</c> / <c>ASTER_*</c> under the explicit fields (default true).</summary>
    public bool FromEnv { get; init; } = true;

    /// <summary>The <c>[observability]</c> TOML table (header optional), used below the environment.</summary>
    public string? ConfigToml { get; init; }
}

/// <summary>
/// The observability pipeline over the Rust core (<c>aster-otel</c>): OpenTelemetry SDK,
/// exporters, the <c>tracing</c> subscriber, the Prometheus renderer and the log bridge. This class
/// starts and stops it, delivers Rust log records to <c>ILogger</c> (<see cref="AsterLogging"/>) and
/// mirrors <see cref="System.Diagnostics.Activity"/>s of source <c>Aster</c> into it
/// (<see cref="Tracing"/>).
/// <para>
/// Without a call, the first <see cref="AsterServer"/> / <see cref="AsterClient"/> start runs
/// <see cref="EnsureStarted"/>: the log bridge and the <c>Activity</c> bridge start, and exporters
/// only when <c>OTEL_*</c> asks for them. To configure the pipeline, call <see cref="Init"/> first;
/// a configuration that differs from the running pipeline's throws an <see cref="IrohException"/>
/// with <see cref="Status.AlreadyExists"/> (the SDK's globals are per process).
/// </para>
/// </summary>
public static class AsterObservability
{
    private static readonly object s_lock = new();
    private static int s_refs;
    private static bool s_auto;

    /// <summary>Whether this binding holds a reference to the running pipeline.</summary>
    public static bool IsInitialized => Volatile.Read(ref s_refs) > 0;

    /// <summary>Starts the pipeline, or joins the running one (see the class remarks).</summary>
    public static unsafe void Init(ObservabilityOptions? options = null)
    {
        options ??= new ObservabilityOptions();
        lock (s_lock)
        {
            string? logFilter = options.LogFilter;
            bool pinned = logFilter is not null;
            if (options.LogBridge && !pinned)
            {
                logFilter = AsterLogging.InitialFilter();
                pinned = logFilter is null; // ASTER_LOG / ASTER_LOG_LEVEL in the environment
            }

            using var scope = new NativeScope();
            uint flags = 0;
            if (options.FromEnv) flags |= Native.ObservabilityFromEnv;
            if (options.Traces == true) flags |= Native.ObservabilityTraces;
            if (options.Metrics == true) flags |= Native.ObservabilityMetrics;
            if (options.OtlpLogs == true) flags |= Native.ObservabilityOtlpLogs;
            if (options.LogBridge) flags |= Native.ObservabilityLogBridge;

            var cfg = new ObservabilityConfigNative
            {
                struct_size = (uint)sizeof(ObservabilityConfigNative),
                flags = flags,
                service_name = scope.Utf8(options.ServiceName),
                otlp_endpoint = scope.Utf8(options.OtlpEndpoint),
                otlp_headers = scope.Bytes(Pairs(options.OtlpHeaders)),
                resource_attrs = scope.Bytes(Pairs(options.ResourceAttributes)),
                log_filter = scope.Utf8(logFilter),
                extra_toml = scope.Utf8(options.ConfigToml),
                trace_sample_ratio = options.SampleRatio ?? -1.0,
                protocol = 0,
                log_queue_capacity = options.LogQueue,
                binding = Native.BindingDotnet,
            };
            Native.Check(Native.aster_observability_init(&cfg), "aster_observability_init");
            s_refs++;

            if (options.LogBridge)
            {
                LogBridge.SetPinned(pinned);
                LogBridge.Start();
            }
            if (options.ActivityBridge) ActivityBridge.Start();
        }
    }

    /// <summary>
    /// The implicit start (called when a server or client starts): opens the log bridge and the
    /// <c>Activity</c> bridge with an environment-only configuration. Exporters start only when
    /// <c>OTEL_*</c> asks for them. A no-op after any <see cref="Init"/>.
    /// </summary>
    public static void EnsureStarted()
    {
        lock (s_lock)
        {
            if (s_auto || IsInitialized) return;
            s_auto = true;
        }
        try
        {
            // Without FromEnv the signal flags decide alone, so the environment is applied only
            // when it asks for an exporter (an unconfigured process must not push to localhost).
            Init(new ObservabilityOptions { FromEnv = EnvWantsExport() });
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"aster: observability could not start: {ex.Message}");
        }
    }

    private static bool EnvWantsExport()
    {
        static string? Get(string k) => Environment.GetEnvironmentVariable(k);
        foreach (var signal in new[] { "TRACES", "METRICS", "LOGS" })
        {
            string? exporter = Get($"OTEL_{signal}_EXPORTER")?.Trim().ToLowerInvariant();
            if (!string.IsNullOrEmpty(exporter)) { if (exporter != "none") return true; continue; }
            if (!string.IsNullOrEmpty(Get($"OTEL_EXPORTER_OTLP_{signal}_ENDPOINT"))) return true;
        }
        return !string.IsNullOrEmpty(Get("OTEL_EXPORTER_OTLP_ENDPOINT"));
    }

    /// <summary>Flushes every signal, waiting at most <paramref name="timeout"/> (default 2 s).</summary>
    public static void Flush(TimeSpan? timeout = null)
        => Native.Check(Native.aster_observability_flush(Millis(timeout ?? TimeSpan.FromSeconds(2))), "aster_observability_flush");

    /// <summary>
    /// Releases every reference this process holds on the pipeline: the log poll thread and the
    /// <c>Activity</c> listener stop, providers flush and stop (unless an ops listener still holds the
    /// pipeline), spans end and metric instruments are freed (<see cref="Metrics"/> objects declare
    /// themselves again on next use). Returns whether anything was held. A later <see cref="Init"/>
    /// starts a new pipeline.
    /// </summary>
    public static bool Shutdown(TimeSpan? timeout = null)
    {
        uint ms = Millis(timeout ?? TimeSpan.FromSeconds(2));
        bool held = false;
        lock (s_lock)
        {
            ActivityBridge.Stop();
            LogBridge.Stop(TimeSpan.FromSeconds(1));
            while (Native.aster_observability_shutdown(ms) == 0) held = true;
            s_refs = 0;
            s_auto = false;
        }
        return held;
    }

    /// <summary>
    /// The Prometheus text of the running pipeline (what <c>/metrics</c> serves), or null while no
    /// <see cref="Aster.Health.HealthServer"/> has given the pipeline a pull reader.
    /// </summary>
    public static unsafe string? MetricsText()
    {
        Bytes output = default;
        int r = Native.aster_metrics_prometheus(&output);
        if (r == (int)Status.NotFound) return null;
        Native.Check(r, "aster_metrics_prometheus");
        return Encoding.UTF8.GetString(NativeScope.TakeOwned(ref output));
    }

    private static uint Millis(TimeSpan t) => (uint)Math.Clamp(t.TotalMilliseconds, 0, uint.MaxValue - 1);

    private static byte[] Pairs(IReadOnlyDictionary<string, string>? map)
        => map is null || map.Count == 0 ? Array.Empty<byte>() : KvList.Encode(map);

    /// <summary>Formats an attribute value the way every binding does (strings only on the wire).</summary>
    internal static string Str(object? v) => v switch
    {
        null => "",
        string s => s,
        bool b => b ? "true" : "false",
        IFormattable f => f.ToString(null, CultureInfo.InvariantCulture),
        _ => v.ToString() ?? "",
    };
}
