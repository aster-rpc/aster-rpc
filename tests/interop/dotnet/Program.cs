// The .NET side of the InteropEcho contract (tests/interop/): a leg of the
// tests/interop/run.py matrix (contract / server / client).
//
// Contract (must match tests/interop/rust/contract.rs):
//
//   service InteropEcho v1
//     echo(interop/EchoRequest) -> interop/EchoReply                (unary)
//     generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
//     ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
//     chat(stream interop/EchoRequest) -> stream interop/EchoReply  (bidi)
//     hold(interop/EchoRequest) -> interop/EchoReply                (unary, blocks)
//     cancelled(interop/EchoRequest) -> interop/EchoReply           (unary)
//     traceEcho(interop/EchoRequest) -> interop/EchoReply           (unary)
//   service InteropSession v1 (scope = session): add(interop/EchoRequest) -> interop/EchoReply
//   (semantics: tests/interop/rust/contract.rs)
//   interop/EchoRequest { message: string, count: varint32, data: binary }
//   interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }
//
// Usage: dotnet InteropEcho.dll contract | server | chain-server | client <aster1-ticket> | expose-server
// expose-server: ALPN aster-expose/interop/1, HTTP service interop-http (GET /hello, POST /echo
// streamed back, header x-server-lang), prints ADDRESS <aster1...>, serves until stdin closes.
// Every mode prints CONTRACT_ID <hex> first; `server` prints ADDRESS <aster1...> and serves
// until stdin closes (loopback, relays off); `client` prints CASE <name> OK|FAIL lines.

using System.Runtime.CompilerServices;
using System.Diagnostics;
using System.Text.Json;
using System.Threading.Channels;
using Apache.Fory;
using Aster;
using Aster.Expose;
using Aster.Interceptors;
using Aster.Rpc;
using Aster.Upcall;

var service = InteropEcho.Service("dotnet", args is ["chain-server"]);
var session = InteropEcho.SessionService("dotnet");
Console.WriteLine($"CONTRACT_ID {service.ContractId}");
Console.WriteLine($"SESSION_CONTRACT_ID {session.ContractId}");

switch (args)
{
    case ["contract"]:
        return 0;
    case ["server"]:
    case ["chain-server"]:
        await InteropEcho.ServeAsync(service, session);
        return 0;
    case ["expose-server"]:
        await InteropEcho.ExposeServeAsync();
        return 0;
    case ["client", var ticket]:
        return await InteropEcho.ClientAsync(service, session, ticket) ? 0 : 1;
    case ["campaign-client", var ticket]:
        await InteropEcho.CampaignClientAsync(service, ticket);
        return 0;
    default:
        Console.Error.WriteLine("usage: InteropEcho contract | server | chain-server | client <aster1-ticket> | expose-server");
        return 2;
}

[ForyStruct]
[WireType("interop/EchoRequest")]
public sealed class EchoRequest
{
    [WireName("message")] public string Message { get; set; } = "";
    [WireName("count")] public int Count { get; set; }
    [WireName("data")] public byte[] Data { get; set; } = [];
}

[ForyStruct]
[WireType("interop/EchoReply")]
public sealed class EchoReply
{
    [WireName("message")] public string Message { get; set; } = "";
    [WireName("count")] public int Count { get; set; }
    [WireName("data")] public byte[] Data { get; set; } = [];
    [WireName("server_lang")] public string ServerLang { get; set; } = "";
}

internal static class InteropEcho
{
    private sealed class CampaignStats
    {
        public readonly object Mutex = new();
        public long Completed, Attempted, Unary, Stream, Cancel, Unexpected, Integrity, Emitted, EmitErrors;
        public long FirstSuccess = -1, LastSuccess = -1;
        public readonly long[] Histogram = new long[27];
        public readonly Dictionary<string, long> Errors = new();
        public readonly Dictionary<string, string> ErrorSamples = new();
        public int Active, High;
    }

    private static string CampaignError(Exception error) => error switch
    {
        OperationCanceledException => "cancelled",
        TimeoutException => "local_timeout",
        RpcError rpc when rpc.Code == StatusCode.Cancelled => "cancelled",
        RpcError rpc when rpc.Code == StatusCode.DeadlineExceeded => "deadline_exceeded",
        RpcError rpc when rpc.Code == StatusCode.Unavailable => "unavailable",
        _ => "local_error",
    };

    private static bool CampaignExpected(JsonElement command, string error)
    {
        if (!command.TryGetProperty("phase", out var phase) || phase.GetString() != "fault") return false;
        if (error is not ("unavailable" or "deadline_exceeded" or "cancelled")) return false;
        return command.TryGetProperty("expected_errors", out var expected)
            && expected.EnumerateArray().Any(item => item.GetString() == error);
    }

    private static async Task<object> CampaignWindowAsync(RpcConnection conn, JsonElement command, int generation, string ownerIdentity, bool emit)
    {
        double duration = command.GetProperty("duration_s").GetDouble();
        int slowMs = command.TryGetProperty("slow_ms", out var slow) ? slow.GetInt32() : 0;
        if (!(duration > 0 && duration <= 3600) || slowMs < 0 || slowMs > 300)
            throw new ArgumentException("invalid campaign duration/slow_ms");
        var stats = new CampaignStats();
        string windowId = command.GetProperty("window_id").GetString()!;
        if (System.Text.Encoding.UTF8.GetByteCount(windowId) > 128) throw new ArgumentException("window_id too long");
        string marker = $"campaign:dotnet:{windowId};";
        byte[] data = Enumerable.Range(0, 16384).Select(i => (byte)((i * 131 + i / 8) % 251)).ToArray();
        var jobs = Channel.CreateBounded<long>(new BoundedChannelOptions(128) { FullMode = BoundedChannelFullMode.Wait });
        using var admission = new CancellationTokenSource(TimeSpan.FromSeconds(duration));
        var started = Stopwatch.StartNew();
        Task[] workers = Enumerable.Range(0, 4).Select(_ => Task.Run(async () =>
        {
            while (!admission.IsCancellationRequested)
            {
                long sequence;
                try { sequence = await jobs.Reader.ReadAsync(admission.Token); }
                catch (OperationCanceledException) { break; }
                if (admission.IsCancellationRequested) break;
                Interlocked.Increment(ref stats.Active);
                long callStarted = Stopwatch.GetTimestamp();
                int kind = (int)(sequence % 4);
                bool bad = false, success = false;
                Exception? failure = null;
                // The token is a harness watchdog; the CallOptions deadline is
                // the public SDK budget and must win boundary races.
                using var call = new CancellationTokenSource(TimeSpan.FromSeconds(kind == 3 ? 10 : 3));
                var options = new CallOptions { Deadline = TimeSpan.FromSeconds(kind == 3 ? 10 : 2) };
                var request = new EchoRequest { Message = "campaign", Count = kind == 1 ? 5 : 41, Data = data };
                try
                {
                    if (kind == 3)
                    {
                        call.CancelAfter(50); // short-wait-ok: campaign mode: expiry is a classified outcome shared by every language leg (the planned 50 ms cancel)
                        await conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "hold", request, options, call.Token);
                        bad = true;
                    }
                    else if (kind == 1)
                    {
                        int count = 0;
                        await foreach (var reply in conn.ServerStreamAsync<EchoRequest, EchoReply>("InteropEcho", "generate", request, options, call.Token))
                        {
                            if (reply.Count != count || reply.Message != request.Message || !reply.Data.AsSpan().SequenceEqual(data) || reply.ServerLang != "rust") bad = true;
                            count++;
                            if (slowMs > 0) await Task.Delay(slowMs, call.Token);
                        }
                        if (count != 5) bad = true;
                    }
                    else
                    {
                        var reply = await conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "echo", request, options, call.Token);
                        bad = reply.Count != 42 || reply.Message != request.Message || !reply.Data.AsSpan().SequenceEqual(data) || reply.ServerLang != "rust";
                    }
                    success = !bad;
                }
                catch (Exception error) { failure = error; }
                finally { Interlocked.Decrement(ref stats.Active); }
                lock (stats.Mutex)
                {
                    stats.Attempted++;
                    if (bad) stats.Integrity++;
                    if (failure is not null)
                    {
                        string error = kind != 3 && failure is OperationCanceledException && call.IsCancellationRequested
                            ? "local_timeout" : CampaignError(failure);
                        stats.Errors[error] = stats.Errors.GetValueOrDefault(error) + 1;
                        string sample = failure.ToString();
                        stats.ErrorSamples[error] = sample[..Math.Min(1024, sample.Length)];
                        if (kind == 3 && error == "cancelled") success = true;
                        else if (!CampaignExpected(command, error)) stats.Unexpected++;
                    }
                    if (success)
                    {
                        stats.Completed++;
                        if (kind == 3) stats.Cancel++; else if (kind == 1) stats.Stream++; else stats.Unary++;
                        long micros = Math.Max(1, (long)(Stopwatch.GetElapsedTime(callStarted).TotalMilliseconds * 1000));
                        int bin = 0;
                        while (bin < 26 && micros > (1L << bin)) bin++;
                        stats.Histogram[bin]++;
                        if (kind != 3)
                        {
                            if (stats.FirstSuccess < 0) stats.FirstSuccess = started.ElapsedMilliseconds;
                            stats.LastSuccess = started.ElapsedMilliseconds;
                        }
                    }
                }
                if (success && emit)
                {
                    try
                    {
                        Aster.Observability.AsterLogEmit.Emit(Microsoft.Extensions.Logging.LogLevel.Information, "app.campaign", marker);
                        lock (stats.Mutex) stats.Emitted++;
                    }
                    catch (Exception) { lock (stats.Mutex) { stats.EmitErrors++; stats.Unexpected++; } }
                }
            }
        })).ToArray();
        long sequence = 0;
        try
        {
            while (!admission.IsCancellationRequested)
            {
                await jobs.Writer.WriteAsync(sequence++, admission.Token);
                int depth = jobs.Reader.Count;
                for (int old = Volatile.Read(ref stats.High); depth > old; old = Volatile.Read(ref stats.High))
                    if (Interlocked.CompareExchange(ref stats.High, depth, old) == old) break;
            }
        }
        catch (OperationCanceledException) { }
        // short-wait-ok: campaign mode: expiry is a classified outcome shared by every language leg (4 s worker drain, as in the Rust leg)
        await Task.WhenAll(workers).WaitAsync(TimeSpan.FromSeconds(4));
        int discarded = jobs.Reader.Count;
        while (jobs.Reader.TryRead(out _)) { }
        int flushErrors = 0;
        string flushClass = "", flushMessage = "";
        long flushStarted = Stopwatch.GetTimestamp();
        if (emit)
        {
            try { Aster.Observability.AsterObservability.Flush(TimeSpan.FromMilliseconds(500)); }
            catch (Exception error)
            {
                flushErrors++;
                flushClass = error is IrohException nativeError && nativeError.Status == Status.Timeout ? "timeout" : "sdk";
                flushMessage = error.ToString();
                bool expected = command.TryGetProperty("phase", out var phase) && phase.GetString() == "fault"
                    && command.TryGetProperty("expected_flush_errors", out var errors)
                    && errors.EnumerateArray().Any(item => item.GetString() == flushClass);
                if (!expected) stats.Unexpected++;
            }
        }
        return new
        {
            phase = "campaign_window", language = "dotnet", pid = Environment.ProcessId, owner_identity = ownerIdentity, generation, duration_s = duration, elapsed_s = started.Elapsed.TotalSeconds,
            window_id = windowId, log_marker = marker,
            completed = stats.Completed, attempted = stats.Attempted, unary = stats.Unary, stream = stats.Stream, cancel = stats.Cancel,
            unexpected_errors = stats.Unexpected, integrity_failures = stats.Integrity, active_calls = stats.Active,
            queue_scope = "adapter_admission", queue_capacity = 128, queue_high_water = stats.High, queue_depth = jobs.Reader.Count, queue_discarded = discarded,
            latency_histogram = new { unit = "us", upper_bounds_powers_of_two = true, counts = stats.Histogram }, error_classes = stats.Errors,
            first_success_elapsed_ms = stats.FirstSuccess, last_success_elapsed_ms = stats.LastSuccess,
            app_log_emitted = stats.Emitted, app_log_emit_errors = stats.EmitErrors, collector_flush_errors = flushErrors,
            error_samples = stats.ErrorSamples, flush_completed = emit && flushErrors == 0,
            flush_elapsed_ms = Stopwatch.GetElapsedTime(flushStarted).TotalMilliseconds, flush_error_class = flushClass, flush_error_message = flushMessage,
        };
    }

    public static async Task CampaignClientAsync(ServiceDefinition service, string ticket)
    {
        string? collector = Environment.GetEnvironmentVariable("ASTER_CAMPAIGN_COLLECTOR");
        bool emit = !string.IsNullOrEmpty(collector);
        if (emit) Aster.Observability.AsterObservability.Init(new Aster.Observability.ObservabilityOptions
        { ServiceName = "dotnet-campaign", OtlpEndpoint = collector, OtlpLogs = true, FromEnv = false });
        try
        {
            await using var client = await AsterClient.StartAsync(Config());
            client.RegisterTypes(service);
            AsterTicket peer = AsterTicket.Parse(ticket);
            async Task<RpcConnection> Connect(AsterTicket target)
            {
                // short-wait-ok: campaign mode: expiry is a classified outcome shared by every language leg (2 s connect, as in the Rust leg)
                using var deadline = new CancellationTokenSource(TimeSpan.FromSeconds(2)); // short-wait-ok: same 2 s campaign connect bound
                return await client.ConnectAsync(target, deadline.Token).WaitAsync(TimeSpan.FromSeconds(2));
            }
            RpcConnection conn = await Connect(peer);
            string ownerIdentity = client.Node.NodeId();
            int generation = 0;
            void Reply(object response) { Console.WriteLine(JsonSerializer.Serialize(response)); Console.Out.Flush(); }
            Reply(new { phase = "campaign_ready", language = "dotnet", pid = Environment.ProcessId, owner_identity = ownerIdentity, generation, active_calls = 0, collector_enabled = emit });
            for (string? line; (line = await Console.In.ReadLineAsync()) is not null;)
            {
                using var document = JsonDocument.Parse(line);
                var command = document.RootElement;
                string? name = command.TryGetProperty("cmd", out var cmd) ? cmd.GetString()
                    : command.TryGetProperty("command", out var alias) ? alias.GetString() : command.GetProperty("op").GetString();
                switch (name)
                {
                    case "baseline":
                    case "run": Reply(await CampaignWindowAsync(conn, command, generation, ownerIdentity, emit)); break;
                    case "pause": Reply(new { phase = "campaign_idle", generation, active_calls = 0, queue_depth = 0, unexpected_errors = 0, pid = Environment.ProcessId, owner_identity = ownerIdentity }); break;
                    case "replace_peer":
                        var replacement = AsterTicket.Parse(command.GetProperty("ticket").GetString()!);
                        if (replacement.ToNodeAddr().EndpointId == peer.ToNodeAddr().EndpointId) throw new ArgumentException("replace_peer requires fresh peer identity");
                        await conn.DisposeAsync();
                        conn = await Connect(replacement);
                        peer = replacement; generation = command.GetProperty("generation").GetInt32();
                        Reply(new { phase = "campaign_peer", generation, active_calls = 0, pid = Environment.ProcessId, owner_identity = ownerIdentity }); break;
                    case "stop":
                        await conn.DisposeAsync();
                        Reply(new { phase = "campaign_stopped", generation, active_calls = 0, unexpected_errors = 0, pid = Environment.ProcessId, owner_identity = ownerIdentity }); return;
                    default: throw new ArgumentException($"unknown campaign command {name}");
                }
            }
            await conn.DisposeAsync();
        }
        finally { if (emit) Aster.Observability.AsterObservability.Shutdown(TimeSpan.FromSeconds(2)); }
    }

    /// <summary>The service (same semantics in every language).</summary>
    public static ServiceDefinition Service(string lang, bool chain = false) => ServiceDefinition.Create("InteropEcho", 1)
        .Unary<EchoRequest, EchoReply>("echo", (req, _) => ValueTask.FromResult(new EchoReply
        {
            Message = req.Message,
            Count = unchecked(req.Count + 1),
            Data = req.Data,
            ServerLang = lang,
        }))
        .ServerStream<EchoRequest, EchoReply>("generate", Generate)
        .ClientStream<EchoRequest, EchoReply>("ingest", async (reqs, ctx) =>
        {
            int n = 0;
            string last = "";
            await foreach (var r in reqs.WithCancellation(ctx.CancellationToken))
            {
                n = unchecked(n + 1);
                last = r.Message;
            }
            return new EchoReply { Message = last, Count = n, Data = [], ServerLang = lang };
        })
        .BidiStream<EchoRequest, EchoReply>("chat", Chat)
        .Unary<EchoRequest, EchoReply>("hold", async (_, ctx) =>
        {
            if (chain) Volatile.Write(ref _chainHoldReceipt, ChainReceipt(ctx));
            try
            {
                await Task.Delay(TimeSpan.FromSeconds(60), ctx.CancellationToken);
            }
            catch (OperationCanceledException)
            {
                Interlocked.Increment(ref _holdCancelled);
                throw;
            }
            return new EchoReply { Message = "timeout", Count = -1, Data = [], ServerLang = lang };
        })
        .Unary<EchoRequest, EchoReply>("cancelled", (_, _) => ValueTask.FromResult(new EchoReply
        {
            Message = "cancelled",
            Count = Volatile.Read(ref _holdCancelled),
            Data = chain ? System.Text.Encoding.UTF8.GetBytes(Volatile.Read(ref _chainHoldReceipt)) : [],
            ServerLang = lang,
        }))
        // The handler runs in an Activity parented on the call's server span (Activity bridge).
        .Unary<EchoRequest, EchoReply>("traceEcho", (_, ctx) => ValueTask.FromResult(new EchoReply
        {
            Message = chain ? ChainReceipt(ctx) : Aster.Observability.Tracing.CurrentTraceId ?? "",
            Count = 0,
            Data = [],
            ServerLang = lang,
        }))
        .Build();

    /// <summary>`hold` calls that observed a cancellation (process-wide).</summary>
    private static int _holdCancelled;
    private static string _chainHoldReceipt = "";

    private static string ChainReceipt(ServerCallContext ctx)
    {
        var baggage = Aster.Observability.Tracing.CurrentBaggage;
        long remaining = ctx.Deadline is { } deadline
            ? Math.Max(0, (long)(deadline - DateTimeOffset.UtcNow).TotalMilliseconds) : -1;
        return $"dotnet|{Aster.Observability.Tracing.CurrentTraceId}|{baggage.GetValueOrDefault("tenant", "")}|{baggage.GetValueOrDefault("request", "")}|{remaining}";
    }

    private sealed class SessionState
    {
        public int Total;
        public int Calls;
    }

    /// <summary>The session-scoped service (one instance per connection + session id).</summary>
    public static ServiceDefinition SessionService(string lang) => ServiceDefinition.Create("InteropSession", 1, ServiceScope.Session)
        .SessionFactory(_ => new SessionState())
        .Unary<EchoRequest, EchoReply>("add", (req, ctx) =>
        {
            var st = (SessionState)ctx.Session!;
            st.Total = unchecked(st.Total + req.Count);
            st.Calls++;
            return ValueTask.FromResult(new EchoReply { Message = $"calls={st.Calls}", Count = st.Total, Data = [], ServerLang = lang });
        })
        .Build();

    private static async IAsyncEnumerable<EchoReply> Chat(IAsyncEnumerable<EchoRequest> reqs, ServerCallContext ctx)
    {
        int i = 0;
        await foreach (var r in reqs.WithCancellation(ctx.CancellationToken))
            yield return new EchoReply { Message = r.Message, Count = i++, Data = r.Data, ServerLang = "dotnet" };
    }

    private static async IAsyncEnumerable<EchoReply> Generate(EchoRequest req, ServerCallContext ctx)
    {
        for (int i = 0; i < Math.Max(req.Count, 0); i++)
        {
            yield return new EchoReply { Message = req.Message, Count = i, Data = req.Data, ServerLang = "dotnet" };
            await Task.Yield();
        }
    }

    private static AsterConfig Config() => AsterConfig.FromEnv() with { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    public static async Task ServeAsync(ServiceDefinition service, ServiceDefinition session)
    {
        await using var server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = Config(),
            Services = { service, session },
        });
        var ticket = await server.TicketAsync();
        Console.Error.WriteLine($"node addr: {server.Node.NodeAddr()}");
        Console.WriteLine($"ADDRESS {ticket}");
        Console.Out.Flush();
        while (await Console.In.ReadLineAsync() != null)
        {
            // serve until stdin closes
        }
    }

    /// <summary>The shared cases. Must match cases() in the other drivers.</summary>
    private static IEnumerable<(string Name, EchoRequest Request)> Cases() =>
    [
        ("small", new EchoRequest { Message = "hello from dotnet", Count = 41, Data = [0, 1, 2, 255] }),
        // > 4096 bytes on the wire and highly compressible: exercises zstd.
        ("zstd", new EchoRequest
        {
            Message = string.Concat(Enumerable.Repeat("héllo ✓ ", 200)),
            Count = -123_456,
            Data = Enumerable.Range(0, 20_000).Select(i => (byte)(i % 251)).ToArray(),
        }),
        // A single string field longer than 4095 bytes (Fory string-length edge).
        ("long_string", new EchoRequest { Message = new string('x', 5000), Count = int.MaxValue - 1, Data = [] }),
    ];

    private static string Check(EchoRequest req, EchoReply reply)
    {
        if (reply.Message != req.Message)
            throw new InvalidOperationException($"message mismatch (len {reply.Message.Length} vs {req.Message.Length})");
        if (reply.Count != unchecked(req.Count + 1))
            throw new InvalidOperationException($"count {reply.Count} != {unchecked(req.Count + 1)}");
        if (!reply.Data.AsSpan().SequenceEqual(req.Data))
            throw new InvalidOperationException($"data mismatch (len {reply.Data.Length} vs {req.Data.Length})");
        if (string.IsNullOrEmpty(reply.ServerLang))
            throw new InvalidOperationException("empty server_lang");
        return reply.ServerLang;
    }

    private static async Task<bool> RunCase(string name, Func<Task<string>> body)
    {
        try
        {
            using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(15));
            string lang = await body().WaitAsync(timeout.Token);
            Console.WriteLine($"CASE {name} OK server_lang={lang}");
            return true;
        }
        catch (Exception e)
        {
            Console.WriteLine($"CASE {name} FAIL {e.GetType().Name}: {e.Message}");
            return false;
        }
        finally
        {
            Console.Out.Flush();
        }
    }

    public static async Task<bool> ClientAsync(ServiceDefinition service, ServiceDefinition session, string ticketText)
    {
        await using var client = await AsterClient.StartAsync(Config());
        client.RegisterTypes(service);
        client.RegisterTypes(session);
        RpcConnection conn;
        try
        {
            conn = await client.ConnectAsync(AsterTicket.Parse(ticketText)).WaitAsync(TimeSpan.FromSeconds(30));
        }
        catch (Exception e)
        {
            Console.WriteLine($"CASE connect FAIL {e.GetType().Name}: {e.Message}");
            return false;
        }

        bool ok = true;
        foreach (var (name, req) in Cases())
            ok &= await RunCase(name, async () =>
                Check(req, await conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "echo", req)));

        ok &= await RunCase("server_stream", async () =>
        {
            var req = new EchoRequest { Message = "stream", Count = 3, Data = [7, 8, 9] };
            var got = new List<EchoReply>();
            await foreach (var r in conn.ServerStreamAsync<EchoRequest, EchoReply>("InteropEcho", "generate", req))
                got.Add(r);
            if (got.Count != 3) throw new InvalidOperationException($"{got.Count} replies != 3");
            for (int i = 0; i < 3; i++)
                if (got[i].Count != i || got[i].Message != req.Message || !got[i].Data.AsSpan().SequenceEqual(req.Data))
                    throw new InvalidOperationException($"reply {i} mismatch");
            return got[0].ServerLang;
        });

        ok &= await RunCase("client_stream", async () =>
        {
            var r = await conn.ClientStreamAsync<EchoRequest, EchoReply>("InteropEcho", "ingest", Requests());
            if (r.Count != 3 || r.Message != "m2") throw new InvalidOperationException($"reply mismatch: {r.Count} {r.Message}");
            if (string.IsNullOrEmpty(r.ServerLang)) throw new InvalidOperationException("empty server_lang");
            return r.ServerLang;
        });

        ok &= await RunCase("bidi", () => ChatCase(conn));
        ok &= await RunCase("deadline", () => DeadlineCase(conn));
        ok &= await RunCase("cancel", () => CancelCase(conn));
        ok &= await RunCase("session", () => SessionCase(conn));
        ok &= await RunCase("trace", () => TraceCase(conn));
        return ok;
    }

    private const string TraceId = "4bf92f3577b34da6a3ce929d0e0e4736";

    /// <summary>traceEcho under a known, sampled traceparent: the client call takes it from the current
    /// Activity, and the server must report the same trace id.</summary>
    private static async Task<string> TraceCase(RpcConnection conn)
    {
        string traceparent = $"00-{TraceId}-{Convert.ToHexString(System.Security.Cryptography.RandomNumberGenerator.GetBytes(8)).ToLowerInvariant()}-01";
        EchoReply r;
        using (Aster.Observability.Tracing.WithTraceContext(traceparent))
        {
            r = await conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "traceEcho", Probe()).WaitAsync(TimeSpan.FromSeconds(15));
        }
        if (r.Message != TraceId) throw new InvalidOperationException($"server saw trace id '{r.Message}', want {TraceId}");
        return r.ServerLang;
    }

    private static EchoRequest Probe(string message = "probe") => new() { Message = message };

    private static async Task<int> CancelledCount(RpcConnection conn)
        => (await conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "cancelled", Probe()).WaitAsync(TimeSpan.FromSeconds(10))).Count;

    private static async Task ExpectServerCancel(RpcConnection conn, int want)
    {
        int seen = 0;
        for (int i = 0; i < 50; i++)
        {
            seen = await CancelledCount(conn);
            if (seen >= want) return;
            await Task.Delay(100);
        }
        throw new InvalidOperationException($"server never observed the cancellation (count {seen}, want {want})");
    }

    /// <summary>chat, interleaved: send c0, read its reply, ... then end the request stream and expect the response stream to end.</summary>
    private static async Task<string> ChatCase(RpcConnection conn)
    {
        var requests = System.Threading.Channels.Channel.CreateUnbounded<EchoRequest>();
        await using var e = conn.BidiStreamAsync<EchoRequest, EchoReply>("InteropEcho", "chat", requests.Reader.ReadAllAsync())
            .GetAsyncEnumerator();
        string lang = "";
        for (int i = 0; i < 3; i++)
        {
            var req = new EchoRequest { Message = $"c{i}", Count = 100 + i, Data = [(byte)i, 9] };
            await requests.Writer.WriteAsync(req);
            if (!await e.MoveNextAsync()) throw new InvalidOperationException($"stream ended before reply {i}");
            var r = e.Current;
            if (r.Count != i || r.Message != req.Message || !r.Data.AsSpan().SequenceEqual(req.Data))
                throw new InvalidOperationException($"reply {i} mismatch: {r.Count} {r.Message}");
            lang = r.ServerLang;
        }
        requests.Writer.Complete();
        if (await e.MoveNextAsync()) throw new InvalidOperationException("unexpected extra reply");
        if (lang.Length == 0) throw new InvalidOperationException("empty server_lang");
        return lang;
    }

    /// <summary>hold with a 1 s client deadline: DEADLINE_EXCEEDED, and the server observes a cancellation.</summary>
    private static async Task<string> DeadlineCase(RpcConnection conn)
    {
        int before = await CancelledCount(conn);
        try
        {
            var r = await conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "hold", Probe("hold"),
                new CallOptions { Deadline = TimeSpan.FromSeconds(1) }).WaitAsync(TimeSpan.FromSeconds(15));
            throw new InvalidOperationException($"hold returned {r.Message} instead of an error");
        }
        catch (RpcError e) when (e.Code == StatusCode.DeadlineExceeded)
        {
        }
        await ExpectServerCancel(conn, before + 1);
        return "dotnet";
    }

    /// <summary>hold cancelled by the client (token) after 300 ms: OperationCanceledException locally, and the server observes it.</summary>
    private static async Task<string> CancelCase(RpcConnection conn)
    {
        int before = await CancelledCount(conn);
        // short-wait-ok: hold never answers; this client cancel is the case under test, and the server leg must observe it
        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(300));
        try
        {
            var r = await conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "hold", Probe("hold"), null, cts.Token);
            throw new InvalidOperationException($"hold returned {r.Message} before the client cancelled it");
        }
        catch (OperationCanceledException)
        {
        }
        await ExpectServerCancel(conn, before + 1);
        return "dotnet";
    }

    /// <summary>Two sessions on one connection: state persists per session and is isolated between them.</summary>
    private static async Task<string> SessionCase(RpcConnection conn)
    {
        var a = new CallOptions { SessionId = conn.NextSessionId() };
        var b = new CallOptions { SessionId = conn.NextSessionId() };
        (CallOptions S, int Add, int Total, string Calls)[] steps =
            [(a, 5, 5, "calls=1"), (a, 7, 12, "calls=2"), (b, 1, 1, "calls=1"), (a, 0, 12, "calls=3"), (b, 10, 11, "calls=2")];
        string lang = "";
        for (int i = 0; i < steps.Length; i++)
        {
            var (s, add, total, calls) = steps[i];
            var r = await conn.UnaryAsync<EchoRequest, EchoReply>("InteropSession", "add",
                new EchoRequest { Message = "add", Count = add }, s).WaitAsync(TimeSpan.FromSeconds(15));
            if (r.Count != total || r.Message != calls)
                throw new InvalidOperationException($"step {i}: got total {r.Count} {r.Message}, want total {total} {calls}");
            lang = r.ServerLang;
        }
        if (lang.Length == 0) throw new InvalidOperationException("empty server_lang");
        return lang;
    }

    private sealed class AllowAll : IExposeAuthorizer
    {
        public ValueTask<bool> AuthorizeAsync(ExposeAuthorizeRequest request, CancellationToken ct) => new(true);
    }

    /// <summary>Expose mini-matrix server: GET /hello, POST /echo (request body streamed back), x-server-lang.</summary>
    public static async Task ExposeServeAsync()
    {
        const string alpn = "aster-expose/interop/1";
        await using var node = await Node.StartAsync(Config() with { Alpns = [alpn] });
        await using var up = Upcalls.Create(node.Runtime);
        using var bodies = HttpBodies.Create(node.Runtime);
        using var registry = ExposeRegistry.Create(node);
        up.ServeExposeAuthorizer(new AllowAll());
        registry.ExposeHttp("interop-http", up, bodies, req =>
        {
            HttpHeader[] headers = [new HttpHeader("x-server-lang", "dotnet")];
            if (req.Method == "GET" && req.Uri == "/hello")
                return ValueTask.FromResult(ExposeHttpResponse.Inline(200, System.Text.Encoding.UTF8.GetBytes("hello from dotnet"), headers));
            if (req.Method == "POST" && req.Uri == "/echo")
                return ValueTask.FromResult(ExposeHttpResponse.Streaming(200, EchoBody(req.Body, req.Cancellation), headers));
            return ValueTask.FromResult(ExposeHttpResponse.Inline(404, [], headers));
        });
        var serving = registry.ServeAsync(node, [alpn]);
        var addr = node.NodeAddr();
        for (int i = 0; i < 100 && addr.DirectAddresses.Count == 0; i++)
        {
            await Task.Delay(50);
            addr = node.NodeAddr();
        }
        Console.WriteLine($"ADDRESS {AsterTicket.From(addr)}");
        Console.Out.Flush();
        while (await Console.In.ReadLineAsync() != null)
        {
            // serve until stdin closes
        }
        registry.Unexpose("interop-http");
        _ = serving;
    }

    private static async IAsyncEnumerable<ReadOnlyMemory<byte>> EchoBody(Stream body, [EnumeratorCancellation] CancellationToken ct)
    {
        var buf = new byte[16 * 1024];
        while (true)
        {
            int n = await body.ReadAsync(buf, ct);
            if (n == 0) yield break;
            yield return buf.AsMemory(0, n).ToArray();
        }
    }

    private static async IAsyncEnumerable<EchoRequest> Requests([EnumeratorCancellation] CancellationToken ct = default)
    {
        for (int i = 0; i < 3; i++)
        {
            ct.ThrowIfCancellationRequested();
            yield return new EchoRequest { Message = $"m{i}", Count = i, Data = [(byte)i] };
            await Task.Yield();
        }
    }
}
