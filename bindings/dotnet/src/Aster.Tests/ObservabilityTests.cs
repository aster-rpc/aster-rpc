using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Net;
using System.Net.Http;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;
using Aster.Health;
using Aster.Observability;
using Aster.Rpc;
using Microsoft.Extensions.Logging;

namespace Aster.Tests;

/// <summary>
/// Observability (O9): the ILogger bridge, metrics, binding spans, the Activity bridge and
/// Aster.Health over the C ABI 4.6 surface. End-to-end cases assert on what the Rust exporter really
/// sends (a loopback OTLP receiver), not on the binding's own bookkeeping.
/// </summary>
[Collection("Observability")]
public class ObservabilityTests : IAsyncLifetime
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    public Task InitializeAsync()
    {
        Reset();
        return Task.CompletedTask;
    }

    public Task DisposeAsync()
    {
        Reset();
        return Task.CompletedTask;
    }

    private static void Reset()
    {
        AsterObservability.Shutdown(TimeSpan.FromSeconds(2));
        AsterLogging.UseLoggerFactory(null);
    }

    /// <summary>What a handler saw, captured on the handler's thread.</summary>
    private sealed record Seen(
        string? TraceId, string? SpanId, string? ParentSpanId, string? SourceName, string? Traceparent,
        IReadOnlyDictionary<string, string> Baggage, string? ChildTraceId, string? ChildParentSpanId, string? ChildSpanId);

    private sealed class Harness : IAsyncDisposable
    {
        public OtlpReceiver Receiver { get; } = new();
        public ConcurrentQueue<Seen> Handled { get; } = new();
        public AsterServer Server { get; private set; } = null!;
        public AsterClient Client { get; private set; } = null!;
        public RpcConnection Conn { get; private set; } = null!;
        public ActivitySource ChildSource { get; } = new("O9.Http");
        private readonly ActivityListener _childListener;

        public Harness()
        {
            _childListener = new ActivityListener
            {
                ShouldListenTo = s => s.Name == "O9.Http",
                Sample = (ref ActivityCreationOptions<ActivityContext> _) => ActivitySamplingResult.AllDataAndRecorded,
            };
            ActivitySource.AddActivityListener(_childListener);
        }

        public static void InitPipeline(string endpoint, bool logs = false, string? filter = null)
            => AsterObservability.Init(new ObservabilityOptions
            {
                ServiceName = "o9-tests",
                Traces = true,
                OtlpEndpoint = endpoint,
                FromEnv = false,
                LogBridge = logs,
                LogFilter = filter,
            });

        public async Task StartAsync(Func<EchoRequest, ServerCallContext, ValueTask<EchoReply>>? handler = null, HealthOptions? health = null)
        {
            var svc = ServiceDefinition.Create("O9Echo", 1)
                .Unary<EchoRequest, EchoReply>("echo", handler ?? Record)
                .Build();
            Server = await AsterServer.StartAsync(new AsterServerOptions
            {
                Config = Loopback,
                Services = { svc },
                Health = health,
            });
            Client = await AsterClient.StartAsync(Loopback);
            Client.RegisterTypes(svc);
            Conn = await Client.ConnectAsync(await Server.TicketAsync());
        }

        private ValueTask<EchoReply> Record(EchoRequest req, ServerCallContext ctx)
        {
            var a = Activity.Current;
            string? childTrace = null, childParent = null, childSpan = null;
            if (a is not null)
            {
                // What HttpClient / ASP.NET Core / EF Core do: start an activity on Current.
                using var child = ChildSource.StartActivity("GET example.com", ActivityKind.Client);
                childTrace = child?.TraceId.ToHexString();
                childParent = child?.ParentSpanId.ToHexString();
                childSpan = child?.SpanId.ToHexString();
            }
            Handled.Enqueue(new Seen(
                a?.TraceId.ToHexString(), a?.SpanId.ToHexString(), a?.ParentSpanId.ToHexString(), a?.Source.Name,
                Tracing.CurrentTraceparent, Tracing.CurrentBaggage, childTrace, childParent, childSpan));
            return ValueTask.FromResult(new EchoReply { Message = req.Message, Count = req.Count + 1, ServerLang = "dotnet" });
        }

        public Task<EchoReply> CallAsync(string message = "hi")
            => Conn.UnaryAsync<EchoRequest, EchoReply>("O9Echo", "echo", new EchoRequest { Message = message });

        public async ValueTask DisposeAsync()
        {
            if (Client is not null) await Client.DisposeAsync();
            if (Server is not null) await Server.DisposeAsync();
            _childListener.Dispose();
            ChildSource.Dispose();
            Receiver.Dispose();
        }
    }

    private static ActivityListener ListenTo(string source)
    {
        var l = new ActivityListener
        {
            ShouldListenTo = s => s.Name == source,
            Sample = (ref ActivityCreationOptions<ActivityContext> _) => ActivitySamplingResult.AllDataAndRecorded,
        };
        ActivitySource.AddActivityListener(l);
        return l;
    }

    // ─── ABI surface ────────────────────────────────────────────────────────

    [Fact]
    public unsafe void Struct_Sizes_Match_Header()
    {
        Assert.Equal(128, sizeof(ObservabilityConfigNative));
        Assert.Equal(96, sizeof(LogRecordNative));
        Assert.Equal(72, sizeof(LogEmitNative));
        Assert.Equal(32, sizeof(SpanContextNative));
        Assert.Equal(152, sizeof(SpanRecordNative));
        Assert.Equal(96, sizeof(InstrumentDescNative));
        Assert.Equal(24, sizeof(HealthConfigNative));
    }

    [Fact]
    public void AlreadyExists_Is_17_And_Named_By_Native()
    {
        Assert.Equal(17, (int)Status.AlreadyExists);
        Assert.Equal("ASTER_STATUS_ALREADY_EXISTS", Marshal.PtrToStringUTF8(Native.aster_status_name(17)));
        Assert.Equal(0u, Native.RequiredAbiMinor);
    }

    [Fact]
    public void Init_With_A_Different_Config_Throws_AlreadyExists_And_Shutdown_Allows_Reinit()
    {
        AsterObservability.Init(new ObservabilityOptions { ServiceName = "first", FromEnv = false, LogBridge = false });
        Assert.True(AsterObservability.IsInitialized);
        // The same configuration joins the running pipeline.
        AsterObservability.Init(new ObservabilityOptions { ServiceName = "first", FromEnv = false, LogBridge = false });
        var ex = Assert.Throws<IrohException>(() =>
            AsterObservability.Init(new ObservabilityOptions { ServiceName = "second", FromEnv = false, LogBridge = false }));
        Assert.Equal(Status.AlreadyExists, ex.Status);

        Assert.True(AsterObservability.Shutdown());
        Assert.False(AsterObservability.IsInitialized);
        AsterObservability.Init(new ObservabilityOptions { ServiceName = "second", FromEnv = false, LogBridge = false });
        Assert.True(AsterObservability.Shutdown());
        Assert.False(AsterObservability.Shutdown());
    }

    // ─── Logs ───────────────────────────────────────────────────────────────

    [Fact]
    public async Task App_Logs_Are_Exported_Through_The_Rust_Logs_SDK_With_Trace_Ids()
    {
        using var receiver = new OtlpReceiver();
        AsterObservability.Init(new ObservabilityOptions
        {
            ServiceName = "o9-pathb", OtlpLogs = true, OtlpEndpoint = receiver.Url, FromEnv = false, LogBridge = false,
        });
        using var provider = new AsterLogEmitProvider();
        var logger = provider.CreateLogger("app.pathb");

        using var source = new ActivitySource("O9.PathB");
        using var listener = new ActivityListener
        {
            ShouldListenTo = _ => true,
            Sample = (ref ActivityCreationOptions<ActivityContext> _) => ActivitySamplingResult.AllDataAndRecorded,
        };
        ActivitySource.AddActivityListener(listener);
        using var activity = source.StartActivity("work")!;
        logger.LogWarning("pathb-marker-app {Order}", "o-1");

        // A record the Rust bridge delivered (aster.origin=rust) is not re-emitted.
        var bridged = new List<KeyValuePair<string, object?>> { new("aster.origin", "rust"), new("{OriginalFormat}", "pathb-marker-rust") };
        logger.Log(LogLevel.Information, new EventId(0), (IReadOnlyList<KeyValuePair<string, object?>>)bridged, null, (_, _) => "pathb-marker-rust");

        Assert.True(await WaitLogs(receiver, "pathb-marker-app"));
        Assert.True(receiver.LogsContain("app.pathb"u8));
        Assert.True(receiver.LogsContain("Order"u8) && receiver.LogsContain("o-1"u8));
        Assert.True(receiver.LogsContain(Convert.FromHexString(activity.TraceId.ToHexString())));
        Assert.True(receiver.LogsContain(Convert.FromHexString(activity.SpanId.ToHexString())));
        Assert.False(receiver.LogsContain("pathb-marker-rust"u8));
    }

    private static async Task<bool> WaitLogs(OtlpReceiver r, string marker)
    {
        var needle = System.Text.Encoding.UTF8.GetBytes(marker);
        for (int i = 0; i < 100 && !r.LogsContain(needle); i++)
        {
            AsterObservability.Flush(TimeSpan.FromSeconds(2));
            await Task.Delay(100);
        }
        return r.LogsContain(needle);
    }

    [Theory]
    [InlineData("http503")]
    [InlineData("refused")]
    [InlineData("stall")]
    public async Task App_Log_Export_Remains_Bounded_During_503_And_Recovers(string fault)
    {
        using var receiver = new OtlpReceiver();
        var bridge = new CapturingLoggerFactory { MinimumLevel = LogLevel.Trace };
        AsterLogging.UseLoggerFactory(bridge);
        AsterObservability.Init(new ObservabilityOptions
        {
            ServiceName = "o9-outage",
            OtlpLogs = true,
            Traces = true,
            OtlpEndpoint = receiver.Url,
            FromEnv = false,
            LogBridge = true,
            LogFilter = "warn,aster=trace",
        });
        using var provider = new AsterLogEmitProvider();
        var logger = provider.CreateLogger("app.outage");
        await using var node = await Node.StartAsync(Loopback);
        using var health = HealthServer.Start(node);
        health.SetReady(true);

        receiver.RejectLogs = true;
        receiver.StallLogs = fault == "stall";
        if (fault == "refused")
        {
            receiver.StopListening();
            using var probe = new System.Net.Sockets.TcpClient();
            Assert.Throws<System.Net.Sockets.SocketException>(() => probe.Connect(new Uri(receiver.Url).Host, new Uri(receiver.Url).Port));
        }
        int rejectedBefore = receiver.RejectedLogs;
        var emitStart = Stopwatch.StartNew();
        for (int i = 0; i < 1_000; i++)
            logger.LogWarning("dotnet-outage-{Index}", i);
        emitStart.Stop();
        Assert.True(emitStart.Elapsed < TimeSpan.FromSeconds(1), $"1,000 emissions took {emitStart.Elapsed}");

        var flushStart = Stopwatch.StartNew();
        Assert.Throws<IrohException>(() => AsterObservability.Flush(TimeSpan.FromSeconds(1)));
        flushStart.Stop();
        Assert.True(flushStart.Elapsed < TimeSpan.FromSeconds(3), $"failed flush took {flushStart.Elapsed}");
        if (fault != "refused")
            Assert.True(receiver.RejectedLogs > rejectedBefore, "the receiver did not observe a failed log export");
        Assert.False(receiver.LogsContain("dotnet-outage-"u8));
        var (readyCode, readyBody) = await ProbeAsync(health.BaseUrl + "/readyz");
        Assert.Equal(HttpStatusCode.OK, readyCode);
        Assert.Contains("\"audit\":true", readyBody.Replace(" ", ""));

        receiver.RejectLogs = false;
        if (fault == "refused") receiver.StartListening();
        using var source = new ActivitySource("O9.Outage");
        using var listener = ListenTo("O9.Outage");
        using var activity = source.StartActivity("recovered-work")!;
        AsterLogEmit.Emit(LogLevel.Warning, "app.outage", "dotnet-recovered",
            new[] { new KeyValuePair<string, string>("phase", "recovered"), new("payload.key", "payload-value") },
            activity: activity);
        Assert.True(await WaitLogs(receiver, "dotnet-recovered"));
        byte[] recovered = receiver.LogBodies.First(body => body.AsSpan().IndexOf("dotnet-recovered"u8) >= 0);
        Assert.True(recovered.AsSpan().IndexOf("app.outage"u8) >= 0);
        Assert.True(recovered.AsSpan().IndexOf("payload.key"u8) >= 0 && recovered.AsSpan().IndexOf("payload-value"u8) >= 0);
        Assert.True(recovered.AsSpan().IndexOf(Convert.FromHexString(activity.TraceId.ToHexString())) >= 0);
        Assert.True(recovered.AsSpan().IndexOf(Convert.FromHexString(activity.SpanId.ToHexString())) >= 0);
        Assert.DoesNotContain(bridge.Logs, l => l.Message.Contains("dotnet-recovered", StringComparison.Ordinal));
        Assert.Equal(HttpStatusCode.OK, (await ProbeAsync(health.BaseUrl + "/readyz")).Code);
        health.Dispose();
        await node.DisposeAsync();

        receiver.RejectLogs = true;
        if (fault == "refused") receiver.StopListening();
        logger.LogWarning("dotnet-shutdown-outage");
        var shutdownStart = Stopwatch.StartNew();
        Assert.True(AsterObservability.Shutdown(TimeSpan.FromSeconds(1)));
        shutdownStart.Stop();
        Assert.False(AsterObservability.Shutdown(TimeSpan.FromSeconds(1)));
        Assert.True(shutdownStart.Elapsed < TimeSpan.FromSeconds(3), $"outage shutdown took {shutdownStart.Elapsed}");
        receiver.RejectLogs = false;
        if (fault == "refused") receiver.StartListening();
        AsterObservability.Init(new ObservabilityOptions
        {
            FromEnv = false, ServiceName = "dotnet-reinit", Traces = false, Metrics = false,
            OtlpLogs = true, OtlpEndpoint = receiver.Url,
        });
        logger.LogWarning("dotnet-after-outage-shutdown");
        Assert.True(await WaitLogs(receiver, "dotnet-after-outage-shutdown"));
    }

    [Fact]
    public async Task Rust_Logs_Reach_ILogger_With_Trace_Ids_Of_The_Handler()
    {
        var factory = new CapturingLoggerFactory();
        AsterLogging.UseLoggerFactory(factory);
        await using var h = new Harness();
        Harness.InitPipeline(h.Receiver.Url, logs: true, filter: "warn,aster=trace");
        await h.StartAsync();
        await h.CallAsync();
        Assert.True(h.Handled.TryDequeue(out var seen));
        Assert.NotNull(seen!.TraceId);

        // Records are bridged by a poll thread: wait for the call's completion record.
        CapturedLog[] inTrace = [];
        for (int i = 0; i < 100 && inTrace.Length == 0; i++)
        {
            await Task.Delay(50);
            inTrace = factory.Logs.Where(l => l.Scope.TryGetValue("TraceId", out var t) && (string?)t == seen.TraceId).ToArray();
        }
        Assert.NotEmpty(inTrace);
        var rec = inTrace.First(l => l.Category.StartsWith("aster.rpc", StringComparison.Ordinal));
        Assert.Equal(16, ((string)rec.Scope["SpanId"]!).Length);
        Assert.Equal("rust", rec.State["aster.origin"]);
        Assert.Equal("O9Echo", rec.State["rpc.service"]);
        Assert.Equal("echo", rec.State["rpc.method"]);
        Assert.True(rec.Level is >= LogLevel.Trace and <= LogLevel.Error);
        Assert.DoesNotContain("::", rec.Category);
    }

    [Fact]
    public void Level_Sync_Follows_The_Logger_Factory()
    {
        var factory = new CapturingLoggerFactory { MinimumLevel = LogLevel.Warning };
        AsterLogging.UseLoggerFactory(factory);
        Assert.Null(AsterLogging.SyncLevel()); // no bridge open yet

        AsterObservability.Init(new ObservabilityOptions { FromEnv = false });
        Assert.Equal("warn,aster=warn", AsterLogging.SyncLevel());
        factory.MinimumLevel = LogLevel.Debug;
        Assert.Equal("warn,aster=debug", AsterLogging.SyncLevel());
        factory.MinimumLevel = LogLevel.None;
        Assert.Equal("warn,aster=off", AsterLogging.SyncLevel());
    }

    // ─── Activity bridge ────────────────────────────────────────────────────

    private static bool Has(IReadOnlyList<ExportedSpan> spans, string name) => spans.Any(s => s.Name == name);

    [Fact]
    public async Task Child_Activity_In_A_Handler_Joins_The_Aster_Trace()
    {
        await using var h = new Harness();
        Harness.InitPipeline(h.Receiver.Url);
        await h.StartAsync();
        await h.CallAsync();
        Assert.True(h.Handled.TryDequeue(out var seen));

        // The handler runs in an activity of source Aster; the child (an HttpClient-style activity
        // from another source) is its child in the same trace.
        Assert.Equal("Aster", seen!.SourceName);
        Assert.Equal(seen.TraceId, seen.ChildTraceId);
        Assert.Equal(seen.SpanId, seen.ChildParentSpanId);
        Assert.Contains(seen.TraceId!, seen.Traceparent);

        Assert.True(await h.Receiver.WaitAsync(s => Has(s, "O9Echo/echo handler")));
        var spans = h.Receiver.Spans;
        var handler = Assert.Single(spans, s => s.Name == "O9Echo/echo handler");
        Assert.Equal(seen.TraceId, handler.TraceId);
        Assert.Equal(seen.SpanId, handler.SpanId);
        Assert.Equal(seen.ParentSpanId, handler.ParentSpanId);
        // Its parent is the Rust server span of the call, in the same trace.
        var server = Assert.Single(spans, s => s.SpanId == handler.ParentSpanId);
        Assert.Equal(seen.TraceId, server.TraceId);
        Assert.Equal(2, server.Kind); // SERVER (OTLP numbering)
        Assert.Equal("aster", handler.Attributes["rpc.system"]);
        Assert.Equal("echo", handler.Attributes["rpc.method"]);
    }

    [Fact]
    public async Task Aster_Call_Inside_A_Parent_Activity_Joins_It()
    {
        await using var h = new Harness();
        using var appSource = new ActivitySource("O9.App");
        using var appListener = ListenTo("O9.App");
        Harness.InitPipeline(h.Receiver.Url);
        await h.StartAsync();

        // An ASP.NET-style request activity from a foreign source is the call's parent.
        using var request = appSource.StartActivity("POST /orders", ActivityKind.Server);
        Assert.NotNull(request);
        await h.CallAsync();
        string trace = request!.TraceId.ToHexString();
        string requestSpan = request.SpanId.ToHexString();
        request.Stop();

        Assert.True(h.Handled.TryDequeue(out var seen));
        Assert.Equal(trace, seen!.TraceId);

        bool got = await h.Receiver.WaitAsync(s => Has(s, "O9Echo/echo handler"));
        Assert.True(got, string.Join("\n", h.Receiver.Spans.Select(s => $"{s.Name} k={s.Kind} trace={s.TraceId} span={s.SpanId} parent={s.ParentSpanId}")) + $"\nexpected trace {trace} request {requestSpan}");
        var spans = h.Receiver.Spans.Where(s => s.TraceId == trace).ToList();
        // request -> Rust client span -> Rust server span -> the handler's activity
        var client = Assert.Single(spans, s => s.ParentSpanId == requestSpan);
        Assert.Equal(3, client.Kind); // CLIENT (OTLP numbering)
        var server = Assert.Single(spans, s => s.ParentSpanId == client.SpanId);
        Assert.Equal(2, server.Kind);
        var handler = Assert.Single(spans, s => s.Name == "O9Echo/echo handler");
        Assert.Equal(server.SpanId, handler.ParentSpanId);
    }

    [Fact]
    public async Task Every_Aster_Activity_Is_Exported_Exactly_Once()
    {
        await using var h = new Harness();
        Harness.InitPipeline(h.Receiver.Url);
        await h.StartAsync(async (req, ctx) =>
        {
            using (var inner = Tracing.StartSpan("inner work", ActivityKind.Internal))
            {
                inner!.SetTag("k", "v");
                await Task.Yield();
            }
            return new EchoReply { Message = req.Message };
        });
        await h.CallAsync();
        await h.CallAsync();

        Assert.True(await h.Receiver.WaitAsync(s => s.Count(x => x.Name == "inner work") >= 2 && s.Count(x => x.Name == "O9Echo/echo handler") >= 2));
        await Task.Delay(500);
        AsterObservability.Flush();
        var spans = h.Receiver.Spans;
        Assert.Equal(2, spans.Count(s => s.Name == "O9Echo/echo handler"));
        Assert.Equal(2, spans.Count(s => s.Name == "inner work"));
        // No span id is exported twice.
        Assert.Equal(spans.Count, spans.Select(s => s.SpanId).Distinct().Count());
        // Each inner span hangs under its call's handler span.
        foreach (var inner in spans.Where(s => s.Name == "inner work"))
            Assert.Contains(spans, s => s.Name == "O9Echo/echo handler" && s.SpanId == inner.ParentSpanId);
        // Activities of foreign sources are not ours to export.
        Assert.DoesNotContain(spans, s => s.Name == "GET example.com");
    }

    [Fact]
    public async Task Binding_Span_Carries_Attributes_Events_And_Status()
    {
        await using var h = new Harness();
        Harness.InitPipeline(h.Receiver.Url);

        string traceId, spanId;
        using (var span = Tracing.StartSpan("checkout", ActivityKind.Producer, [new("cart.items", 3), new("flag", true)]))
        {
            Assert.NotNull(span);
            traceId = span!.TraceId.ToHexString();
            spanId = span.SpanId.ToHexString();
            span.AddEvent(new ActivityEvent("tick", tags: new ActivityTagsCollection { ["n"] = 2 }));
            span.RecordException(new InvalidOperationException("boom"));
        }

        Assert.True(await h.Receiver.WaitAsync(s => Has(s, "checkout")));
        var exported = Assert.Single(h.Receiver.Spans, s => s.Name == "checkout");
        Assert.Equal(traceId, exported.TraceId);
        Assert.Equal(spanId, exported.SpanId);
        Assert.Equal("", exported.ParentSpanId);
        Assert.Equal(4, exported.Kind); // PRODUCER (OTLP numbering)
        Assert.Equal("3", exported.Attributes["cart.items"]);
        Assert.Equal("true", exported.Attributes["flag"]);
        Assert.Contains("tick", exported.Events);
        Assert.Contains("exception", exported.Events);
        Assert.Equal(2, exported.StatusCode); // ERROR
    }

    [Fact]
    public async Task Inbound_Baggage_Becomes_Activity_Baggage_In_The_Handler()
    {
        await using var h = new Harness();
        using var appSource = new ActivitySource("O9.App");
        using var appListener = ListenTo("O9.App");
        Harness.InitPipeline(h.Receiver.Url);
        await h.StartAsync();

        using var request = appSource.StartActivity("work", ActivityKind.Server);
        request!.AddBaggage("tenant", "acme");
        request.AddBaggage("note", "a b,c");
        await h.CallAsync();

        Assert.True(h.Handled.TryDequeue(out var seen));
        Assert.Equal("acme", seen!.Baggage["tenant"]);
        Assert.Equal("a b,c", seen.Baggage["note"]);
    }

    [Fact]
    public async Task Unsampled_Parent_Is_Not_Exported_And_Does_Not_Break_The_Call()
    {
        await using var h = new Harness();
        Harness.InitPipeline(h.Receiver.Url);
        await h.StartAsync();
        const string trace = "4bf92f3577b34da6a3ce929d0e0e4736";
        using (Tracing.WithTraceContext($"00-{trace}-00f067aa0ba902b7-00"))
            Assert.Equal("hi", (await h.CallAsync()).Message);
        // A sampled one, to prove the receiver is live.
        using (Tracing.WithTraceContext("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"))
            await h.CallAsync();
        Assert.True(await h.Receiver.WaitAsync(s => s.Any(x => x.TraceId == "0af7651916cd43dd8448eb211c80319c")));
        Assert.DoesNotContain(h.Receiver.Spans, s => s.TraceId == trace);
    }

    [Fact]
    public async Task A_Foreign_Listener_On_Source_Aster_Does_Not_Make_Us_Export_Twice()
    {
        await using var h = new Harness();
        Harness.InitPipeline(h.Receiver.Url);
        // An OpenTelemetry-SDK-style subscription that records everything, including activities whose
        // parent is not sampled: it exports them itself, so the binding must not.
        using var foreign = ListenTo("Aster");
        using (Tracing.WithTraceContext("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"))
        {
            using var span = Tracing.StartSpan("foreign recorded");
            Assert.NotNull(span);
            Assert.True(span!.Recorded);
        }
        using (var sampled = Tracing.StartSpan("ours"))
            Assert.NotNull(sampled);

        Assert.True(await h.Receiver.WaitAsync(s => Has(s, "ours")));
        Assert.DoesNotContain(h.Receiver.Spans, s => s.Name == "foreign recorded");
    }

    [Fact]
    public async Task HttpClient_Inside_A_Handler_Sends_The_Aster_Trace()
    {
        // A real HttpClient: with an Activity.Current it propagates traceparent without any listener.
        using var http = new HttpListener();
        var probe = new System.Net.Sockets.TcpListener(IPAddress.Loopback, 0);
        probe.Start();
        int port = ((IPEndPoint)probe.LocalEndpoint).Port;
        probe.Stop();
        http.Prefixes.Add($"http://127.0.0.1:{port}/");
        http.Start();
        var header = new TaskCompletionSource<string?>(TaskCreationOptions.RunContinuationsAsynchronously);
        _ = Task.Run(async () =>
        {
            var ctx = await http.GetContextAsync();
            header.TrySetResult(ctx.Request.Headers["traceparent"]);
            ctx.Response.StatusCode = 204;
            ctx.Response.Close();
        });

        // HttpClient's own activity (what OpenTelemetry's HttpClient instrumentation enables).
        var httpActivities = new ConcurrentQueue<(string Parent, string Span)>();
        using var httpListener = new ActivityListener
        {
            ShouldListenTo = s => s.Name == "System.Net.Http",
            Sample = (ref ActivityCreationOptions<ActivityContext> _) => ActivitySamplingResult.AllDataAndRecorded,
            ActivityStarted = a => httpActivities.Enqueue((a.ParentSpanId.ToHexString(), a.SpanId.ToHexString())),
        };
        ActivitySource.AddActivityListener(httpListener);

        await using var h = new Harness();
        Harness.InitPipeline(h.Receiver.Url);
        await h.StartAsync(async (req, ctx) =>
        {
            using var client = new HttpClient();
            using var resp = await client.GetAsync($"http://127.0.0.1:{port}/");
            return new EchoReply { Message = ((int)resp.StatusCode).ToString() };
        });
        var reply = await h.CallAsync();
        Assert.Equal("204", reply.Message);

        string? sent = await header.Task.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.NotNull(sent);
        Assert.True(await h.Receiver.WaitAsync(s => Has(s, "O9Echo/echo handler")));
        var handler = Assert.Single(h.Receiver.Spans, s => s.Name == "O9Echo/echo handler");
        // 00-<trace>-<span>-<flags>: the outbound call is a child of the handler's activity.
        var parts = sent!.Split('-');
        Assert.Equal(handler.TraceId, parts[1]);
        var httpAct = Assert.Single(httpActivities);
        Assert.Equal(handler.SpanId, httpAct.Parent);
        Assert.Equal(httpAct.Span, parts[2]);
        Assert.Equal("01", parts[3]);
    }

    [Fact]
    public void Inject_Adds_Current_Context_And_Keeps_Callers_Own()
    {
        Assert.Null(Tracing.Inject(null));
        using var source = new ActivitySource("O9.Inject");
        using var listener = ListenTo("O9.Inject");
        using var a = source.StartActivity("op")!;
        a.AddBaggage("k", "v w");

        var md = Tracing.Inject(null)!.ToDictionary(kv => kv.Key, kv => kv.Value);
        Assert.Equal($"00-{a.TraceId.ToHexString()}-{a.SpanId.ToHexString()}-01", md["traceparent"]);
        Assert.Equal("k=v%20w", md["baggage"]);

        var own = new List<KeyValuePair<string, string>> { new("traceparent", "00-" + new string('1', 32) + "-" + new string('2', 16) + "-00") };
        var kept = Tracing.Inject(own)!;
        Assert.Equal(own[0].Value, kept.Single(kv => kv.Key == "traceparent").Value);
        Assert.Contains(kept, kv => kv.Key == "baggage");
    }

    [Fact]
    public void Baggage_Round_Trips_And_Respects_Limits()
    {
        var parsed = Tracing.ParseBaggage("tenant=acme;prop=1, note=a%20b%2Cc ,bad, =x");
        Assert.Equal([new("tenant", "acme"), new("note", "a b,c")], parsed);
        Assert.Equal("a=1,b=x%20y", Tracing.FormatBaggage([new("a", "1"), new("b", "x y"), new("a", "dup")]));
        Assert.Null(Tracing.FormatBaggage([]));
        var many = Enumerable.Range(0, 100).Select(i => new KeyValuePair<string, string?>($"k{i}", "v"));
        Assert.Equal(64, Tracing.FormatBaggage(many)!.Split(',').Length);
    }

    // ─── Metrics and Prometheus ─────────────────────────────────────────────

    private static void MatchesIn(string text, string pattern)
        => Assert.True(System.Text.RegularExpressions.Regex.IsMatch(text, pattern),
            $"/{pattern}/ not found in:\n" + string.Join("\n", text.Split('\n').Where(l => l.StartsWith("o9_", StringComparison.Ordinal))));

    private static async Task<string> GetAsync(string url)
    {
        using var http = new HttpClient();
        return await http.GetStringAsync(url);
    }

    [Fact]
    public async Task Custom_Metrics_Show_In_Prometheus_Text_And_Survive_A_Reinit()
    {
        Assert.Null(AsterObservability.MetricsText()); // no pull reader yet
        await using var node = await Node.StartAsync(Loopback);
        using var health = HealthServer.Start(node);

        var orders = Metrics.Counter("o9.orders.placed", "Orders placed");
        var depth = Metrics.UpDownCounter("o9.queue.depth", "Queue depth");
        var temp = Metrics.Gauge("o9.temp", "Temperature", "Cel");
        var latency = Metrics.Histogram("o9.latency", "Latency", "s", boundaries: [0.5, 1.0]);
        var eu = Metrics.Attributes(("region", "eu"));

        orders.Add(2, ("region", "eu"), ("ok", true));
        orders.Add(1, eu);
        depth.Add(5);
        depth.Add(-2);
        temp.Record(21.5);
        latency.Record(0.7, ("route", "/a"));
        Assert.Throws<ArgumentOutOfRangeException>(() => orders.Add(-1));
        Assert.Throws<IrohException>(() => Metrics.UpDownCounter("o9.orders.placed")); // one kind per name

        string text = await GetAsync(health.BaseUrl + "/metrics");
        Assert.Contains("o9_orders_placed_total{", text);
        MatchesIn(text, "o9_orders_placed_total\\{[^}]*ok=\"true\"[^}]*region=\"eu\"[^}]*\\} 2");
        MatchesIn(text, "o9_orders_placed_total\\{[^}]*region=\"eu\"[^}]*\\} 1");
        Assert.Contains("o9_queue_depth", text);
        MatchesIn(text, "o9_queue_depth(\\{[^}]*\\})? 3");
        MatchesIn(text, "o9_temp_celsius(\\{[^}]*\\})? 21.5");
        Assert.Contains("le=\"0.5\"", text);
        Assert.Contains("le=\"1\"", text);
        Assert.Contains("o9_latency_seconds_count", text);

        // Node.MetricsPrometheus is the same scrape.
        Assert.Contains("o9_orders_placed_total", node.MetricsPrometheus());

        // The pipeline going away frees every handle; the objects declare themselves again.
        health.Dispose();
        AsterObservability.Shutdown();
        orders.Add(1, eu);
        eu.Dispose();
        orders.Dispose();
    }

    // ─── Health ─────────────────────────────────────────────────────────────

    private static async Task<(HttpStatusCode Code, string Body)> ProbeAsync(string url)
    {
        using var http = new HttpClient();
        using var resp = await http.GetAsync(url);
        return (resp.StatusCode, await resp.Content.ReadAsStringAsync());
    }

    [Fact]
    public async Task Health_Endpoints_Follow_Readiness_And_Checks()
    {
        await using var node = await Node.StartAsync(Loopback);
        using var health = HealthServer.Start(node);
        Assert.StartsWith("127.0.0.1:", health.Address);

        Assert.Equal(HttpStatusCode.OK, (await ProbeAsync(health.BaseUrl + "/healthz")).Code);
        Assert.Equal(HttpStatusCode.ServiceUnavailable, (await ProbeAsync(health.BaseUrl + "/readyz")).Code);

        health.SetReady(true);
        Assert.Equal(HttpStatusCode.OK, (await ProbeAsync(health.BaseUrl + "/readyz")).Code);

        var db = health.Check("db"); // created failing
        var (code, body) = await ProbeAsync(health.BaseUrl + "/readyz");
        Assert.Equal(HttpStatusCode.ServiceUnavailable, code);
        Assert.Contains("\"db\":false", body.Replace(" ", ""));
        db.Set(true);
        Assert.Equal(HttpStatusCode.OK, (await ProbeAsync(health.BaseUrl + "/readyz")).Code);

        health.SetHealthy(false);
        Assert.Equal(HttpStatusCode.ServiceUnavailable, (await ProbeAsync(health.BaseUrl + "/healthz")).Code);

        string url = health.BaseUrl;
        health.Dispose(); // the port is free on return
        await Assert.ThrowsAnyAsync<HttpRequestException>(() => GetAsync(url + "/healthz"));
        Assert.Throws<ObjectDisposedException>(() => health.Address);
    }

    [Fact]
    public async Task Server_Attaches_Health_Ready_After_Start_And_Not_Ready_On_Dispose()
    {
        await using var h = new Harness();
        await h.StartAsync(health: new HealthOptions());
        var health = h.Server.Health!;
        string url = health.BaseUrl;

        Assert.Equal(HttpStatusCode.OK, (await ProbeAsync(url + "/readyz")).Code);
        const string completed = "aster_rpc_server_requests_completed_total";
        Assert.DoesNotContain(completed, await GetAsync(url + "/metrics"));
        await h.CallAsync();

        // The native server queues the response/trailer before recording completion.
        // Client receipt is not a barrier for that final server-side metric.
        const string completedCall = "(?m)^aster_rpc_server_requests_completed_total\\{(?=[^}]*rpc_service=\"O9Echo\")(?=[^}]*rpc_method=\"echo\")(?=[^}]*aster_status=\"OK\")[^}]*\\} 1$";
        var wait = Stopwatch.StartNew();
        string text = await GetAsync(url + "/metrics");
        while (!System.Text.RegularExpressions.Regex.IsMatch(text, completedCall)
            && wait.Elapsed < TimeSpan.FromSeconds(2))
        {
            await Task.Delay(20);
            text = await GetAsync(url + "/metrics");
        }
        Assert.Matches(completedCall, text); // the successful call is counted exactly once

        await h.Server.DisposeAsync();
        await Assert.ThrowsAnyAsync<HttpRequestException>(() => GetAsync(url + "/readyz"));
    }

    [Fact]
    public async Task Server_Starts_Health_From_Config()
    {
        var svc = ServiceDefinition.Create("O9Echo", 1)
            .Unary<EchoRequest, EchoReply>("echo", (req, _) => ValueTask.FromResult(new EchoReply { Message = req.Message }))
            .Build();
        await using var server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = Loopback with { HealthEnabled = true }, // port 0 + enabled: ephemeral
            Services = { svc },
        });
        var health = server.Health;
        Assert.NotNull(health);
        Assert.StartsWith("127.0.0.1:", health!.Address);
        Assert.Equal(HttpStatusCode.OK, (await ProbeAsync(health.BaseUrl + "/healthz")).Code);
        var ready = await ProbeAsync(health.BaseUrl + "/readyz");
        Assert.Equal(HttpStatusCode.OK, ready.Code);
        Assert.Contains("\"services\":1", ready.Body.Replace(" ", ""));
        var cache = health.Check("config-cache");
        cache.Set(false);
        var failed = await ProbeAsync(health.BaseUrl + "/readyz");
        Assert.Equal(HttpStatusCode.ServiceUnavailable, failed.Code);
        Assert.Contains("\"config-cache\":false", failed.Body.Replace(" ", ""));
        cache.Set(true);
        Assert.Equal(HttpStatusCode.OK, (await ProbeAsync(health.BaseUrl + "/readyz")).Code);
    }


    [Fact]
    public async Task Health_Config_Live_Disabled_Bind_Precedence_And_Reuse()
    {
        using var occupied = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, 0);
        occupied.Start();
        var port = (ushort)((System.Net.IPEndPoint)occupied.LocalEndpoint).Port;
        var disabled = Loopback with { HealthEnabled = false, HealthPort = port };
        await using (var off = await AsterServer.StartAsync(new AsterServerOptions { Config = disabled }))
            Assert.Null(off.Health);
        var conflict = disabled with { HealthEnabled = true };
        await Assert.ThrowsAnyAsync<Exception>(() => AsterServer.StartAsync(new AsterServerOptions { Config = conflict }));
        var explicitServer = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = conflict,
            Health = new Aster.Health.HealthOptions { Host = "127.0.0.1", Port = 0 },
        });
        int actual;
        try
        {
            Assert.NotNull(explicitServer.Health);
            actual = int.Parse(explicitServer.Health!.Address.Split(':')[1]);
            Assert.NotEqual((int)port, actual);
            Assert.Equal(HttpStatusCode.OK, (await ProbeAsync(explicitServer.Health.BaseUrl + "/readyz")).Code);
        }
        finally { await explicitServer.DisposeAsync(); }
        using var rebound = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, actual);
        rebound.Server.SetSocketOption(System.Net.Sockets.SocketOptionLevel.Socket, System.Net.Sockets.SocketOptionName.ReuseAddress, true);
        rebound.Start();
        Assert.Equal(actual, ((System.Net.IPEndPoint)rebound.LocalEndpoint).Port);
    }

    [Fact]
    public void Health_Config_Defaults_And_Toml()
    {
        var d = new AsterConfig();
        Assert.Equal((ushort)0, d.HealthPort);
        Assert.Equal("127.0.0.1", d.HealthHost);
        Assert.Null(d.HealthEnabled);
        Assert.False(d.IsHealthEnabled);
        Assert.True((d with { HealthPort = 8080 }).IsHealthEnabled);
        Assert.False((d with { HealthPort = 8080, HealthEnabled = false }).IsHealthEnabled);

        string path = System.IO.Path.Combine(System.IO.Path.GetTempPath(), $"aster-{Guid.NewGuid():N}.toml");
        System.IO.File.WriteAllText(path, "[health]\nport = 9091\nhost = \"10.0.0.5\"\nenabled = false\n");
        try
        {
            var c = AsterConfig.FromToml(path);
            Assert.Equal((ushort)9091, c.HealthPort);
            Assert.Equal("10.0.0.5", c.HealthHost);
            Assert.False(c.HealthEnabled);
        }
        finally { System.IO.File.Delete(path); }
    }

    [Fact]
    public void Prometheus_Accessor_Is_Null_Without_A_Pull_Reader()
    {
        Assert.Null(AsterObservability.MetricsText());
    }
    [Fact]
    public async Task Concurrent_Trace_Carriers_Are_Isolated_And_Restored()
    {
        var release = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        var entered = new[] { new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously), new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously) };
        var original = Tracing.CurrentTraceId;
        async Task Worker(int index)
        {
            var trace = new string(index == 0 ? '1' : '2', 32);
            using (Tracing.WithTraceContext($"00-{trace}-00f067aa0ba902b7-01", new Dictionary<string, string> { ["tenant"] = trace }))
            {
                entered[index].SetResult(true);
                await release.Task.WaitAsync(TimeSpan.FromSeconds(5));
                await Task.Yield();
                Assert.Equal(trace, Tracing.CurrentTraceId);
                Assert.Equal(trace, Tracing.CurrentBaggage["tenant"]);
            }
            Assert.Equal(original, Tracing.CurrentTraceId);
        }
        var tasks = new[] { Worker(0), Worker(1) };
        try
        {
            await Task.WhenAll(entered[0].Task, entered[1].Task).WaitAsync(TimeSpan.FromSeconds(5));
            Assert.Equal(original, Tracing.CurrentTraceId);
            release.SetResult(true);
            await Task.WhenAll(tasks).WaitAsync(TimeSpan.FromSeconds(5));
            Assert.Equal(original, Tracing.CurrentTraceId);
        }
        finally { release.TrySetResult(true); await Task.WhenAll(tasks); }
    }

}

[CollectionDefinition("Observability", DisableParallelization = true)]
public class ObservabilityCollection;
