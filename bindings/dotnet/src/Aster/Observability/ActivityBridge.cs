using System;
using System.Buffers.Binary;
using System.Collections.Generic;
using System.Diagnostics;
using System.Text;
using System.Threading;

namespace Aster.Observability;

/// <summary>
/// The <see cref="ActivityListener"/> behind <see cref="Tracing"/>: every stopped, recorded activity of
/// source <c>Aster</c> is exported by the Rust SDK with <c>aster_span_record</c> (explicit ids, times,
/// kind, status, attributes and events). The listener's <c>Sample</c> callback records only when the
/// parent is sampled, and offers a root to Rust's sampler.
/// </summary>
internal static unsafe class ActivityBridge
{
    private static readonly object s_lock = new();
    private static ActivityListener? s_listener;
    private static int s_warned;
    private const string ForeignKey = "aster.foreign_recorded";

    public static bool IsRunning => s_listener is not null;

    public static void Start()
    {
        lock (s_lock)
        {
            if (s_listener is not null) return;
            s_listener = new ActivityListener
            {
                // By name: the listener may be added before Tracing's static fields exist.
                ShouldListenTo = static s => s.Name == Tracing.SourceName,
                Sample = static (ref ActivityCreationOptions<ActivityContext> o) =>
                    o.Parent == default || (o.Parent.TraceFlags & ActivityTraceFlags.Recorded) != 0
                        ? ActivitySamplingResult.AllDataAndRecorded
                        : ActivitySamplingResult.PropagationData,
                SampleUsingParentId = static (ref ActivityCreationOptions<string> o) =>
                    ParentIdRecorded(o.Parent)
                        ? ActivitySamplingResult.AllDataAndRecorded
                        : ActivitySamplingResult.PropagationData,
                ActivityStarted = OnStarted,
                ActivityStopped = OnStopped,
            };
            ActivitySource.AddActivityListener(s_listener);
        }
    }

    public static void Stop()
    {
        lock (s_lock)
        {
            s_listener?.Dispose();
            s_listener = null;
        }
    }

    /// <summary>A W3C parent id ends in the trace flags (<c>-01</c>); anything else is a root.</summary>
    private static bool ParentIdRecorded(string? parentId)
    {
        if (string.IsNullOrEmpty(parentId)) return true;
        if (parentId.Length == 55 && parentId[52] == '-' &&
            byte.TryParse(parentId.AsSpan(53, 2), System.Globalization.NumberStyles.HexNumber, null, out byte flags))
            return (flags & 1) != 0;
        return false;
    }

    private static void OnStarted(Activity a)
    {
        // We answered PropagationData (the in-process parent is not sampled), yet the activity is
        // recorded: another listener on source "Aster" (an OpenTelemetry SDK subscription) sampled it.
        // It exports the activity itself, so we must not. (An activity parented by a bare remote
        // context cannot be told apart: its flags are inherited from the parent.)
        if (a.Parent is { } parent && (parent.ActivityTraceFlags & ActivityTraceFlags.Recorded) == 0
            && a.Recorded && a.IsAllDataRequested)
        {
            a.SetCustomProperty(ForeignKey, true);
            if (Interlocked.Exchange(ref s_warned, 1) == 0)
                Console.Error.WriteLine(
                    "aster: another listener records activities of source 'Aster'; the OpenTelemetry SDK must not " +
                    "subscribe to it (the binding exports them through the Aster pipeline)");
        }
    }

    private static void OnStopped(Activity a)
    {
        try
        {
            if (!a.Recorded || a.GetCustomProperty(ForeignKey) is true) return;
            Record(a);
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"aster: span record failed: {ex.Message}");
        }
    }

    private static ulong UnixNanos(DateTime utc) => (ulong)(utc.Ticks - DateTime.UnixEpoch.Ticks) * 100UL;

    private static void Record(Activity a)
    {
        using var scope = new NativeScope();
        var rec = new SpanRecordNative();
        a.TraceId.CopyTo(new Span<byte>(rec.context.trace_id, 16));
        a.SpanId.CopyTo(new Span<byte>(rec.context.span_id, 8));
        rec.context.trace_flags = (byte)a.ActivityTraceFlags;
        if (a.ParentSpanId != default)
        {
            a.TraceId.CopyTo(new Span<byte>(rec.parent.trace_id, 16));
            a.ParentSpanId.CopyTo(new Span<byte>(rec.parent.span_id, 8));
            rec.parent.trace_flags = (byte)a.ActivityTraceFlags;
        }
        ulong start = UnixNanos(a.StartTimeUtc);
        rec.start_unix_nanos = start;
        rec.end_unix_nanos = start + (ulong)a.Duration.Ticks * 100UL;
        rec.kind = (uint)a.Kind;
        rec.status_code = (uint)a.Status;
        rec.name = scope.Utf8(a.DisplayName);
        rec.status_message = scope.Utf8(a.Status == ActivityStatusCode.Error ? a.StatusDescription : null);

        var tags = new List<KeyValuePair<string, string>>();
        foreach (var t in a.TagObjects) tags.Add(new(t.Key, AsterObservability.Str(t.Value)));
        byte[] kv = tags.Count == 0 ? Array.Empty<byte>() : KvList.Encode(tags);
        rec.kv = scope.Ptr(kv);
        rec.kv_len = (uint)kv.Length;

        byte[] events = EncodeEvents(a);
        rec.events = scope.Ptr(events);
        rec.events_len = (uint)events.Length;

        int r = Native.aster_span_record(&rec);
        if (r != 0) Debug.WriteLine($"aster: aster_span_record: {IrohException.FromStatus(r, "aster_span_record").Message}");
    }

    /// <summary>Events: <c>u32 LE count</c>, then per event <c>u32 len</c> + name, <c>u64 unix_nanos</c>, <c>u32 len</c> + a k/v list.</summary>
    internal static byte[] EncodeEvents(Activity a)
    {
        var events = new List<ActivityEvent>(a.Events);
        if (events.Count == 0) return Array.Empty<byte>();
        using var ms = new System.IO.MemoryStream();
        byte[] w = new byte[8];
        void U32(uint v) { BinaryPrimitives.WriteUInt32LittleEndian(w, v); ms.Write(w, 0, 4); }
        void U64(ulong v) { BinaryPrimitives.WriteUInt64LittleEndian(w, v); ms.Write(w, 0, 8); }
        U32((uint)events.Count);
        foreach (var e in events)
        {
            byte[] name = Encoding.UTF8.GetBytes(e.Name);
            U32((uint)name.Length);
            ms.Write(name);
            U64(UnixNanos(e.Timestamp.UtcDateTime));
            var tags = new List<KeyValuePair<string, string>>();
            foreach (var t in e.Tags) tags.Add(new(t.Key, AsterObservability.Str(t.Value)));
            byte[] ekv = tags.Count == 0 ? Array.Empty<byte>() : KvList.Encode(tags);
            U32((uint)ekv.Length);
            ms.Write(ekv);
        }
        return ms.ToArray();
    }
}
