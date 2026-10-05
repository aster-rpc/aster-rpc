using System;
using System.Collections.Generic;
using System.Text;
using System.Threading;

namespace Aster.Observability;

/// <summary>
/// Application metrics over the Rust OpenTelemetry pipeline. Thin wrappers: each instrument is a
/// handle into <c>aster-otel</c> (meter scope <c>aster.app</c>), recorded through the same
/// <c>MeterProvider</c> as Aster's built-in RPC metrics (<c>aster.rpc.server.*</c>, ...), so one
/// exporter and one <c>/metrics</c> endpoint serve both. Without a pipeline
/// (<see cref="AsterObservability.Init"/>) recording is a no-op.
/// <code>
/// var orders = Metrics.Counter("orders.placed", "Orders placed");
/// var latency = Metrics.Histogram("orders.latency", "Order latency", unit: "s");
/// orders.Add(1, ("region", "eu"));
/// latency.Record(0.042, ("region", "eu"));
/// </code>
/// Attribute values are stringified. Keep their cardinality bounded: the SDK folds series beyond
/// 2000 per instrument into <c>otel.metric.overflow=true</c>. Names follow the OpenTelemetry rules
/// (a letter, then letters, digits, <c>_ . - /</c>); a name has one kind. A histogram in seconds
/// (<c>unit: "s"</c>) gets the RPC duration buckets (0.005 to 10 s); pass <c>boundaries</c> (finite,
/// strictly increasing) for others. <c>autoAttrs</c> and <c>cardinalityLimit</c> are accepted and
/// recorded, but have no effect yet. Scrape with <see cref="Aster.Health.HealthServer"/> (<c>/metrics</c>)
/// or <see cref="AsterObservability.MetricsText"/>.
/// </summary>
public static class Metrics
{
    /// <summary>A monotonic sum.</summary>
    public static Counter Counter(string name, string description = "", string unit = "",
        IReadOnlyList<string>? autoAttrs = null, uint cardinalityLimit = 0)
        => new(name, description, unit, autoAttrs, cardinalityLimit);

    /// <summary>A sum that may go down (in-flight work, queue depth).</summary>
    public static UpDownCounter UpDownCounter(string name, string description = "", string unit = "",
        IReadOnlyList<string>? autoAttrs = null, uint cardinalityLimit = 0)
        => new(name, description, unit, autoAttrs, cardinalityLimit);

    /// <summary>A distribution of values (durations, sizes).</summary>
    public static Histogram Histogram(string name, string description = "", string unit = "",
        IReadOnlyList<double>? boundaries = null, IReadOnlyList<string>? autoAttrs = null, uint cardinalityLimit = 0)
        => new(name, description, unit, autoAttrs, cardinalityLimit, boundaries);

    /// <summary>The last recorded value.</summary>
    public static Gauge Gauge(string name, string description = "", string unit = "",
        IReadOnlyList<string>? autoAttrs = null, uint cardinalityLimit = 0)
        => new(name, description, unit, autoAttrs, cardinalityLimit);

    /// <summary>
    /// An interned attribute set: build once, record many times without re-encoding. Dispose to free
    /// the native handle (a pipeline shutdown frees it too; the set re-creates itself on next use).
    /// </summary>
    public static AttributeSet Attributes(params (string Key, object? Value)[] attrs) => new(attrs);

    /// <summary>Encodes attributes as a C ABI k/v list (values stringified).</summary>
    internal static byte[] Encode(ReadOnlySpan<(string Key, object? Value)> attrs)
    {
        if (attrs.IsEmpty) return Array.Empty<byte>();
        var list = new List<KeyValuePair<string, string>>(attrs.Length);
        foreach (var (k, v) in attrs) list.Add(new(k, AsterObservability.Str(v)));
        return KvList.Encode(list);
    }
}

/// <summary>An interned attribute set (<c>aster_attrset_t</c>).</summary>
public sealed unsafe class AttributeSet : IDisposable
{
    private readonly byte[] _kv;
    private ulong _handle;

    internal AttributeSet((string Key, object? Value)[] attrs) => _kv = Metrics.Encode(attrs);

    /// <summary>The handle, created lazily and again after a pipeline shutdown freed it. 0 = none.</summary>
    internal ulong Handle(bool recreate = false)
    {
        if (_kv.Length == 0) return 0;
        ulong h = Volatile.Read(ref _handle);
        if (h != 0 && !recreate) return h;
        ulong fresh;
        fixed (byte* p = _kv)
            Native.Check(Native.aster_attrset_new(p, (uint)_kv.Length, &fresh), "aster_attrset_new");
        Volatile.Write(ref _handle, fresh);
        return fresh;
    }

    /// <summary>Frees the native handle.</summary>
    public void Dispose()
    {
        ulong h = Interlocked.Exchange(ref _handle, 0);
        if (h != 0) Native.aster_attrset_free(h);
    }
}

/// <summary>Common part of the four instruments.</summary>
public abstract unsafe class Instrument : IDisposable
{
    private readonly object _declareLock = new();
    private ulong _handle;
    private readonly double[] _boundaries;
    private readonly byte[] _autoAttrs;

    internal Instrument(uint kind, string name, string description, string unit,
        IReadOnlyList<string>? autoAttrs, uint cardinalityLimit, IReadOnlyList<double>? boundaries)
    {
        ArgumentException.ThrowIfNullOrEmpty(name);
        Kind = kind;
        Name = name;
        Description = description ?? "";
        Unit = unit ?? "";
        CardinalityLimit = cardinalityLimit;
        _boundaries = boundaries is { Count: > 0 } ? [.. boundaries] : Array.Empty<double>();
        _autoAttrs = EncodeKeys(autoAttrs);
        Declare(); // surfaces a bad name or a kind clash now
    }

    /// <summary>The OpenTelemetry instrument name.</summary>
    public string Name { get; }
    /// <summary>The description.</summary>
    public string Description { get; }
    /// <summary>The UCUM unit ("" = none).</summary>
    public string Unit { get; }
    /// <summary>The per-instrument series limit (0 = default); recorded, not applied yet.</summary>
    public uint CardinalityLimit { get; }

    private uint Kind { get; }

    private static byte[] EncodeKeys(IReadOnlyList<string>? keys)
    {
        if (keys is not { Count: > 0 }) return Array.Empty<byte>();
        int size = 4;
        foreach (var k in keys) size += 4 + Encoding.UTF8.GetByteCount(k);
        var buf = new byte[size];
        System.Buffers.Binary.BinaryPrimitives.WriteUInt32LittleEndian(buf, (uint)keys.Count);
        int o = 4;
        foreach (var k in keys)
        {
            int n = Encoding.UTF8.GetBytes(k, buf.AsSpan(o + 4));
            System.Buffers.Binary.BinaryPrimitives.WriteUInt32LittleEndian(buf.AsSpan(o), (uint)n);
            o += 4 + n;
        }
        return buf;
    }

    private void Declare()
    {
        lock (_declareLock)
        {
            using var scope = new NativeScope();
            var d = new InstrumentDescNative
            {
                struct_size = (uint)sizeof(InstrumentDescNative),
                kind = Kind,
                name = scope.Utf8(Name),
                description = scope.Utf8(Description),
                unit = scope.Utf8(Unit),
                auto_attrs = scope.Bytes(_autoAttrs),
                cardinality_limit = CardinalityLimit,
            };
            if (_boundaries.Length > 0)
            {
                var arr = (double*)scope.Alloc(sizeof(double) * _boundaries.Length);
                for (int i = 0; i < _boundaries.Length; i++) arr[i] = _boundaries[i];
                d.boundaries = arr;
                d.boundaries_len = (uint)_boundaries.Length;
            }
            ulong h;
            Native.Check(Native.aster_instrument_new(&d, &h), "aster_instrument_new");
            Volatile.Write(ref _handle, h);
        }
    }

    private protected enum Op { AddU64, AddI64, RecordF64 }

    /// <summary>
    /// One record. A handle freed by a pipeline shutdown (<c>NOT_FOUND</c>) is declared again and
    /// the record retried once.
    /// </summary>
    private protected void Send(Op op, ulong u, long i, double d, ReadOnlySpan<(string Key, object? Value)> attrs, AttributeSet? set)
    {
        byte[] kv = set is null ? Metrics.Encode(attrs) : Array.Empty<byte>();
        fixed (byte* p = kv)
        {
            byte* kp = kv.Length == 0 ? null : p;
            int r = Once(op, u, i, d, kp, (uint)kv.Length, set, recreate: false);
            if (r == (int)Status.NotFound)
            {
                Declare();
                r = Once(op, u, i, d, kp, (uint)kv.Length, set, recreate: true);
            }
            if (r != 0) throw IrohException.FromStatus(r, "aster_metric (" + Name + ")");
        }
    }

    private int Once(Op op, ulong u, long i, double d, byte* kv, uint kvLen, AttributeSet? set, bool recreate)
    {
        ulong h = Volatile.Read(ref _handle);
        ulong s = set?.Handle(recreate) ?? 0;
        return op switch
        {
            Op.AddU64 => Native.aster_metric_add_u64(h, u, s, kv, kvLen, 0),
            Op.AddI64 => Native.aster_metric_add_i64(h, i, s, kv, kvLen, 0),
            _ => Native.aster_metric_record_f64(h, d, s, kv, kvLen, 0),
        };
    }

    /// <summary>Frees the native handle (optional; a pipeline shutdown frees every handle).</summary>
    public void Dispose()
    {
        ulong h = Interlocked.Exchange(ref _handle, 0);
        if (h != 0) Native.aster_instrument_free(h);
    }

    /// <inheritdoc/>
    public override string ToString() => $"{GetType().Name}({Name})";
}

/// <summary>A monotonic sum.</summary>
public sealed unsafe class Counter : Instrument
{
    internal Counter(string n, string d, string u, IReadOnlyList<string>? a, uint c) : base(1, n, d, u, a, c, null) { }

    /// <summary>Adds <paramref name="value"/> (not negative).</summary>
    public void Add(long value, params (string Key, object? Value)[] attrs)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(value);
        Send(Op.AddU64, (ulong)value, 0, 0, attrs, null);
    }

    /// <summary>Adds with a prebuilt <see cref="AttributeSet"/>.</summary>
    public void Add(long value, AttributeSet set)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(value);
        Send(Op.AddU64, (ulong)value, 0, 0, default, set);
    }
}

/// <summary>A sum that may go down.</summary>
public sealed unsafe class UpDownCounter : Instrument
{
    internal UpDownCounter(string n, string d, string u, IReadOnlyList<string>? a, uint c) : base(2, n, d, u, a, c, null) { }

    /// <summary>Adds <paramref name="value"/> (may be negative).</summary>
    public void Add(long value, params (string Key, object? Value)[] attrs)
        => Send(Op.AddI64, 0, value, 0, attrs, null);

    /// <summary>Adds with a prebuilt <see cref="AttributeSet"/>.</summary>
    public void Add(long value, AttributeSet set)
        => Send(Op.AddI64, 0, value, 0, default, set);
}

/// <summary>A distribution of values.</summary>
public sealed unsafe class Histogram : Instrument
{
    internal Histogram(string n, string d, string u, IReadOnlyList<string>? a, uint c, IReadOnlyList<double>? b) : base(3, n, d, u, a, c, b) { }

    /// <summary>Records <paramref name="value"/>.</summary>
    public void Record(double value, params (string Key, object? Value)[] attrs)
        => Send(Op.RecordF64, 0, 0, value, attrs, null);

    /// <summary>Records with a prebuilt <see cref="AttributeSet"/>.</summary>
    public void Record(double value, AttributeSet set)
        => Send(Op.RecordF64, 0, 0, value, default, set);
}

/// <summary>The last recorded value.</summary>
public sealed unsafe class Gauge : Instrument
{
    internal Gauge(string n, string d, string u, IReadOnlyList<string>? a, uint c) : base(4, n, d, u, a, c, null) { }

    /// <summary>Records <paramref name="value"/>.</summary>
    public void Record(double value, params (string Key, object? Value)[] attrs)
        => Send(Op.RecordF64, 0, 0, value, attrs, null);

    /// <summary>Records with a prebuilt <see cref="AttributeSet"/>.</summary>
    public void Record(double value, AttributeSet set)
        => Send(Op.RecordF64, 0, 0, value, default, set);
}
