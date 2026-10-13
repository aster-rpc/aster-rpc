using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;
using Apache.Fory;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster.Tests;

[ForyStruct]
[WireType("interop/EchoRequest")]
public sealed class EchoRequest
{
    public string Message { get; set; } = "";
    public int Count { get; set; }
    public byte[] Data { get; set; } = [];
}

[ForyStruct]
[WireType("interop/EchoReply")]
public sealed class EchoReply
{
    public string Message { get; set; } = "";
    public int Count { get; set; }
    public byte[] Data { get; set; } = [];
    public string ServerLang { get; set; } = "";
}

[ForyStruct]
[WireType("test/Numbers")]
public sealed class Numbers
{
    public int I { get; set; }
    public long L { get; set; }
    public uint U { get; set; }
    public ulong UL { get; set; }
    public short S { get; set; }
    public sbyte SB { get; set; }
    public byte B { get; set; }
    public float F { get; set; }
    public double D { get; set; }
    public bool Flag { get; set; }
    public int? MaybeInt { get; set; }
    public string? MaybeText { get; set; }
    public List<string> Names { get; set; } = [];
    public Dictionary<string, long> Totals { get; set; } = [];
    public EchoRequest Nested { get; set; } = new();
}

/// <summary>Server + client end to end over loopback (unary, streaming, errors, deadlines).</summary>
public class RpcTests : IAsyncLifetime
{
    /// <summary>InteropEcho contract id shared by Rust, Python, TS, Java (tests/interop).</summary>
    public const string InteropEchoContractId = "c5aa690b04f415990755ae87a2a608b14f41cf07306117028942b461af081c77";

    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    private AsterServer _server = null!;
    private AsterClient _client = null!;
    private RpcConnection _conn = null!;
    private RpcCodecDiagnostics? _codecDiagnostics;
    private readonly TaskCompletionSource<CancellationToken> _slowCancelled = new();
    private readonly TaskCompletionSource<bool> _hangStarted = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly TaskCompletionSource<bool> _hangCancelled = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private int _parkEntered, _parkCancelled;
    private int _adminEntered;

    public static ServiceDefinition InteropEcho(string lang) => ServiceDefinition.Create("InteropEcho", 1)
        .Unary<EchoRequest, EchoReply>("echo", (req, _) => ValueTask.FromResult(new EchoReply
        {
            Message = req.Message, Count = unchecked(req.Count + 1), Data = req.Data, ServerLang = lang,
        }))
        .ServerStream<EchoRequest, EchoReply>("generate", Generate)
        .ClientStream<EchoRequest, EchoReply>("ingest", async (reqs, ctx) =>
        {
            int n = 0;
            string last = "";
            await foreach (var r in reqs) { n++; last = r.Message; }
            return new EchoReply { Message = last, Count = n, ServerLang = lang };
        })
        .Build();

    private static async IAsyncEnumerable<EchoReply> Generate(EchoRequest req, ServerCallContext ctx)
    {
        for (int i = 0; i < Math.Max(req.Count, 0); i++)
        {
            await Task.Yield();
            yield return new EchoReply { Message = req.Message, Count = i, Data = req.Data, ServerLang = "dotnet" };
        }
    }

    private ServiceDefinition Extra() => ServiceDefinition.Create("Extra", 1)
        .Unary<EchoRequest, EchoReply>("fail", (req, _) => throw new InvalidOperationException($"boom: {req.Message}"))
        .Unary<EchoRequest, EchoReply>("denied", (_, _) => throw new RpcError(StatusCode.PermissionDenied, "nope",
            new Dictionary<string, string> { ["why"] = "test" }))
        .Unary<EchoRequest, EchoReply>("meta", (_, ctx) => ValueTask.FromResult(new EchoReply
        {
            Message = string.Join(",", ctx.Metadata.Select(kv => $"{kv.Key}={kv.Value}")),
            Count = ctx.Deadline is null ? 0 : 1,
            ServerLang = ctx.PeerId,
        }))
        .Unary<EchoRequest, EchoReply>("slow", async (_, ctx) =>
        {
            _slowCancelled.TrySetResult(ctx.CancellationToken);
            await Task.Delay(TimeSpan.FromSeconds(30), ctx.CancellationToken);
            return new EchoReply();
        })
        .Unary<EchoRequest, EchoReply>("park", async (_, ctx) =>
        {
            Interlocked.Increment(ref _parkEntered);
            try { await Task.Delay(TimeSpan.FromSeconds(30), ctx.CancellationToken); }
            catch (OperationCanceledException) { Interlocked.Increment(ref _parkCancelled); throw; }
            return new EchoReply();
        })
        .Unary<EchoRequest, EchoReply>("guarded", (req, _) => ValueTask.FromResult(new EchoReply { Message = req.Message }))
        .ServerStream<EchoRequest, EchoReply>("hang", Hang)
        .Unary<EchoRequest, EchoReply>("slowDefault", async (_, ctx) =>
        {
            await Task.Delay(TimeSpan.FromSeconds(30), ctx.CancellationToken);
            return new EchoReply();
        }, defaultTimeout: TimeSpan.FromMilliseconds(500))
        .Unary<EchoRequest, EchoReply>("admin", (_, ctx) =>
        {
            Interlocked.Increment(ref _adminEntered);
            return ValueTask.FromResult(new EchoReply
            {
                Message = ctx.Attributes.TryGetValue("aster.role", out var role) ? role : "",
            });
        }, requires: CapabilityRequirement.AnyOf("admin", "ops"))
        .BidiStream<EchoRequest, EchoReply>("chat", Chat)
        .ServerStream<EchoRequest, EchoReply>("failMidStream", FailMidStream)
        .Build();

    private static async IAsyncEnumerable<EchoReply> Chat(IAsyncEnumerable<EchoRequest> reqs, ServerCallContext ctx)
    {
        await foreach (var r in reqs)
            yield return new EchoReply { Message = r.Message.ToUpperInvariant(), Count = r.Count };
    }

    private async IAsyncEnumerable<EchoReply> Hang(EchoRequest req, ServerCallContext ctx)
    {
        yield return new EchoReply { Count = 1 };
        _hangStarted.TrySetResult(true);
        try { await Task.Delay(TimeSpan.FromSeconds(30), ctx.CancellationToken); }
        catch (OperationCanceledException) { _hangCancelled.TrySetResult(true); throw; }
        yield return new EchoReply { Count = 2 };
    }

    private static async IAsyncEnumerable<EchoReply> FailMidStream(EchoRequest req, ServerCallContext ctx)
    {
        yield return new EchoReply { Count = 0 };
        await Task.Yield();
        throw new RpcError(StatusCode.DataLoss, "mid-stream failure");
    }

    public async Task InitializeAsync()
    {
        var echo = InteropEcho("dotnet");
        var extra = Extra();
        _codecDiagnostics = RpcCodecDiagnostics.Create();
        _server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = Loopback,
            Services = { echo, extra },
            Codec = _codecDiagnostics?.For("server", echo, extra),
            Interceptors = { new RequireTokenInterceptor() },
        });
        _client = await AsterClient.StartAsync(Loopback, _codecDiagnostics?.For("client", echo));
        _client.RegisterTypes(echo);
        _conn = await _client.ConnectAsync(await _server.TicketAsync());
    }

    public async Task DisposeAsync()
    {
        await _client.DisposeAsync();
        await _server.DisposeAsync();
        _codecDiagnostics?.Dispose();
    }

    [Fact]
    public void InteropEcho_ContractId_Matches_Other_Languages()
        => Assert.Equal(InteropEchoContractId, InteropEcho("dotnet").ContractId);

    [Fact]
    public void Contract_Maps_Integers_To_Varints_And_Containers()
    {
        string json = ContractBuilder.TypeDefJson(typeof(Numbers));
        foreach (var expected in new[]
        {
            "\"name\":\"i\",\"type_kind\":\"primitive\",\"type_primitive\":\"varint32\"",
            "\"name\":\"l\",\"type_kind\":\"primitive\",\"type_primitive\":\"varint64\"",
            "\"name\":\"u\",\"type_kind\":\"primitive\",\"type_primitive\":\"var_uint32\"",
            "\"name\":\"ul\",\"type_kind\":\"primitive\",\"type_primitive\":\"var_uint64\"",
            "\"name\":\"s\",\"type_kind\":\"primitive\",\"type_primitive\":\"int16\"",
            "\"name\":\"sb\",\"type_kind\":\"primitive\",\"type_primitive\":\"int8\"",
            "\"name\":\"b\",\"type_kind\":\"primitive\",\"type_primitive\":\"uint8\"",
            "\"name\":\"f\",\"type_kind\":\"primitive\",\"type_primitive\":\"float32\"",
            "\"name\":\"d\",\"type_kind\":\"primitive\",\"type_primitive\":\"float64\"",
        })
            Assert.Contains(expected, json);
        Assert.Contains("\"name\":\"maybe_int\",\"type_kind\":\"primitive\",\"type_primitive\":\"varint32\",\"type_ref\":\"\",\"self_ref_name\":\"\",\"optional\":true", json);
        Assert.Contains("\"name\":\"maybe_text\",\"type_kind\":\"primitive\",\"type_primitive\":\"string\",\"type_ref\":\"\",\"self_ref_name\":\"\",\"optional\":true", json);
        Assert.Contains("\"name\":\"names\",\"type_kind\":\"primitive\",\"type_primitive\":\"string\",\"type_ref\":\"\",\"self_ref_name\":\"\",\"optional\":false,\"ref_tracked\":false,\"container\":\"list\"", json);
        Assert.Contains("\"container\":\"map\",\"container_key_kind\":\"primitive\",\"container_key_primitive\":\"string\"", json);
        Assert.Contains($"\"name\":\"nested\",\"type_kind\":\"ref\",\"type_primitive\":\"\",\"type_ref\":\"{ContractBuilder.TypeHash(typeof(EchoRequest))}\"", json);
        Assert.Equal(64, ContractBuilder.TypeHash(typeof(Numbers)).Length);
    }

    [Theory]
    [InlineData("small", 41, 4)]
    [InlineData("zstd", -123_456, 20_000)]
    [InlineData("long", int.MaxValue, 0)]
    public async Task Unary_Echo(string message, int count, int dataLen)
    {
        var req = new EchoRequest
        {
            Message = message == "long" ? new string('x', 5000) : message,
            Count = count,
            Data = Enumerable.Range(0, dataLen).Select(i => (byte)(i % 251)).ToArray(),
        };
        var reply = await _conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "echo", req);
        Assert.Equal(req.Message, reply.Message);
        Assert.Equal(unchecked(count + 1), reply.Count);
        Assert.Equal(req.Data, reply.Data);
        Assert.Equal("dotnet", reply.ServerLang);
    }

    [Fact]
    public async Task Concurrent_Unary_Calls()
    {
        var tasks = Enumerable.Range(0, 64).Select(i =>
            _conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "echo", new EchoRequest { Message = $"m{i}", Count = i }));
        var replies = await Task.WhenAll(tasks);
        for (int i = 0; i < replies.Length; i++)
        {
            var reply = replies[i];
            Assert.True(reply.Count == i + 1 && reply.Message == $"m{i}" && reply.ServerLang == "dotnet",
                $"call {i}: count={reply.Count}, message='{reply.Message}', server='{reply.ServerLang}'");
        }
        Assert.Equal(Enumerable.Range(1, 64), replies.Select(r => r.Count).OrderBy(c => c));
    }

    [Fact]
    public async Task Server_Stream()
    {
        var got = new List<EchoReply>();
        await foreach (var r in _conn.ServerStreamAsync<EchoRequest, EchoReply>("InteropEcho", "generate",
                           new EchoRequest { Message = "s", Count = 50, Data = [9] }))
            got.Add(r);
        Assert.Equal(Enumerable.Range(0, 50), got.Select(r => r.Count));
        Assert.All(got, r => Assert.Equal("s", r.Message));
    }

    [Fact]
    public async Task Server_Stream_Backpressure_Uses_Capacity_Reservation()
    {
        long before = DispatchServer.QueueFullHits;
        // 2000 x 8 KiB incompressible frames (16 MiB) overflow the QUIC windows and the
        // per-call response channel while the reader sleeps, so the server must wait for
        // RESPONSE_WRITABLE (the QUEUE_FULL path) instead of dropping frames. Whether the
        // channel actually fills depends on scheduling, so retry a few times.
        var data = new byte[8192];
        new Random(42).NextBytes(data);
        for (int attempt = 0; attempt < 4 && DispatchServer.QueueFullHits == before; attempt++)
        {
            int n = 0;
            await foreach (var r in _conn.ServerStreamAsync<EchoRequest, EchoReply>("InteropEcho", "generate",
                               new EchoRequest { Message = "bp", Count = 2000, Data = data }))
            {
                Assert.Equal(n, r.Count);
                Assert.Equal(data, r.Data);
                if (n++ == 0) await Task.Delay(1500);
            }
            Assert.Equal(2000, n);
        }
        Assert.True(DispatchServer.QueueFullHits > before, "expected at least one QUEUE_FULL");
    }

    [Fact]
    public async Task Server_Stream_Zero_Items()
    {
        var got = await _conn.ServerStreamAsync<EchoRequest, EchoReply>("InteropEcho", "generate", new EchoRequest { Count = 0 }).ToListAsync();
        Assert.Empty(got);
    }

    [Fact]
    public async Task Client_Stream()
    {
        var reqs = Enumerable.Range(0, 10).Select(i => new EchoRequest { Message = $"m{i}", Count = i });
        var reply = await _conn.ClientStreamAsync<EchoRequest, EchoReply>("InteropEcho", "ingest", reqs);
        Assert.Equal(10, reply.Count);
        Assert.Equal("m9", reply.Message);
    }

    [Fact]
    public async Task Client_Stream_Slow_Producer_Makes_The_Server_Wait()
    {
        // Each request arrives after the server drained the queue: the blocking recv path.
        async IAsyncEnumerable<EchoRequest> Reqs()
        {
            for (int i = 0; i < 5; i++)
            {
                await Task.Delay(50);
                yield return new EchoRequest { Message = $"m{i}", Count = i };
            }
        }
        var reply = await _conn.ClientStreamAsync<EchoRequest, EchoReply>("InteropEcho", "ingest", Reqs());
        Assert.Equal(5, reply.Count);
        Assert.Equal("m4", reply.Message);
    }

    [Fact]
    public async Task Client_Stream_Many_Messages()
    {
        var reqs = Enumerable.Range(0, 10_000).Select(i => new EchoRequest { Message = $"m{i}", Count = i, Data = [1, 2, 3] });
        var reply = await _conn.ClientStreamAsync<EchoRequest, EchoReply>("InteropEcho", "ingest", reqs);
        Assert.Equal(10_000, reply.Count);
        Assert.Equal("m9999", reply.Message);
    }

    [Fact]
    public async Task Client_Stream_Single_Large_Message()
    {
        var reqs = new[] { new EchoRequest { Message = new string('z', 10_000), Data = new byte[50_000] } };
        var reply = await _conn.ClientStreamAsync<EchoRequest, EchoReply>("InteropEcho", "ingest", reqs);
        Assert.Equal(1, reply.Count);
        Assert.Equal(10_000, reply.Message.Length);
    }

    [Fact]
    public async Task Bidi_Stream()
    {
        async IAsyncEnumerable<EchoRequest> Reqs()
        {
            for (int i = 0; i < 5; i++)
            {
                await Task.Yield();
                yield return new EchoRequest { Message = $"hi{i}", Count = i };
            }
        }
        var got = await _conn.BidiStreamAsync<EchoRequest, EchoReply>("Extra", "chat", Reqs()).ToListAsync();
        Assert.Equal(new[] { "HI0", "HI1", "HI2", "HI3", "HI4" }, got.Select(r => r.Message));
    }

    private sealed class RequireTokenInterceptor : IInterceptor
    {
        public object OnRequest(CallContext ctx, object request)
        {
            if (ctx.Method == "guarded" && !ctx.Metadata.ContainsKey("token"))
                throw new RpcError(StatusCode.Unauthenticated, "token required");
            return request;
        }
    }

    [Fact]
    public async Task Server_Interceptor_Can_Reject()
    {
        var ex = await Assert.ThrowsAsync<RpcError>(() => _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "guarded", new EchoRequest()));
        Assert.Equal(StatusCode.Unauthenticated, ex.Code);
        var ok = await _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "guarded", new EchoRequest { Message = "m" },
            new CallOptions { Metadata = [new("token", "t")] });
        Assert.Equal("m", ok.Message);
    }

    [Fact]
    public async Task Handler_Exception_Becomes_Internal_Trailer()
    {
        var ex = await Assert.ThrowsAsync<RpcError>(() =>
            _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "fail", new EchoRequest { Message = "x" }));
        Assert.Equal(StatusCode.Internal, ex.Code);
        Assert.Equal("boom: x", ex.Message);
    }

    [Fact]
    public async Task RpcError_Code_Message_And_Details_Propagate()
    {
        var ex = await Assert.ThrowsAsync<RpcError>(() =>
            _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "denied", new EchoRequest()));
        Assert.Equal(StatusCode.PermissionDenied, ex.Code);
        Assert.Equal("nope", ex.Message);
        Assert.Equal("test", ex.Details["why"]);
    }

    [Fact]
    public async Task Server_Stream_Error_After_First_Item()
    {
        var got = new List<EchoReply>();
        var ex = await Assert.ThrowsAsync<RpcError>(async () =>
        {
            await foreach (var r in _conn.ServerStreamAsync<EchoRequest, EchoReply>("Extra", "failMidStream", new EchoRequest()))
                got.Add(r);
        });
        Assert.Single(got);
        Assert.Equal(StatusCode.DataLoss, ex.Code);
    }

    [Theory]
    [InlineData("Nope", "echo", StatusCode.NotFound)] // Rust's server: unknown (service, version)
    [InlineData("InteropEcho", "nope", StatusCode.Unimplemented)]
    public async Task Unknown_Service_Or_Method_Is_Rejected_By_Rust(string service, string method, StatusCode expected)
    {
        var ex = await Assert.ThrowsAsync<RpcError>(() => _conn.UnaryAsync<EchoRequest, EchoReply>(service, method, new EchoRequest()));
        Assert.Equal(expected, ex.Code);
    }

    [Fact]
    public async Task Session_Id_On_Shared_Service_Is_Rejected()
    {
        var ex = await Assert.ThrowsAsync<RpcError>(() => _conn.UnaryAsync<EchoRequest, EchoReply>(
            "InteropEcho", "echo", new EchoRequest(), new CallOptions { SessionId = 3 }));
        Assert.Equal(StatusCode.FailedPrecondition, ex.Code);
    }

    [Fact]
    public async Task Metadata_Deadline_And_Peer_Reach_Handler()
    {
        var reply = await _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "meta", new EchoRequest(), new CallOptions
        {
            Deadline = TimeSpan.FromMilliseconds(1500),
            Metadata = [new("trace", "abc"), new("k", "v")],
        });
        Assert.Equal("trace=abc,k=v", reply.Message);
        Assert.Equal(1, reply.Count);
        Assert.Equal(_client.Node.NodeId(), reply.ServerLang);
    }

    [Fact]
    public async Task Deadline_Exceeded_Cancels_Handler()
    {
        var ex = await Assert.ThrowsAsync<RpcError>(() => _conn.UnaryAsync<EchoRequest, EchoReply>(
            "Extra", "slow", new EchoRequest(), new CallOptions { Deadline = TimeSpan.FromSeconds(1) }));
        Assert.Equal(StatusCode.DeadlineExceeded, ex.Code);
        Assert.True((await _slowCancelled.Task).IsCancellationRequested);
    }

    [Fact]
    public async Task Client_Cancellation_Stops_Waiting()
    {
        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(300));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() =>
            _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "slow", new EchoRequest(), cancellationToken: cts.Token));
    }

    [Fact]
    public void CallOptions_Deadline_Is_Whole_Milliseconds_Rounded_Up_And_Capped()
    {
        Assert.Equal(1001u, new CallOptions { Deadline = TimeSpan.FromMilliseconds(1000.2) }.DeadlineMs);
        Assert.Equal(1u, new CallOptions { Deadline = TimeSpan.FromTicks(1) }.DeadlineMs);
        Assert.Equal(0u, new CallOptions().DeadlineMs);
        Assert.Equal(0u, new CallOptions { Deadline = TimeSpan.FromSeconds(-1) }.DeadlineMs);
        Assert.Equal(CallOptions.MaxDeadlineMs, new CallOptions { Deadline = TimeSpan.FromDays(10) }.DeadlineMs);
    }

    [Fact]
    public void Rust_Derives_The_Same_Contract_Id_From_The_Registration_Json()
    {
        foreach (var svc in new[] { InteropEcho("dotnet"), Extra(), SessionTests.Counter(new()) })
            Assert.Equal(svc.ContractId, DispatchServer.ContractId(svc.RegistrationJson));
        // Requirements and default timeouts are part of the contract.
        Assert.NotEqual(Extra().ContractId, ServiceDefinition.Create("Extra", 1)
            .Unary<EchoRequest, EchoReply>("admin", (_, _) => ValueTask.FromResult(new EchoReply())).Build().ContractId);
        Assert.Contains("\"requires\":{\"kind\":\"any_of\",\"roles\":[\"admin\",\"ops\"]}", Extra().RegistrationJson);
        Assert.Contains("\"default_timeout\":0.5", Extra().RegistrationJson);
    }

    [Fact]
    public async Task Client_Cancellation_Cancels_The_Server_Handler()
    {
        // aster_call_cancel sends FLAG_CANCEL; Rust turns it into a CANCELLED event for streaming
        // calls (unary calls on the server are not watched for cancels by Rust).
        using var cts = new CancellationTokenSource();
        var call = Task.Run(async () =>
        {
            await foreach (var _ in _conn.ServerStreamAsync<EchoRequest, EchoReply>("Extra", "hang", new EchoRequest(), cancellationToken: cts.Token))
            {
            }
        });
        await _hangStarted.Task.WaitAsync(TimeSpan.FromSeconds(10));
        cts.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => call);
        Assert.True(await _hangCancelled.Task.WaitAsync(TimeSpan.FromSeconds(10)));
    }

    [Fact]
    public async Task Unary_Cancel_Reaches_The_Handler_On_The_Pooled_Path()
    {
        // ABI 4.5: a cancellable unary call stays on the pooled fast path with a native cancel
        // token. Ten calls: eight fill the SHARED pool's streams, two wait for one; cancelling
        // them all reaches every started handler, and the pool is whole again afterwards.
        var ctss = Enumerable.Range(0, 10).Select(_ => new CancellationTokenSource()).ToList();
        var calls = ctss.Select(c => _conn.UnaryAsync<EchoRequest, EchoReply>(
            "Extra", "park", new EchoRequest(), cancellationToken: c.Token)).ToList();
        var until = DateTime.UtcNow + TimeSpan.FromSeconds(20);
        while (Volatile.Read(ref _parkEntered) < 8 && DateTime.UtcNow < until) await Task.Delay(5);
        Assert.True(Volatile.Read(ref _parkEntered) >= 8, "the pool's eight streams carry parked calls");
        foreach (var c in ctss) c.Cancel();
        foreach (var call in calls)
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => call.WaitAsync(TimeSpan.FromSeconds(10)));
        until = DateTime.UtcNow + TimeSpan.FromSeconds(10);
        while (Volatile.Read(ref _parkCancelled) < Volatile.Read(ref _parkEntered) && DateTime.UtcNow < until)
            await Task.Delay(5);
        Assert.Equal(Volatile.Read(ref _parkEntered), Volatile.Read(ref _parkCancelled));
        foreach (var c in ctss) c.Dispose();

        using var live = new CancellationTokenSource();
        var after = Enumerable.Range(0, 16).Select(i => _conn.UnaryAsync<EchoRequest, EchoReply>(
            "InteropEcho", "echo", new EchoRequest { Count = i }, cancellationToken: live.Token)).ToList();
        var replies = await Task.WhenAll(after).WaitAsync(TimeSpan.FromSeconds(20));
        for (int i = 0; i < 16; i++) Assert.Equal(i + 1, replies[i].Count);
    }

    [Fact]
    public async Task Method_Default_Timeout_Is_Enforced_By_Rust()
    {
        var ex = await Assert.ThrowsAsync<RpcError>(() =>
            _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "slowDefault", new EchoRequest()));
        Assert.Equal(StatusCode.DeadlineExceeded, ex.Code);
    }

    [Fact]
    public async Task Gate3_Requires_Uses_The_Peer_Attribute_Bridge()
    {
        var ex = await Assert.ThrowsAsync<RpcError>(() => _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "admin", new EchoRequest()));
        Assert.Equal(StatusCode.PermissionDenied, ex.Code);
        Assert.Equal(0, Volatile.Read(ref _adminEntered));

        string peer = _client.Node.NodeId();
        _server.SetPeerAttributes(peer, new Dictionary<string, string> { ["aster.role"] = "ops" });
        var reply = await _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "admin", new EchoRequest());
        Assert.Equal("ops", reply.Message);
        Assert.Equal(1, Volatile.Read(ref _adminEntered));

        _server.SetPeerAttributes(peer, null);
        ex = await Assert.ThrowsAsync<RpcError>(() => _conn.UnaryAsync<EchoRequest, EchoReply>("Extra", "admin", new EchoRequest()));
        Assert.Equal(StatusCode.PermissionDenied, ex.Code);
        Assert.Equal(1, Volatile.Read(ref _adminEntered));
    }

    [Fact]
    public async Task Server_Stream_Cancel_Stops_Reading()
    {
        using var cts = new CancellationTokenSource();
        int n = 0;
        await Assert.ThrowsAnyAsync<OperationCanceledException>(async () =>
        {
            await foreach (var _ in _conn.ServerStreamAsync<EchoRequest, EchoReply>("InteropEcho", "generate",
                               new EchoRequest { Count = 1_000_000 }, cancellationToken: cts.Token))
                if (++n == 10) cts.Cancel();
        });
        Assert.InRange(n, 10, 1_000_000 - 1);
        // The connection is still usable afterwards.
        var reply = await _conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "echo", new EchoRequest { Count = 1 });
        Assert.Equal(2, reply.Count);
    }
}

internal static class AsyncEnumerableTestExtensions
{
    public static async Task<List<T>> ToListAsync<T>(this IAsyncEnumerable<T> source, CancellationToken ct = default)
    {
        var list = new List<T>();
        await foreach (var item in source.WithCancellation(ct)) list.Add(item);
        return list;
    }
}
