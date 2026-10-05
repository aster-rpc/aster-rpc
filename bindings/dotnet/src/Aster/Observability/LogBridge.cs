using System;
using System.Collections;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Microsoft.Extensions.Logging;

namespace Aster.Observability;

/// <summary>
/// Where Rust log records go. The Rust core emits <c>tracing</c> events (connection, admission,
/// dispatch, ...) into a bounded, drop-counting queue; a dedicated background thread
/// (<c>aster-log-poll</c>) takes them with <c>aster_log_poll</c> and logs each one on an
/// <see cref="ILogger"/> whose category is the Rust target (<c>aster::rpc::dispatch</c> becomes
/// <c>aster.rpc.dispatch</c>) after an <c>IsEnabled</c> check.
/// <para>
/// Each record is logged inside a scope with <c>TraceId</c> / <c>SpanId</c> (hex ids of the enclosing
/// exported span, the same keys <c>ActivityTrackingOptions</c> uses), <c>aster.call_id</c> and
/// <c>aster.audit</c> when they apply. The state is the event's structured fields (plus
/// <c>rpc.service</c>, <c>rpc.method</c>, <c>aster.peer</c> and <c>thread.name</c> of the enclosing
/// call), <c>aster.origin=rust</c> (an OTel log appender skips these: Rust already exported them) and
/// <c>{OriginalFormat}</c>.
/// </para>
/// <para>
/// Level sync: Rust filters by level before it allocates anything, so its filter must follow the
/// logger. <see cref="SyncLevel"/> copies the lowest level the factory enables for the <c>aster</c>
/// category to Rust; call it after changing the logging configuration (it is also called by
/// <see cref="UseLoggerFactory"/>). The poll thread re-checks every 5 s and calls it when the
/// effective level changed, which catches filters changed in place. An explicit
/// <see cref="ObservabilityOptions.LogFilter"/> or <c>ASTER_LOG</c> pins the Rust filter.
/// </para>
/// Without a factory, records at Warning and above go to standard error.
/// </summary>
public static class AsterLogging
{
    private static volatile ILoggerFactory? s_factory;
    private static readonly ConcurrentDictionary<string, ILogger> s_loggers = new(StringComparer.Ordinal);

    /// <summary>Sets the factory Rust records are logged through (null = standard error, Warning and above).</summary>
    public static void UseLoggerFactory(ILoggerFactory? factory)
    {
        s_factory = factory;
        s_loggers.Clear();
        if (LogBridge.IsRunning) SyncLevel();
    }

    /// <summary>
    /// Makes Rust filter by the factory's effective level for the <c>aster</c> category now. Returns
    /// the directives applied, or null when the log bridge is not open.
    /// </summary>
    public static string? SyncLevel() => LogBridge.SyncLevel();

    /// <summary>The Rust filter directives matching the factory, or null when <c>ASTER_LOG</c> already says.</summary>
    internal static string? InitialFilter()
        => string.IsNullOrEmpty(Environment.GetEnvironmentVariable("ASTER_LOG"))
           && string.IsNullOrEmpty(Environment.GetEnvironmentVariable("ASTER_LOG_LEVEL"))
            ? Directive(EffectiveLevel())
            : null;

    /// <summary>The lowest level the factory enables for <c>aster</c> (None = all disabled).</summary>
    internal static LogLevel EffectiveLevel()
    {
        var factory = s_factory;
        if (factory is null) return LogLevel.Warning;
        ILogger logger = factory.CreateLogger("aster");
        foreach (var level in new[] { LogLevel.Trace, LogLevel.Debug, LogLevel.Information, LogLevel.Warning, LogLevel.Error })
            if (logger.IsEnabled(level)) return level;
        return LogLevel.None;
    }

    internal static string Directive(LogLevel level) => "warn,aster=" + level switch
    {
        LogLevel.Trace => "trace",
        LogLevel.Debug => "debug",
        LogLevel.Information => "info",
        LogLevel.Warning => "warn",
        LogLevel.Error or LogLevel.Critical => "error",
        _ => "off",
    };

    internal static ILogger? LoggerFor(string category)
    {
        var factory = s_factory;
        if (factory is null) return null;
        return s_loggers.GetOrAdd(category, static (c, f) => f.CreateLogger(c), factory);
    }
}

/// <summary>The poll thread behind <see cref="AsterLogging"/>.</summary>
internal static unsafe class LogBridge
{
    private const int BatchSize = 256;
    private const uint PollTimeoutMs = 250;
    private static readonly TimeSpan s_recheck = TimeSpan.FromSeconds(5);

    private static readonly object s_lock = new();
    private static Thread? s_thread;
    private static volatile bool s_stop;
    private static volatile bool s_pinned;
    private static LogLevel s_lastLevel = LogLevel.None;

    public static bool IsRunning => s_thread is { IsAlive: true };

    public static void SetPinned(bool pinned) => s_pinned = pinned;

    public static void Start()
    {
        lock (s_lock)
        {
            if (IsRunning) return;
            s_stop = false;
            s_lastLevel = AsterLogging.EffectiveLevel();
            s_thread = new Thread(Run) { Name = "aster-log-poll", IsBackground = true };
            s_thread.Start();
        }
    }

    /// <summary>Closes the Rust queue (waking the poller) and joins the thread.</summary>
    public static void Stop(TimeSpan timeout)
    {
        Thread? t;
        lock (s_lock)
        {
            t = s_thread;
            s_thread = null;
            s_stop = true;
        }
        if (t is null) return;
        Native.aster_log_close();
        if (!t.Join(timeout))
            Console.Error.WriteLine("aster: the log poll thread did not stop in time");
    }

    public static string? SyncLevel()
    {
        LogLevel level = AsterLogging.EffectiveLevel();
        string directive = AsterLogging.Directive(level);
        byte[] bytes = Encoding.UTF8.GetBytes(directive);
        int r;
        fixed (byte* p = bytes)
            r = Native.aster_log_set_filter(p, (uint)bytes.Length);
        if (r != 0) return null; // no bridge open (NOT_FOUND)
        s_lastLevel = level;
        return directive;
    }

    private static void Run()
    {
        var buffer = (LogRecordNative*)NativeMemory.AlignedAlloc((nuint)(BatchSize * sizeof(LogRecordNative)), 8);
        long lastCheck = Stopwatch.GetTimestamp();
        try
        {
            while (!s_stop)
            {
                long begin = Stopwatch.GetTimestamp();
                uint n = Native.aster_log_poll(buffer, (uint)sizeof(LogRecordNative), BatchSize, PollTimeoutMs);
                for (uint i = 0; i < n; i++)
                {
                    try { Deliver(&buffer[i]); }
                    catch (Exception) { /* one bad record must not stop the batch */ }
                }
                if (!s_pinned && Stopwatch.GetElapsedTime(lastCheck) >= s_recheck)
                {
                    lastCheck = Stopwatch.GetTimestamp();
                    if (AsterLogging.EffectiveLevel() != s_lastLevel) SyncLevel();
                }
                // A closed or foreign-closed bridge returns at once: do not spin.
                if (n == 0 && Stopwatch.GetElapsedTime(begin) < TimeSpan.FromMilliseconds(20))
                    Thread.Sleep(50);
            }
        }
        finally
        {
            NativeMemory.AlignedFree(buffer);
        }
    }

    private static LogLevel Map(byte level) => level switch
    {
        1 => LogLevel.Error,
        2 => LogLevel.Warning,
        3 => LogLevel.Information,
        4 => LogLevel.Debug,
        _ => LogLevel.Trace,
    };

    private static string Utf8(byte* p, uint len) => len == 0 ? "" : Encoding.UTF8.GetString(p, (int)len);

    private static string Category(byte* p, uint len)
    {
        string target = Utf8(p, len);
        return target.Replace("::", ".", StringComparison.Ordinal);
    }

    private static string Hex(byte* p, int n)
    {
        Span<char> chars = stackalloc char[n * 2];
        const string digits = "0123456789abcdef";
        for (int i = 0; i < n; i++)
        {
            chars[2 * i] = digits[p[i] >> 4];
            chars[2 * i + 1] = digits[p[i] & 0xf];
        }
        return new string(chars);
    }

    private static void Deliver(LogRecordNative* r)
    {
        LogLevel level = Map(r->level);
        string category = Category(r->target_ptr, r->target_len);
        ILogger? logger = AsterLogging.LoggerFor(category);
        if (logger is null)
        {
            if (level >= LogLevel.Warning)
                Console.Error.WriteLine($"{level} {category}: {Utf8(r->message_ptr, r->message_len)}");
            return;
        }
        if (!logger.IsEnabled(level)) return;

        string message = Utf8(r->message_ptr, r->message_len);
        var fields = r->fields_len == 0
            ? Array.Empty<KeyValuePair<string, string>>()
            : KvList.Decode(new ReadOnlySpan<byte>(r->fields_ptr, (int)r->fields_len));
        var state = new RecordState(message, fields);

        Dictionary<string, object?>? scope = null;
        if ((r->flags & Native.LogFlagHasTrace) != 0 || (r->flags & (Native.LogFlagInCall | Native.LogFlagAudit)) != 0)
        {
            scope = new Dictionary<string, object?>(4);
            if ((r->flags & Native.LogFlagHasTrace) != 0)
            {
                scope["TraceId"] = Hex(r->trace_id, 16);
                scope["SpanId"] = Hex(r->span_id, 8);
            }
            if ((r->flags & Native.LogFlagInCall) != 0) scope["aster.call_id"] = r->call_id;
            if ((r->flags & Native.LogFlagAudit) != 0) scope["aster.audit"] = true;
        }

        using IDisposable? s2 = scope is null ? null : logger.BeginScope(new ScopeState(scope));
        logger.Log(level, new EventId(0, "aster"), state, null, static (st, _) => st.Message);
    }

    /// <summary>The structured state of one bridged record.</summary>
    private sealed class RecordState : IReadOnlyList<KeyValuePair<string, object?>>
    {
        private readonly KeyValuePair<string, object?>[] _items;
        public string Message { get; }

        public RecordState(string message, IReadOnlyList<KeyValuePair<string, string>> fields)
        {
            Message = message;
            _items = new KeyValuePair<string, object?>[fields.Count + 2];
            for (int i = 0; i < fields.Count; i++)
                _items[i] = new(fields[i].Key, fields[i].Value);
            _items[fields.Count] = new("aster.origin", "rust");
            _items[fields.Count + 1] = new("{OriginalFormat}", message);
        }

        public int Count => _items.Length;
        public KeyValuePair<string, object?> this[int index] => _items[index];
        public IEnumerator<KeyValuePair<string, object?>> GetEnumerator() => ((IEnumerable<KeyValuePair<string, object?>>)_items).GetEnumerator();
        IEnumerator IEnumerable.GetEnumerator() => _items.GetEnumerator();
        public override string ToString() => Message;
    }

    /// <summary>The scope state: a read-only list of key/value pairs, as MEL's own activity scope.</summary>
    private sealed class ScopeState : IReadOnlyList<KeyValuePair<string, object?>>
    {
        private readonly KeyValuePair<string, object?>[] _items;
        public ScopeState(Dictionary<string, object?> d) => _items = d.ToArray();
        public int Count => _items.Length;
        public KeyValuePair<string, object?> this[int index] => _items[index];
        public IEnumerator<KeyValuePair<string, object?>> GetEnumerator() => ((IEnumerable<KeyValuePair<string, object?>>)_items).GetEnumerator();
        IEnumerator IEnumerable.GetEnumerator() => _items.GetEnumerator();
        public override string ToString() => string.Join(", ", _items.Select(kv => $"{kv.Key}:{kv.Value}"));
    }
}
