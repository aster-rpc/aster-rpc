using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster.Tests;

/// <summary>Breaker state machine without a network.</summary>
public class CircuitBreakerUnitTests
{
    private static readonly RpcError Unavailable = new(StatusCode.Unavailable, "x");

    [Fact]
    public void Opens_After_Threshold_Then_Half_Opens_And_Closes_On_Success()
    {
        var b = new CircuitBreakerInterceptor(failureThreshold: 2, recoveryTimeout: 0.15);
        b.BeforeCall(); b.RecordFailure(Unavailable);
        b.BeforeCall(); b.RecordFailure(Unavailable);
        Assert.Equal(CircuitBreakerInterceptor.StateOpen, b.State);
        var e = Assert.Throws<RpcError>(b.BeforeCall);
        Assert.Equal(StatusCode.Unavailable, e.Code);
        Thread.Sleep(250);
        b.BeforeCall();                                      // the probe
        Assert.Equal(CircuitBreakerInterceptor.StateHalfOpen, b.State);
        Assert.Throws<RpcError>(b.BeforeCall);               // probe budget used
        b.RecordSuccess();
        Assert.Equal(CircuitBreakerInterceptor.StateClosed, b.State);
        Assert.Equal(0, b.FailureCount);
    }

    [Fact]
    public void Failed_Probe_Reopens()
    {
        var b = new CircuitBreakerInterceptor(failureThreshold: 1, recoveryTimeout: 0.1);
        b.RecordFailure(Unavailable);
        Thread.Sleep(200);
        b.BeforeCall();
        b.RecordFailure(Unavailable);
        Assert.Equal(CircuitBreakerInterceptor.StateOpen, b.State);
    }

    [Fact]
    public void Only_Unavailable_Internal_Unknown_Trip()
    {
        var b = new CircuitBreakerInterceptor(failureThreshold: 1);
        foreach (var code in new[] { StatusCode.InvalidArgument, StatusCode.PermissionDenied, StatusCode.NotFound, StatusCode.DeadlineExceeded })
            b.RecordFailure(new RpcError(code, "x"));
        Assert.Equal(CircuitBreakerInterceptor.StateClosed, b.State);
        foreach (var code in new[] { StatusCode.Internal, StatusCode.Unknown })
        {
            var t = new CircuitBreakerInterceptor(failureThreshold: 1);
            t.RecordFailure(new RpcError(code, "x"));
            Assert.Equal(CircuitBreakerInterceptor.StateOpen, t.State);
        }
    }

    [Fact]
    public void A_NonTripping_Failure_Frees_The_Half_Open_Probe_Slot()
    {
        var b = new CircuitBreakerInterceptor(failureThreshold: 1, recoveryTimeout: 0.05);
        b.RecordFailure(Unavailable);
        Thread.Sleep(120);
        b.BeforeCall();                                      // probe 1 (half-open)
        b.RecordFailure(new RpcError(StatusCode.InvalidArgument, "x"));
        b.BeforeCall();                                      // slot was freed: not stuck half-open
        b.RecordSuccess();
        Assert.Equal(CircuitBreakerInterceptor.StateClosed, b.State);
    }

    [Fact]
    public void Concurrent_Use_Keeps_A_Valid_State()
    {
        var b = new CircuitBreakerInterceptor(failureThreshold: 5, recoveryTimeout: 0.001, halfOpenMaxCalls: 3);
        var states = new HashSet<string> { CircuitBreakerInterceptor.StateClosed, CircuitBreakerInterceptor.StateOpen, CircuitBreakerInterceptor.StateHalfOpen };
        Parallel.For(0, 8, t =>
        {
            var rng = new Random(t);
            for (int i = 0; i < 20_000; i++)
            {
                try { b.BeforeCall(); }
                catch (RpcError) { continue; }
                if (rng.Next(3) == 0) b.RecordSuccess(); else b.RecordFailure(Unavailable);
                Assert.Contains(b.State, states);
                Assert.True(b.FailureCount >= 0);
            }
        });
        Assert.Contains(b.State, states);
    }
}

/// <summary>Client interceptors against a real server over loopback, all four call patterns.</summary>
public class ClientInterceptorTests : IAsyncLifetime
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    private AsterServer _server = null!;
    private AsterClient _client = null!;
    private RpcConnection _conn = null!;

    // Server-side behaviour, set per test.
    private int _flakyCalls;
    private int _flakyFailUntil;                              // calls numbered <= this fail
    private StatusCode _flakyCode = StatusCode.Unavailable;
    private int _ingestCalls;
    private int _ingestFailUntil;
    private int _slowCalls;
    private readonly ConcurrentQueue<double> _slowRemaining = new();
    private readonly ConcurrentQueue<double> _flakyRemaining = new();

    private ServiceDefinition Service() => ServiceDefinition.Create("Flaky", 1)
        .Unary<EchoRequest, EchoReply>("flaky", Flaky, idempotent: true)
        .Unary<EchoRequest, EchoReply>("flakyNonIdem", Flaky)
        .Unary<EchoRequest, EchoReply>("denied", (_, _) => throw new RpcError(StatusCode.PermissionDenied, "nope"), idempotent: true)
        .Unary<EchoRequest, EchoReply>("slow", async (_, ctx) =>
        {
            Interlocked.Increment(ref _slowCalls);
            if (ctx.Deadline is { } d) _slowRemaining.Enqueue((d - DateTimeOffset.UtcNow).TotalSeconds);
            await Task.Delay(TimeSpan.FromSeconds(30), ctx.CancellationToken);
            return new EchoReply();
        }, idempotent: true)
        .Unary<EchoRequest, EchoReply>("meta", (req, ctx) => ValueTask.FromResult(new EchoReply
        {
            Message = req.Message + "|" + string.Join(",", ctx.Metadata.OrderBy(kv => kv.Key).Select(kv => $"{kv.Key}={kv.Value}")),
        }))
        .ServerStream<EchoRequest, EchoReply>("gen", Gen)
        .ServerStream<EchoRequest, EchoReply>("failMid", FailMid)
        .ClientStream<EchoRequest, EchoReply>("ingest", async (reqs, ctx) =>
        {
            if (Interlocked.Increment(ref _ingestCalls) <= _ingestFailUntil)
                throw new RpcError(StatusCode.Unavailable, "ingest flake");
            int n = 0;
            string all = "";
            await foreach (var r in reqs) { n++; all += r.Message; }
            return new EchoReply { Message = all, Count = n };
        })
        .BidiStream<EchoRequest, EchoReply>("chat", Chat)
        .Build();

    private ValueTask<EchoReply> Flaky(EchoRequest req, ServerCallContext ctx)
    {
        int n = Interlocked.Increment(ref _flakyCalls);
        if (ctx.Deadline is { } d) _flakyRemaining.Enqueue((d - DateTimeOffset.UtcNow).TotalSeconds);
        if (n <= Volatile.Read(ref _flakyFailUntil)) throw new RpcError(_flakyCode, $"flake {n}");
        return ValueTask.FromResult(new EchoReply { Message = req.Message, Count = n });
    }

    private static async IAsyncEnumerable<EchoReply> Gen(EchoRequest req, ServerCallContext ctx)
    {
        for (int i = 0; i < req.Count; i++)
        {
            await Task.Yield();
            yield return new EchoReply { Message = req.Message, Count = i };
        }
    }

    private static async IAsyncEnumerable<EchoReply> FailMid(EchoRequest req, ServerCallContext ctx)
    {
        yield return new EchoReply { Count = 0 };
        await Task.Yield();
        throw new RpcError(StatusCode.DataLoss, "mid-stream failure");
    }

    private static async IAsyncEnumerable<EchoReply> Chat(IAsyncEnumerable<EchoRequest> reqs, ServerCallContext ctx)
    {
        await foreach (var r in reqs) yield return new EchoReply { Message = r.Message, Count = r.Count };
    }

    public async Task InitializeAsync()
    {
        var svc = Service();
        _server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = Loopback,
            Services = { svc, RpcTests.InteropEcho("dotnet") },
        });
        _client = await AsterClient.StartAsync(Loopback);
        _client.RegisterTypes(svc);
        _conn = await _client.ConnectAsync(await _server.TicketAsync());
    }

    public async Task DisposeAsync()
    {
        await _client.DisposeAsync();
        await _server.DisposeAsync();
    }

    private static readonly CallOptions Idem = new() { Idempotent = true };

    private static RetryInterceptor FastRetry(int attempts = 3, params StatusCode[] codes)
        => new(attempts, codes.Length == 0 ? null : [.. codes], initialMs: 1, maxMs: 5, jitter: 0.0);

    private static EchoRequest Req(string m = "hi", int count = 0) => new() { Message = m, Count = count };

    /// <summary>Records hook order and the attempt each request hook saw.</summary>
    private sealed class Recorder(string name, ConcurrentQueue<string> log) : IInterceptor
    {
        public object OnRequest(CallContext ctx, object request) { log.Enqueue($"{name}.req#{ctx.Attempt}"); return request; }
        public object OnResponse(CallContext ctx, object response) { log.Enqueue($"{name}.resp"); return response; }
        public RpcError? OnError(CallContext ctx, RpcError error) { log.Enqueue($"{name}.err"); return error; }
    }

    private sealed class Mutator : IInterceptor
    {
        public object OnRequest(CallContext ctx, object request)
        {
            ctx.Metadata["x-trace"] = "t-1";
            return request is EchoRequest r ? new EchoRequest { Message = r.Message.ToUpperInvariant(), Count = r.Count } : request;
        }
        public object OnResponse(CallContext ctx, object response)
            => response is EchoReply r ? new EchoReply { Message = r.Message + "!", Count = r.Count } : response;
    }

    private sealed class Suppress : IInterceptor
    {
        public RpcError? OnError(CallContext ctx, RpcError error) => null;
    }

    private sealed class Probe : IInterceptor
    {
        public CallContext? Seen;
        public object OnRequest(CallContext ctx, object request) { Seen = ctx; return request; }
    }

    // ---- unary: retry -----------------------------------------------------------------------

    [Fact]
    public async Task Unary_Idempotent_Retries_Unavailable_Until_It_Succeeds()
    {
        _flakyFailUntil = 2;
        var log = new ConcurrentQueue<string>();
        var conn = _conn.WithInterceptors([new Recorder("a", log), FastRetry(3)]);
        var reply = await conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), Idem);
        Assert.Equal(3, reply.Count);
        Assert.Equal(3, _flakyCalls);
        Assert.Equal(["a.req#1", "a.err", "a.req#2", "a.err", "a.req#3", "a.resp"], log.ToArray());
    }

    [Fact]
    public async Task Unary_Retry_Exhaustion_Throws_The_Last_Error()
    {
        _flakyFailUntil = int.MaxValue;
        var conn = _conn.WithInterceptors([FastRetry(3)]);
        var e = await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), Idem));
        Assert.Equal(StatusCode.Unavailable, e.Code);
        Assert.Equal(3, _flakyCalls);
    }

    [Fact]
    public async Task Unary_NonIdempotent_Is_Not_Retried()
    {
        _flakyFailUntil = 1;
        var conn = _conn.WithInterceptors([FastRetry(3)]);
        var e = await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flakyNonIdem", Req()));
        Assert.Equal(StatusCode.Unavailable, e.Code);
        Assert.Equal(1, _flakyCalls);
    }

    [Fact]
    public async Task Unary_NonRetryable_Code_Is_Not_Retried()
    {
        _flakyFailUntil = 1;
        _flakyCode = StatusCode.PermissionDenied;
        var conn = _conn.WithInterceptors([FastRetry(3)]);
        var e = await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), Idem));
        Assert.Equal(StatusCode.PermissionDenied, e.Code);
        Assert.Equal(1, _flakyCalls);
    }

    [Fact]
    public async Task Unary_Custom_Retryable_Codes()
    {
        _flakyFailUntil = 1;
        _flakyCode = StatusCode.Aborted;
        var conn = _conn.WithInterceptors([FastRetry(2, StatusCode.Aborted)]);
        var reply = await conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), Idem);
        Assert.Equal(2, reply.Count);
    }

    [Fact]
    public async Task Without_A_Retry_Interceptor_Nothing_Is_Retried()
    {
        _flakyFailUntil = 1;
        var conn = _conn.WithInterceptors([new Recorder("a", new())]);
        await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), Idem));
        Assert.Equal(1, _flakyCalls);
    }

    [Fact]
    public async Task Cancelling_During_Backoff_Stops_The_Call_And_Is_Not_A_Breaker_Failure()
    {
        _flakyFailUntil = int.MaxValue;
        var breaker = new CircuitBreakerInterceptor(failureThreshold: 10);
        var conn = _conn.WithInterceptors([breaker, new RetryInterceptor(5, initialMs: 5000, maxMs: 5000, jitter: 0.0)]);
        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(400));
        var sw = Stopwatch.StartNew();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), Idem, cts.Token));
        Assert.True(sw.Elapsed < TimeSpan.FromSeconds(3), $"took {sw.Elapsed}");
        Assert.Equal(1, breaker.FailureCount);               // the one real failure, not the cancellation
        Assert.Equal(1, _flakyCalls);
    }

    // ---- unary: breaker ---------------------------------------------------------------------

    [Fact]
    public async Task Breaker_Opens_Fails_Fast_Then_Recovers()
    {
        _flakyFailUntil = int.MaxValue;
        var breaker = new CircuitBreakerInterceptor(failureThreshold: 2, recoveryTimeout: 0.3);
        var conn = _conn.WithInterceptors([breaker]);
        for (int i = 0; i < 2; i++)
            Assert.Equal(StatusCode.Unavailable,
                (await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req()))).Code);
        Assert.Equal(CircuitBreakerInterceptor.StateOpen, breaker.State);

        var open = await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req()));
        Assert.Equal(StatusCode.Unavailable, open.Code);
        Assert.Contains("circuit breaker is open", open.Message);
        Assert.Equal(2, _flakyCalls);                        // the open circuit never reached the server

        _flakyFailUntil = 0;
        await Task.Delay(450);
        var reply = await conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req());
        Assert.Equal(3, reply.Count);
        Assert.Equal(CircuitBreakerInterceptor.StateClosed, breaker.State);
    }

    [Fact]
    public async Task Breaker_Open_Is_Not_Retried_And_Skips_The_Hooks()
    {
        _flakyFailUntil = int.MaxValue;
        var log = new ConcurrentQueue<string>();
        var breaker = new CircuitBreakerInterceptor(failureThreshold: 1, recoveryTimeout: 30);
        var conn = _conn.WithInterceptors([breaker, new Recorder("a", log), FastRetry(5)]);
        await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), Idem));
        Assert.Equal(1, _flakyCalls);                        // attempt 2 hit the open breaker and stopped
        Assert.Equal(["a.req#1", "a.err"], log.ToArray());
    }

    [Fact]
    public async Task Breaker_Does_Not_Wrap_Streaming_Server_Failures()
    {
        var breaker = new CircuitBreakerInterceptor(failureThreshold: 1);
        var conn = _conn.WithInterceptors([breaker]);
        await Assert.ThrowsAsync<RpcError>(async () =>
        {
            await foreach (var _ in conn.ServerStreamAsync<EchoRequest, EchoReply>("Flaky", "failMid", Req())) { }
        });
        Assert.Equal(CircuitBreakerInterceptor.StateClosed, breaker.State);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task Native_Breaker_Abandoned_Probe_Then_Healthy_Recovery(bool cancelProbe)
    {
        _flakyFailUntil = 1;
        var first = new CircuitBreakerInterceptor(failureThreshold: 1, recoveryTimeout: 0);
        var second = new CircuitBreakerInterceptor(failureThreshold: 1, recoveryTimeout: 0.15);
        var conn = _conn.WithInterceptors(cancelProbe ? [first] : [first, second]);
        var seed = await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req()));
        Assert.Equal(StatusCode.Unavailable, seed.Code);
        if (cancelProbe)
        {
            using var cancel = new CancellationTokenSource();
            var pending = conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "slow", Req(), cancellationToken: cancel.Token);
            try
            {
                var until = DateTime.UtcNow.AddSeconds(3);
                while (Volatile.Read(ref _slowCalls) == 0)
                {
                    Assert.True(DateTime.UtcNow < until, "probe did not reach peer");
                    await Task.Delay(5);
                }
                cancel.Cancel();
                await Assert.ThrowsAnyAsync<OperationCanceledException>(() => pending);
            }
            finally { cancel.Cancel(); }
        }
        else
        {
            var refusal = await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req()));
            Assert.Equal(StatusCode.Unavailable, refusal.Code);
            Assert.Equal(1, _flakyCalls);
            await Task.Delay(180);
        }
        Assert.Equal(CircuitBreakerInterceptor.StateHalfOpen, first.State);
        Assert.Equal(1, first.FailureCount);
        var healthy = await conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req());
        Assert.Equal(2, healthy.Count);
        Assert.Equal(CircuitBreakerInterceptor.StateClosed, first.State);
        Assert.Equal(0, first.FailureCount);
        if (!cancelProbe) Assert.Equal(CircuitBreakerInterceptor.StateClosed, second.State);
    }

    // ---- deadlines --------------------------------------------------------------------------

    [Fact]
    public async Task Deadline_Bounds_Retries_To_The_Original_Budget()
    {
        var conn = _conn.WithInterceptors([new DeadlineInterceptor(), FastRetry(5, StatusCode.DeadlineExceeded)]);
        var sw = Stopwatch.StartNew();
        var e = await Assert.ThrowsAsync<RpcError>(() =>
            conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "slow", Req(), new CallOptions { Idempotent = true, Deadline = TimeSpan.FromSeconds(1) }));
        Assert.Equal(StatusCode.DeadlineExceeded, e.Code);
        Assert.True(sw.Elapsed < TimeSpan.FromSeconds(3), $"took {sw.Elapsed}");
        Assert.Equal(1, _slowCalls);                         // no time left for a second attempt
    }

    [Fact]
    public async Task Deadline_Interceptor_Passes_The_Remaining_Time_To_Each_Attempt()
    {
        // The first attempt fails fast; the retry waits 2.5 s. The server reports the time left on the
        // deadline it received, which travels in whole seconds (rounded up): with a deadline interceptor
        // the retry is sent ~7.5 s of the 10 s budget, so its header says 8 s and the handler can never
        // see more than that. A full-deadline attempt says 10 s. The handler measures when it runs, so
        // dispatch delay under a loaded parallel suite (seen at ~1 s on CI) is subtracted from both;
        // the thresholds leave 1.5 s for it.
        var o = new CallOptions { Idempotent = true, Deadline = TimeSpan.FromSeconds(10) };
        _flakyFailUntil = 1;
        var bounded = _conn.WithInterceptors([new DeadlineInterceptor(), new RetryInterceptor(2, null, 2500, 2500, 2.0, 0.0)]);
        await bounded.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), o);
        var seen = _flakyRemaining.ToArray();
        Assert.Equal(2, seen.Length);
        Assert.True(seen[0] > 8.5, $"first {seen[0]}");
        Assert.True(seen[1] <= 8.05, $"second {seen[1]} should be at most the 8 s left after the back-off");

        // Without one every attempt gets the full deadline.
        _flakyCalls = 0;
        _flakyFailUntil = 1;
        while (_flakyRemaining.TryDequeue(out _)) { }
        var plain = _conn.WithInterceptors([new RetryInterceptor(2, null, 2500, 2500, 2.0, 0.0)]);
        await plain.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), o);
        seen = _flakyRemaining.ToArray();
        Assert.Equal(2, seen.Length);
        Assert.True(seen.All(s => s > 8.5), string.Join(",", seen));
    }

    [Fact]
    public async Task Expired_Deadline_Is_Refused_Before_Sending()
    {
        var expire = new ExpireNow();
        var conn = _conn.WithInterceptors([expire, new DeadlineInterceptor()]);
        var e = await Assert.ThrowsAsync<RpcError>(() =>
            conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), new CallOptions { Deadline = TimeSpan.FromSeconds(5) }));
        Assert.Equal(StatusCode.DeadlineExceeded, e.Code);
        Assert.Equal(0, _flakyCalls);
    }

    private sealed class ExpireNow : IInterceptor
    {
        public object OnRequest(CallContext ctx, object request)
        {
            ctx.Deadline = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() / 1000.0 - 100;
            return request;
        }
    }

    [Fact]
    public async Task Native_Deadline_Still_Applies_Without_A_Deadline_Interceptor()
    {
        var conn = _conn.WithInterceptors([new Recorder("a", new())]);
        var e = await Assert.ThrowsAsync<RpcError>(() =>
            conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "slow", Req(), new CallOptions { Deadline = TimeSpan.FromSeconds(1) }));
        Assert.Equal(StatusCode.DeadlineExceeded, e.Code);
    }

    // ---- hooks on every pattern -------------------------------------------------------------

    [Fact]
    public async Task Hooks_Run_In_Order_Errors_In_Reverse_And_Metadata_Reaches_The_Server()
    {
        var log = new ConcurrentQueue<string>();
        var conn = _conn.WithInterceptors([new Recorder("a", log), new Mutator(), new Recorder("b", log)]);
        var reply = await conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "meta", Req("hello"));
        Assert.Equal("HELLO|x-trace=t-1!", reply.Message);   // request upper-cased, metadata sent, response suffixed
        Assert.Equal(["a.req#1", "b.req#1", "a.resp", "b.resp"], log.ToArray());

        _flakyFailUntil = 1;
        log.Clear();
        await Assert.ThrowsAsync<RpcError>(() => conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req()));
        Assert.Equal(["a.req#1", "b.req#1", "b.err", "a.err"], log.ToArray());
    }

    [Fact]
    public async Task Suppressing_An_Error_Returns_Default()
    {
        _flakyFailUntil = int.MaxValue;
        var conn = _conn.WithInterceptors([new Suppress()]);
        Assert.Null(await conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req()));
    }

    [Fact]
    public async Task Context_Describes_The_Call()
    {
        var probe = new Probe();
        var conn = _conn.WithInterceptors([probe]);
        await conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "meta", Req(), new CallOptions { Idempotent = true, Deadline = TimeSpan.FromSeconds(30) });
        var ctx = probe.Seen!;
        Assert.Equal("Flaky", ctx.Service);
        Assert.Equal("meta", ctx.Method);
        Assert.Equal("unary", ctx.Pattern);
        Assert.True(ctx.Idempotent);
        Assert.False(ctx.IsStreaming);
        Assert.InRange(ctx.RemainingSeconds!.Value, 25, 30);
        Assert.Equal(_conn.Connection.RemoteId(), ctx.Peer);
    }

    [Fact]
    public async Task ServerStream_Hooks_Per_Message_And_Error_Hooks()
    {
        var conn = _conn.WithInterceptors([new Mutator()]);
        var items = new List<EchoReply>();
        await foreach (var r in conn.ServerStreamAsync<EchoRequest, EchoReply>("Flaky", "gen", Req("ab", 3))) items.Add(r);
        Assert.Equal(["AB!", "AB!", "AB!"], items.Select(i => i.Message));

        var log = new ConcurrentQueue<string>();
        var failing = _conn.WithInterceptors([new Recorder("a", log)]);
        var e = await Assert.ThrowsAsync<RpcError>(async () =>
        {
            await foreach (var _ in failing.ServerStreamAsync<EchoRequest, EchoReply>("Flaky", "failMid", Req())) { }
        });
        Assert.Equal(StatusCode.DataLoss, e.Code);
        Assert.Equal(["a.req#1", "a.resp", "a.err"], log.ToArray());

        var quiet = _conn.WithInterceptors([new Suppress()]);
        int n = 0;
        await foreach (var _ in quiet.ServerStreamAsync<EchoRequest, EchoReply>("Flaky", "failMid", Req())) n++;
        Assert.Equal(1, n);                                  // suppressed: the stream just ends
    }

    [Fact]
    public async Task ServerStream_Is_Not_Retried()
    {
        var conn = _conn.WithInterceptors([FastRetry(3)]);
        await Assert.ThrowsAsync<RpcError>(async () =>
        {
            await foreach (var _ in conn.ServerStreamAsync<EchoRequest, EchoReply>("Flaky", "failMid", Req(), Idem)) { }
        });
    }

    [Fact]
    public async Task ClientStream_Hooks_Per_Message_And_Retry_Replays_The_Requests()
    {
        var conn = _conn.WithInterceptors([new Mutator(), FastRetry(3)]);
        _ingestFailUntil = 1;
        var reply = await conn.ClientStreamAsync<EchoRequest, EchoReply>("Flaky", "ingest", Msgs("a", "b", "c"), Idem);
        Assert.Equal("ABC!", reply.Message);                 // each message through the request hook, the reply through the response hook
        Assert.Equal(3, reply.Count);
        Assert.Equal(2, _ingestCalls);                       // attempt 1 failed, attempt 2 replayed all three
    }

    [Fact]
    public async Task ClientStream_Breaker_And_NonIdempotent()
    {
        _ingestFailUntil = int.MaxValue;
        var breaker = new CircuitBreakerInterceptor(failureThreshold: 2, recoveryTimeout: 30);
        var conn = _conn.WithInterceptors([breaker, FastRetry(3)]);
        // Not idempotent: no retry, one failure counted.
        await Assert.ThrowsAsync<RpcError>(() => conn.ClientStreamAsync<EchoRequest, EchoReply>("Flaky", "ingest", Msgs("a")));
        Assert.Equal(1, _ingestCalls);
        Assert.Equal(1, breaker.FailureCount);
        await Assert.ThrowsAsync<RpcError>(() => conn.ClientStreamAsync<EchoRequest, EchoReply>("Flaky", "ingest", Msgs("a")));
        var open = await Assert.ThrowsAsync<RpcError>(() => conn.ClientStreamAsync<EchoRequest, EchoReply>("Flaky", "ingest", Msgs("a")));
        Assert.Contains("circuit breaker is open", open.Message);
        Assert.Equal(2, _ingestCalls);
    }

    [Fact]
    public async Task Bidi_Hooks_Per_Message_In_Both_Directions()
    {
        var log = new ConcurrentQueue<string>();
        var conn = _conn.WithInterceptors([new Mutator(), new Recorder("a", log)]);
        var got = new List<string>();
        await foreach (var r in conn.BidiStreamAsync<EchoRequest, EchoReply>("Flaky", "chat", Msgs("x", "y", "z")))
            got.Add(r.Message);
        Assert.Equal(["X!", "Y!", "Z!"], got);
        Assert.Equal(3, log.Count(l => l == "a.resp"));
        Assert.Equal(3, log.Count(l => l.StartsWith("a.req")));
    }

    private static async IAsyncEnumerable<EchoRequest> Msgs(params string[] messages)
    {
        foreach (var m in messages)
        {
            await Task.Yield();
            yield return new EchoRequest { Message = m };
        }
    }

    // ---- client-level configuration, views, concurrency --------------------------------------

    [Fact]
    public async Task AsterClient_Applies_Its_Interceptors_To_Every_Connection()
    {
        _flakyFailUntil = 1;
        var svc = Service();
        await using var client = await AsterClient.StartAsync(Loopback, interceptors: [FastRetry(3)]);
        client.RegisterTypes(svc);
        Assert.Single(client.Interceptors);
        var conn = await client.ConnectAsync(await _server.TicketAsync());
        var reply = await conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req(), Idem);
        Assert.Equal(2, reply.Count);
        Assert.Single(conn.Interceptors);
    }

    [Fact]
    public async Task WithInterceptors_Is_Independent_And_Disposing_It_Keeps_The_Connection()
    {
        _flakyFailUntil = int.MaxValue;
        var a = _conn.WithInterceptors([new CircuitBreakerInterceptor(failureThreshold: 1, recoveryTimeout: 30)]);
        var b = _conn.WithInterceptors([new CircuitBreakerInterceptor(failureThreshold: 1, recoveryTimeout: 30)]);
        await Assert.ThrowsAsync<RpcError>(() => a.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req()));
        var open = await Assert.ThrowsAsync<RpcError>(() => a.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req()));
        Assert.Contains("open", open.Message);
        _flakyFailUntil = 0;
        Assert.Equal(2, (await b.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req())).Count);   // b's breaker is untouched
        await a.DisposeAsync();
        Assert.Equal(3, (await _conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req())).Count);
    }

    [Fact]
    public async Task No_Interceptors_Keeps_The_Direct_Path()
    {
        Assert.Empty(_conn.Interceptors);
        var reply = await _conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req("p"));
        Assert.Equal("p", reply.Message);
    }

    [Fact]
    public async Task Concurrent_Calls_Share_One_Interceptor_Stack()
    {
        var breaker = new CircuitBreakerInterceptor(failureThreshold: 5);
        var conn = _conn.WithInterceptors([new DeadlineInterceptor(), breaker, FastRetry(3), new Mutator()]);
        var replies = await Task.WhenAll(Enumerable.Range(0, 64).Select(i =>
            conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "meta", Req($"m{i}"), new CallOptions { Idempotent = true, Deadline = TimeSpan.FromSeconds(20) })));
        for (int i = 0; i < replies.Length; i++)
            Assert.Equal($"M{i}|x-trace=t-1!", replies[i].Message);
        Assert.Equal(CircuitBreakerInterceptor.StateClosed, breaker.State);
    }

    [Fact]
    public async Task Concurrent_Retrying_Calls()
    {
        _flakyFailUntil = 32;                                // the first 32 calls fail, whoever makes them
        var conn = _conn.WithInterceptors([FastRetry(40)]);
        var replies = await Task.WhenAll(Enumerable.Range(0, 16).Select(i =>
            conn.UnaryAsync<EchoRequest, EchoReply>("Flaky", "flaky", Req($"r{i}"), Idem)));
        Assert.Equal(16, replies.Select(r => r.Message).Distinct().Count());
        Assert.True(_flakyCalls >= 48);
    }
}
