using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Logging;

namespace Aster.Observability;

/// <summary>
/// Path B of the observability design (3.4): the application's own log records go into the Rust
/// OpenTelemetry logs SDK (<c>aster_log_emit</c>), so they are exported over OTLP next to the Rust
/// records. Opt-in: add <see cref="AsterLogEmitExtensions.AddAsterLogEmit"/> to your logging
/// builder, or call <see cref="Emit"/> yourself. Without an OTLP logs exporter
/// (<see cref="ObservabilityOptions.OtlpLogs"/> or <c>OTEL_LOGS_EXPORTER</c>) every call is a quiet
/// no-op. It never blocks, and the Rust side emits no event for it, so an emitted record cannot be
/// read back by the log bridge.
/// </summary>
public static unsafe class AsterLogEmit
{
    /// <summary>
    /// Emits one record. <paramref name="timestampUnixNs"/> 0 = now; <paramref name="activity"/> supplies
    /// the trace ids (null = <see cref="Activity.Current"/>; none when there is no activity).
    /// </summary>
    public static void Emit(
        LogLevel level, string logger, string message,
        IEnumerable<KeyValuePair<string, string>>? fields = null,
        long timestampUnixNs = 0, Activity? activity = null)
    {
        if (level == LogLevel.None) return;
        activity ??= Activity.Current;
        byte[] kv = fields is null ? Array.Empty<byte>() : KvList.Encode(fields);
        byte[] lg = Encoding.UTF8.GetBytes(logger ?? "");
        byte[] msg = Encoding.UTF8.GetBytes(message ?? "");

        SpanContextNative cx = default;
        bool hasCx = activity is not null && activity.IdFormat == ActivityIdFormat.W3C
            && activity.TraceId != default && activity.SpanId != default;
        if (hasCx)
        {
            activity!.TraceId.CopyTo(new Span<byte>(cx.trace_id, 16));
            activity.SpanId.CopyTo(new Span<byte>(cx.span_id, 8));
            cx.trace_flags = (byte)(activity.ActivityTraceFlags & ActivityTraceFlags.Recorded);
        }

        fixed (byte* pkv = kv)
        fixed (byte* plg = lg)
        fixed (byte* pmsg = msg)
        {
            var rec = new LogEmitNative
            {
                struct_size = (uint)sizeof(LogEmitNative),
                level = Code(level),
                timestamp_unix_ns = timestampUnixNs,
                logger = new Bytes { ptr = (IntPtr)plg, len = (UIntPtr)lg.Length },
                message = new Bytes { ptr = (IntPtr)pmsg, len = (UIntPtr)msg.Length },
                fields = new Bytes { ptr = (IntPtr)pkv, len = (UIntPtr)kv.Length },
                context = hasCx ? &cx : null,
            };
            int status = Native.aster_log_emit(&rec);
            if (status != 0) throw new IrohException((Status)status, "aster_log_emit");
        }
    }

    /// <summary><see cref="LogLevel"/> to <c>aster_log_level_t</c> (1 error .. 5 trace).</summary>
    internal static byte Code(LogLevel level) => level switch
    {
        LogLevel.Trace => 5,
        LogLevel.Debug => 4,
        LogLevel.Information => 3,
        LogLevel.Warning => 2,
        _ => 1, // Error, Critical
    };
}

/// <summary>An <see cref="ILoggerProvider"/> that forwards every record to <see cref="AsterLogEmit"/>.</summary>
public sealed class AsterLogEmitProvider : ILoggerProvider
{
    /// <summary>The provider's name for <c>AddFilter</c> / configuration (<c>AsterOtlp</c>).</summary>
    public const string Alias = "AsterOtlp";

    public ILogger CreateLogger(string categoryName) => new EmitLogger(categoryName);

    public void Dispose() { }

    private sealed class EmitLogger(string category) : ILogger
    {
        public IDisposable? BeginScope<TState>(TState state) where TState : notnull => null;

        public bool IsEnabled(LogLevel logLevel) => logLevel != LogLevel.None;

        public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception,
            Func<TState, Exception?, string> formatter)
        {
            if (!IsEnabled(logLevel)) return;
            try
            {
                var fields = new List<KeyValuePair<string, string>>();
                if (state is IEnumerable<KeyValuePair<string, object?>> kvs)
                {
                    foreach (var kv in kvs)
                    {
                        // Records the log bridge delivered were exported by Rust already.
                        if (kv.Key == "aster.origin" && kv.Value as string == "rust") return;
                        if (kv.Key == "{OriginalFormat}") continue;
                        fields.Add(new(kv.Key, Convert.ToString(kv.Value, System.Globalization.CultureInfo.InvariantCulture) ?? ""));
                    }
                }
                if (exception is not null)
                {
                    fields.Add(new("exception.type", exception.GetType().FullName ?? ""));
                    fields.Add(new("exception.message", exception.Message));
                }
                if (eventId.Id != 0) fields.Add(new("event.id", eventId.Id.ToString(System.Globalization.CultureInfo.InvariantCulture)));
                fields.Add(new("code.namespace", category));
                long ts = (DateTime.UtcNow - DateTime.UnixEpoch).Ticks * 100;
                AsterLogEmit.Emit(logLevel, category, formatter(state, exception), fields, ts);
            }
            catch (Exception)
            {
                // Logging must never throw into the application.
            }
        }
    }
}

/// <summary>Opt-in registration of <see cref="AsterLogEmitProvider"/>.</summary>
public static class AsterLogEmitExtensions
{
    /// <summary>
    /// Exports the application's own <see cref="ILogger"/> records through the Rust OTel logs SDK
    /// (path B). Needs <c>ObservabilityOptions.OtlpLogs</c> (or <c>OTEL_LOGS_EXPORTER</c>); otherwise a no-op.
    /// </summary>
    public static ILoggingBuilder AddAsterLogEmit(this ILoggingBuilder builder)
    {
        builder.Services.AddSingleton<ILoggerProvider, AsterLogEmitProvider>();
        return builder;
    }
}
