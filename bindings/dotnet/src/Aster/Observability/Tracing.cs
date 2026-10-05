using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.Text;

namespace Aster.Observability;

/// <summary>
/// Trace context and binding spans on <see cref="Activity"/> (design §5.3). Binding spans are
/// <see cref="Activity"/> objects from <see cref="Source"/> (<c>ActivitySource("Aster")</c>), so .NET's
/// own instrumentation (HttpClient, ASP.NET Core, EF Core) parents on them through
/// <see cref="Activity.Current"/>:
/// <list type="bullet">
/// <item><b>Inbound.</b> Delivering a call, the binding starts an <see cref="Activity"/> whose parent is the
/// Rust server span (and whose baggage is the call's inbound W3C <c>baggage</c>), so
/// <see cref="Activity.Current"/> is set for the handler and everything it calls.</item>
/// <item><b>One export path.</b> An <see cref="ActivityListener"/> the binding registers on source
/// <c>Aster</c> (<see cref="AsterObservability.Init"/>) reports each stopped, recorded activity to Rust with
/// <c>aster_span_record</c>, which exports it with the core spans and the same resource. Sampling follows
/// the parent's sampled flag; a root is offered to Rust's sampler.</item>
/// <item><b>No double export.</b> If the application also runs the OpenTelemetry .NET SDK it must not
/// subscribe to source <c>Aster</c> (the binding warns once when another listener records an activity it
/// did not). Span links are not carried.</item>
/// <item><b>Outbound.</b> An Aster client call reads <see cref="Activity.Current"/> (any source, so ASP.NET's
/// request activity counts) and sends it as <c>traceparent</c> / <c>baggage</c> call metadata.</item>
/// </list>
/// </summary>
public static class Tracing
{
    /// <summary>The name of the binding's <see cref="ActivitySource"/>.</summary>
    public const string SourceName = "Aster";

    private const int BaggageMaxBytes = 8192;
    private const int BaggageMaxMembers = 64;

    /// <summary>The source of every activity the binding creates and exports.</summary>
    public static ActivitySource Source { get; } = new(SourceName);

    /// <summary>
    /// A source whose activities only carry context (a baggage-only inbound call,
    /// <see cref="WithTraceContext"/>); nothing exports them.
    /// </summary>
    internal static ActivitySource CarrierSource { get; } = new("Aster.Carrier");

    private const string BaggageOnlyKey = "aster.baggage_only";

    static Tracing()
    {
        ActivitySource.AddActivityListener(new ActivityListener
        {
            ShouldListenTo = static s => s.Name == "Aster.Carrier",
            Sample = static (ref ActivityCreationOptions<ActivityContext> _) => ActivitySamplingResult.PropagationData,
            SampleUsingParentId = static (ref ActivityCreationOptions<string> _) => ActivitySamplingResult.PropagationData,
        });
    }

    // ─── Current context ────────────────────────────────────────────────────

    private static Activity? Current
        => Activity.Current is { IdFormat: ActivityIdFormat.W3C } a && a.TraceId != default ? a : null;

    /// <summary>The current W3C <c>traceparent</c> (<c>00-&lt;trace&gt;-&lt;span&gt;-&lt;flags&gt;</c>), or null.</summary>
    public static string? CurrentTraceparent => Current is { } a ? Traceparent(a) : null;

    /// <summary>The current trace id (32 lowercase hex characters), or null.</summary>
    public static string? CurrentTraceId => Current?.TraceId.ToHexString();

    /// <summary>The current span id (16 lowercase hex characters), or null.</summary>
    public static string? CurrentSpanId => Current?.SpanId.ToHexString();

    /// <summary>A copy of the current W3C baggage (this activity's and its parents').</summary>
    public static IReadOnlyDictionary<string, string> CurrentBaggage
    {
        get
        {
            var d = new Dictionary<string, string>(StringComparer.Ordinal);
            if (Activity.Current is { } a)
                foreach (var kv in a.Baggage)
                    d.TryAdd(kv.Key, kv.Value ?? "");
            return d;
        }
    }

    private static string Traceparent(Activity a)
        => string.Create(CultureInfo.InvariantCulture,
            $"00-{a.TraceId.ToHexString()}-{a.SpanId.ToHexString()}-{((a.ActivityTraceFlags & ActivityTraceFlags.Recorded) != 0 ? "01" : "00")}");

    // ─── Spans ──────────────────────────────────────────────────────────────

    /// <summary>
    /// Starts a binding span: an <see cref="Activity"/> of source <c>Aster</c> parented on
    /// <see cref="Activity.Current"/>. Dispose it (or call <see cref="Activity.Stop"/>) to end it. Returns
    /// null while no listener is registered (<see cref="AsterObservability.Init"/> has not run).
    /// </summary>
    public static Activity? StartSpan(string name, ActivityKind kind = ActivityKind.Internal,
        IEnumerable<KeyValuePair<string, object?>>? attributes = null)
        // A default parent context makes Activity.Current the parent (and sets Activity.Parent).
        => Source.StartActivity(kind, default, attributes, null, default, name);

    /// <summary>Records <paramref name="ex"/> on the span the way OpenTelemetry does and marks it failed.</summary>
    public static void RecordException(this Activity? activity, Exception ex)
    {
        if (activity is null) return;
        activity.AddEvent(new ActivityEvent("exception", tags: new ActivityTagsCollection
        {
            ["exception.type"] = ex.GetType().FullName,
            ["exception.message"] = ex.Message,
        }));
        activity.SetStatus(ActivityStatusCode.Error, ex.Message);
    }

    /// <summary>
    /// Makes <paramref name="traceparent"/> (and optional <paramref name="baggage"/>) the current
    /// context until the result is disposed. Use it to continue a trace that arrived by other means
    /// (a message queue, a non-Aster header). An invalid <c>traceparent</c> throws
    /// <see cref="ArgumentException"/>.
    /// </summary>
    public static IDisposable WithTraceContext(string traceparent, IReadOnlyDictionary<string, string>? baggage = null)
    {
        if (!ActivityContext.TryParse(traceparent, null, out ActivityContext ctx))
            throw new ArgumentException("invalid traceparent", nameof(traceparent));
        Activity? a = CarrierSource.StartActivity("aster.context", ActivityKind.Internal, ctx);
        if (a is null) return NoopScope.Instance;
        if (baggage is not null)
            foreach (var kv in baggage) a.AddBaggage(kv.Key, kv.Value);
        return a;
    }

    private sealed class NoopScope : IDisposable
    {
        public static readonly NoopScope Instance = new();
        public void Dispose() { }
    }

    // ─── Outbound ───────────────────────────────────────────────────────────

    /// <summary>
    /// <paramref name="metadata"/> plus the current <c>traceparent</c> / <c>baggage</c> (the same
    /// object when there is no current activity or the caller set its own). Aster's client calls this
    /// for every outbound call.
    /// </summary>
    public static IReadOnlyList<KeyValuePair<string, string>>? Inject(IReadOnlyList<KeyValuePair<string, string>>? metadata)
    {
        Activity? a = Activity.Current;
        if (a is null) return metadata;
        bool hasTp = false, hasBaggage = false;
        if (metadata is not null)
            foreach (var kv in metadata)
            {
                if (kv.Key == "traceparent") hasTp = true;
                else if (kv.Key == "baggage") hasBaggage = true;
            }
        string? tp = !hasTp && !IsBaggageOnly(a) && Current is { } w3c ? Traceparent(w3c) : null;
        string? bag = !hasBaggage ? FormatBaggage(a.Baggage) : null;
        if (tp is null && bag is null) return metadata;
        var list = metadata is null ? new List<KeyValuePair<string, string>>(2) : new List<KeyValuePair<string, string>>(metadata);
        if (tp is not null) list.Add(new("traceparent", tp));
        if (bag is not null) list.Add(new("baggage", bag));
        return list;
    }

    /// <summary>True when the activity descends from a baggage-only inbound carrier: there is no trace to send.</summary>
    private static bool IsBaggageOnly(Activity a)
    {
        for (Activity? p = a; p is not null; p = p.Parent)
            if (p.GetCustomProperty(BaggageOnlyKey) is true) return true;
        return false;
    }

    /// <summary>Formats W3C <c>baggage</c> (percent-encoded values, W3C member and size limits), or null when empty.</summary>
    internal static string? FormatBaggage(IEnumerable<KeyValuePair<string, string?>> items)
    {
        StringBuilder? sb = null;
        int members = 0;
        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var kv in items)
        {
            if (!seen.Add(kv.Key)) continue;
            string member = kv.Key + "=" + Uri.EscapeDataString(kv.Value ?? "");
            if (members >= BaggageMaxMembers || (sb?.Length ?? 0) + member.Length + 1 > BaggageMaxBytes) break;
            (sb ??= new StringBuilder()).Append(members++ == 0 ? "" : ",").Append(member);
        }
        return sb?.ToString();
    }

    /// <summary>Parses a W3C <c>baggage</c> header (properties after <c>;</c> are dropped).</summary>
    internal static List<KeyValuePair<string, string>> ParseBaggage(string? header)
    {
        var result = new List<KeyValuePair<string, string>>();
        if (string.IsNullOrEmpty(header) || header.Length > BaggageMaxBytes) return result;
        foreach (string raw in header.Split(','))
        {
            if (result.Count >= BaggageMaxMembers) break;
            string member = raw;
            int semi = member.IndexOf(';');
            if (semi >= 0) member = member[..semi];
            int eq = member.IndexOf('=');
            if (eq <= 0) continue;
            string key = member[..eq].Trim();
            string value = member[(eq + 1)..].Trim();
            if (key.Length == 0) continue;
            try { value = Uri.UnescapeDataString(value); }
            catch (UriFormatException) { /* keep it raw */ }
            result.Add(new(key, value));
        }
        return result;
    }

    // ─── Inbound ────────────────────────────────────────────────────────────

    /// <summary>
    /// The <see cref="Activity"/> of a dispatched call: a child of the Rust server span (null when the
    /// span is not exported and the call carries no baggage). The caller disposes it when the call ends.
    /// </summary>
    internal static Activity? StartInbound(DispatchServer dispatch, ulong callId, DispatchFlags flags, string service, string method)
    {
        bool hasTrace = (flags & DispatchFlags.HasTrace) != 0;
        bool hasBaggage = (flags & DispatchFlags.HasInboundTrace) != 0;
        if (!hasTrace && !hasBaggage) return null;

        string? baggage = null, tracestate = null;
        if (hasBaggage)
            foreach (var kv in dispatch.InboundTrace(callId))
            {
                if (kv.Key == "baggage") baggage = kv.Value;
                else if (kv.Key == "tracestate") tracestate = kv.Value;
            }

        Activity? activity = null;
        string name = service + "/" + method + " handler";
        if (hasTrace && dispatch.TryTraceContext(callId, out ActivityContext parent, tracestate))
        {
            activity = Source.StartActivity(name, ActivityKind.Internal, parent);
            if (activity is null) activity = CarrierSource.StartActivity(name, ActivityKind.Internal, parent);
        }
        else if (baggage is not null)
        {
            activity = CarrierSource.StartActivity(name, ActivityKind.Internal, default(ActivityContext));
            activity?.SetCustomProperty(BaggageOnlyKey, true);
        }
        if (activity is null) return null;

        activity.SetTag("rpc.system", "aster");
        activity.SetTag("rpc.service", service);
        activity.SetTag("rpc.method", method);
        activity.SetTag("aster.call_id", callId);
        if (baggage is not null)
            foreach (var kv in ParseBaggage(baggage)) activity.AddBaggage(kv.Key, kv.Value);
        return activity;
    }
}
