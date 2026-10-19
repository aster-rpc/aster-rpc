using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.IO.Compression;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.Extensions.Logging;

namespace Aster.Tests;

/// <summary>One span an <see cref="OtlpReceiver"/> decoded from an OTLP/HTTP protobuf export.</summary>
public sealed record ExportedSpan(
    string TraceId, string SpanId, string ParentSpanId, string Name, int Kind, int StatusCode,
    IReadOnlyDictionary<string, string> Attributes, IReadOnlyList<string> Events);

/// <summary>
/// A loopback OTLP/HTTP trace receiver (<c>POST /v1/traces</c>, protobuf), so tests assert on what
/// the Rust exporter really sends. Synthetic endpoint only (127.0.0.1).
/// </summary>
public sealed class OtlpReceiver : IDisposable
{
    private readonly HttpListener _listener = new();
    private readonly ConcurrentQueue<ExportedSpan> _spans = new();
    private readonly CancellationTokenSource _stop = new();
    private Task _loop;

    public string Url { get; }

    public OtlpReceiver()
    {
        var probe = new TcpListener(IPAddress.Loopback, 0);
        probe.Start();
        int port = ((IPEndPoint)probe.LocalEndpoint).Port;
        probe.Stop();
        Url = $"http://127.0.0.1:{port}";
        _listener.Prefixes.Add(Url + "/");
        _listener.Start();
        _loop = Task.Run(Loop);
    }

    public IReadOnlyList<ExportedSpan> Spans => _spans.ToArray();

    private readonly ConcurrentQueue<byte[]> _logBodies = new();
    private int _rejectLogs;
    private int _rejectedLogs;

    /// <summary>Raw OTLP log export bodies (<c>POST /v1/logs</c>), decompressed.</summary>
    public IReadOnlyList<byte[]> LogBodies => _logBodies.ToArray();

    /// <summary>When true, log exports receive HTTP 503 and are counted instead of retained.</summary>
    public bool RejectLogs
    {
        get => Volatile.Read(ref _rejectLogs) != 0;
        set => Volatile.Write(ref _rejectLogs, value ? 1 : 0);
    }

    /// <summary>Number of log exports answered with HTTP 503.</summary>
    public int RejectedLogs => Volatile.Read(ref _rejectedLogs);
    public bool StallLogs { get; set; }

    public void StopListening()
    {
        _listener.Stop();
        _loop.Wait(WaitBudget.Value);
    }

    public void StartListening()
    {
        _listener.Start();
        _loop = Task.Run(Loop);
    }

    /// <summary>Whether any received log export contains <paramref name="needle"/> as raw bytes.</summary>
    public bool LogsContain(ReadOnlySpan<byte> needle)
    {
        foreach (var b in _logBodies)
            if (b.AsSpan().IndexOf(needle) >= 0) return true;
        return false;
    }

    private async Task Loop()
    {
        while (!_stop.IsCancellationRequested)
        {
            HttpListenerContext ctx;
            try { ctx = await _listener.GetContextAsync().ConfigureAwait(false); }
            catch (Exception) { return; }
            try
            {
                ctx.Response.StatusCode = 200;
                using var ms = new MemoryStream();
                await ctx.Request.InputStream.CopyToAsync(ms).ConfigureAwait(false);
                byte[] body = ms.ToArray();
                if (string.Equals(ctx.Request.Headers["Content-Encoding"], "gzip", StringComparison.OrdinalIgnoreCase))
                {
                    using var gz = new GZipStream(new MemoryStream(body), CompressionMode.Decompress);
                    using var plain = new MemoryStream();
                    gz.CopyTo(plain);
                    body = plain.ToArray();
                }
                if (ctx.Request.Url!.AbsolutePath.EndsWith("/v1/logs", StringComparison.Ordinal) && RejectLogs)
                {
                    Interlocked.Increment(ref _rejectedLogs);
                    while (StallLogs && RejectLogs && !_stop.IsCancellationRequested)
                        await Task.Delay(10).ConfigureAwait(false);
                    ctx.Response.StatusCode = 503;
                }
                else if (ctx.Request.Url!.AbsolutePath.EndsWith("/v1/traces", StringComparison.Ordinal))
                    foreach (var s in Protobuf.ParseTraces(body)) _spans.Enqueue(s);
                else if (ctx.Request.Url!.AbsolutePath.EndsWith("/v1/logs", StringComparison.Ordinal))
                    _logBodies.Enqueue(body);
                ctx.Response.ContentType = "application/x-protobuf";
                ctx.Response.ContentLength64 = 0;
            }
            catch (Exception) { ctx.Response.StatusCode = 400; }
            finally { ctx.Response.Close(); }
        }
    }

    /// <summary>Waits (up to <paramref name="timeout"/>) for <paramref name="predicate"/> over the spans received so far.</summary>
    public async Task<bool> WaitAsync(Func<IReadOnlyList<ExportedSpan>, bool> predicate, TimeSpan? timeout = null)
    {
        var deadline = DateTime.UtcNow + (timeout ?? TimeSpan.FromSeconds(10));
        while (DateTime.UtcNow < deadline)
        {
            Aster.Observability.AsterObservability.Flush(TimeSpan.FromSeconds(2));
            if (predicate(Spans)) return true;
            await Task.Delay(100);
        }
        return predicate(Spans);
    }

    public void Dispose()
    {
        _stop.Cancel();
        try { _listener.Close(); } catch (Exception) { /* swallow-ok: best-effort teardown of the collector */ }
        try { _loop.Wait(1000); } catch (Exception) { /* swallow-ok: best-effort teardown of the collector */ }
    }
}

/// <summary>Just enough protobuf to read an <c>ExportTraceServiceRequest</c>.</summary>
internal static class Protobuf
{
    private readonly ref struct Field(int number, int wire, ulong varint, ReadOnlySpan<byte> bytes)
    {
        public readonly int Number = number;
        public readonly int Wire = wire;
        public readonly ulong Varint = varint;
        public readonly ReadOnlySpan<byte> Bytes = bytes;
    }

    private static bool Next(ref ReadOnlySpan<byte> b, out Field f)
    {
        f = default;
        if (b.IsEmpty) return false;
        ulong key = ReadVarint(ref b);
        int wire = (int)(key & 7), number = (int)(key >> 3);
        switch (wire)
        {
            case 0: f = new Field(number, wire, ReadVarint(ref b), default); break;
            case 1: f = new Field(number, wire, 0, b[..8]); b = b[8..]; break;
            case 2:
                int n = (int)ReadVarint(ref b);
                f = new Field(number, wire, 0, b[..n]);
                b = b[n..];
                break;
            case 5: f = new Field(number, wire, 0, b[..4]); b = b[4..]; break;
            default: throw new InvalidDataException($"wire type {wire}");
        }
        return true;
    }

    private static ulong ReadVarint(ref ReadOnlySpan<byte> b)
    {
        ulong v = 0;
        int shift = 0;
        while (true)
        {
            byte x = b[0];
            b = b[1..];
            v |= (ulong)(x & 0x7f) << shift;
            if ((x & 0x80) == 0) return v;
            shift += 7;
        }
    }

    public static List<ExportedSpan> ParseTraces(byte[] body)
    {
        var result = new List<ExportedSpan>();
        var req = new ReadOnlySpan<byte>(body);
        while (Next(ref req, out var rs))
        {
            if (rs.Number != 1) continue; // resource_spans
            var resource = rs.Bytes;
            while (Next(ref resource, out var ss))
            {
                if (ss.Number != 2) continue; // scope_spans
                var scope = ss.Bytes;
                while (Next(ref scope, out var sp))
                    if (sp.Number == 2) result.Add(ParseSpan(sp.Bytes));
            }
        }
        return result;
    }

    private static ExportedSpan ParseSpan(ReadOnlySpan<byte> b)
    {
        string trace = "", span = "", parent = "", name = "";
        int kind = 0, status = 0;
        var attrs = new Dictionary<string, string>();
        var events = new List<string>();
        while (Next(ref b, out var f))
        {
            switch (f.Number)
            {
                case 1: trace = Convert.ToHexString(f.Bytes).ToLowerInvariant(); break;
                case 2: span = Convert.ToHexString(f.Bytes).ToLowerInvariant(); break;
                case 4: parent = Convert.ToHexString(f.Bytes).ToLowerInvariant(); break;
                case 5: name = System.Text.Encoding.UTF8.GetString(f.Bytes); break;
                case 6: kind = (int)f.Varint; break;
                case 9:
                    var (k, v) = ParseKeyValue(f.Bytes);
                    attrs[k] = v;
                    break;
                case 11:
                    var ev = f.Bytes;
                    while (Next(ref ev, out var ef))
                        if (ef.Number == 2) events.Add(System.Text.Encoding.UTF8.GetString(ef.Bytes));
                    break;
                case 15:
                    var st = f.Bytes;
                    while (Next(ref st, out var sf))
                        if (sf.Number == 3) status = (int)sf.Varint;
                    break;
            }
        }
        return new ExportedSpan(trace, span, parent, name, kind, status, attrs, events);
    }

    private static (string, string) ParseKeyValue(ReadOnlySpan<byte> b)
    {
        string key = "", value = "";
        while (Next(ref b, out var f))
        {
            if (f.Number == 1) key = System.Text.Encoding.UTF8.GetString(f.Bytes);
            else if (f.Number == 2)
            {
                var any = f.Bytes;
                while (Next(ref any, out var af))
                    if (af.Number == 1) value = System.Text.Encoding.UTF8.GetString(af.Bytes);
            }
        }
        return (key, value);
    }
}

/// <summary>One record an <see cref="ILogger"/> of <see cref="CapturingLoggerFactory"/> saw.</summary>
public sealed record CapturedLog(
    string Category, LogLevel Level, string Message, IReadOnlyDictionary<string, object?> State,
    IReadOnlyDictionary<string, object?> Scope);

/// <summary>An <see cref="ILoggerFactory"/> that records every enabled log call with its scopes.</summary>
public sealed class CapturingLoggerFactory : ILoggerFactory
{
    private readonly ConcurrentQueue<CapturedLog> _logs = new();

    public LogLevel MinimumLevel { get; set; } = LogLevel.Trace;
    public IReadOnlyList<CapturedLog> Logs => _logs.ToArray();

    public ILogger CreateLogger(string categoryName) => new Logger(this, categoryName);
    public void AddProvider(ILoggerProvider provider) { }
    public void Dispose() { }

    [ThreadStatic] private static List<IReadOnlyList<KeyValuePair<string, object?>>>? t_scopes;

    private sealed class Logger(CapturingLoggerFactory owner, string category) : ILogger
    {
        public bool IsEnabled(LogLevel logLevel) => logLevel >= owner.MinimumLevel;

        public IDisposable? BeginScope<TState>(TState state) where TState : notnull
        {
            var scopes = t_scopes ??= new();
            var pairs = state as IReadOnlyList<KeyValuePair<string, object?>> ?? Array.Empty<KeyValuePair<string, object?>>();
            scopes.Add(pairs);
            return new Pop(scopes, pairs);
        }

        public void Log<TState>(LogLevel logLevel, EventId eventId, TState state, Exception? exception, Func<TState, Exception?, string> formatter)
        {
            if (!IsEnabled(logLevel)) return;
            var st = new Dictionary<string, object?>();
            if (state is IEnumerable<KeyValuePair<string, object?>> kvs)
                foreach (var kv in kvs) st[kv.Key] = kv.Value;
            var sc = new Dictionary<string, object?>();
            if (t_scopes is { } scopes)
                foreach (var s in scopes)
                    foreach (var kv in s) sc[kv.Key] = kv.Value;
            owner._logs.Enqueue(new CapturedLog(category, logLevel, formatter(state, exception), st, sc));
        }
    }

    private sealed class Pop(List<IReadOnlyList<KeyValuePair<string, object?>>> scopes, IReadOnlyList<KeyValuePair<string, object?>> item) : IDisposable
    {
        public void Dispose() => scopes.Remove(item);
    }
}
