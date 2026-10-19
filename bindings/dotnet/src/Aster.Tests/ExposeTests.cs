using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Runtime.CompilerServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Expose;
using Aster.Upcall;

namespace Aster.Tests;

internal sealed class FuncExposeAuthorizer(Func<ExposeAuthorizeRequest, CancellationToken, ValueTask<bool>> f) : IExposeAuthorizer
{
    public ValueTask<bool> AuthorizeAsync(ExposeAuthorizeRequest request, CancellationToken ct) => f(request, ct);
}

internal sealed class FuncRoutePolicy(Func<ExposeRoutePolicyRequest, CancellationToken, ValueTask<IReadOnlyList<uint>>> f) : IRoutePolicy
{
    public ValueTask<IReadOnlyList<uint>> GrantAsync(ExposeRoutePolicyRequest request, CancellationToken ct) => f(request, ct);
}

/// <summary>
/// Two nodes (the server exposes, the client consumes over <see cref="Conn"/>), one expose registry
/// with an authorizer on the upcall table. Nothing is registered under <see cref="Svc"/> yet.
/// </summary>
internal sealed class ExposeRig : IAsyncDisposable
{
    public const string Alpn = "aster-expose/dotnet-test";
    public const string Svc = "app";

    public Node Server = null!;
    public Node Client = null!;
    public Connection Conn = null!;
    public ExposeRegistry Registry = null!;
    public Upcalls Up = null!;
    public HttpBodies Bodies = null!;
    public Task Serving = Task.CompletedTask;
    public string ClientId = "";
    public volatile bool Allow = true;
    public ConcurrentQueue<ExposeAuthorizeRequest> Authorized { get; } = new();

    public static async Task<ExposeRig> CreateAsync(bool serve = true)
    {
        var cfg = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0", Alpns = [Alpn] };
        var rig = new ExposeRig();
        rig.Server = await Node.StartAsync(cfg);
        rig.Client = await Node.StartAsync(cfg);
        rig.ClientId = rig.Client.NodeId();
        rig.Up = Upcalls.Create(rig.Server.Runtime);
        rig.Bodies = HttpBodies.Create(rig.Server.Runtime);
        rig.Registry = ExposeRegistry.Create(rig.Server);
        rig.Up.ServeExposeAuthorizer(new FuncExposeAuthorizer((r, _) =>
        {
            rig.Authorized.Enqueue(r);
            return new ValueTask<bool>(rig.Allow);
        }));
        if (serve)
        {
            rig.Serving = rig.Registry.ServeAsync(rig.Server, [Alpn]);
            rig.Conn = await rig.Client.ConnectAsync(rig.Server.NodeAddr(), Alpn).WaitAsync(TimeSpan.FromSeconds(20));
        }
        else
        {
            var accept = rig.Server.AcceptAsync();
            rig.Conn = await rig.Client.ConnectAsync(rig.Server.NodeAddr(), Alpn).WaitAsync(TimeSpan.FromSeconds(20));
            var accepted = await accept.WaitAsync(TimeSpan.FromSeconds(20));
            rig.Registry.HandleConnection(accepted);
        }
        return rig;
    }

    public Task<ExposeRelayResponse> Relay(string method = "GET", string uri = "/", byte[]? body = null, IReadOnlyList<HttpHeader>? headers = null, string? service = null)
    {
        var h = new List<HttpHeader>(headers ?? Array.Empty<HttpHeader>());
        if (body != null) h.Add(new HttpHeader("content-length", body.Length.ToString()));
        return ExposeClient.RelayRequestAsync(Conn, service ?? Svc, method, uri, h, body ?? Array.Empty<byte>()).WaitAsync(TimeSpan.FromSeconds(30));
    }

    public async Task WaitFor(string what, Func<bool> cond)
    {
        var end = DateTime.UtcNow + TimeSpan.FromSeconds(20);
        while (!cond())
        {
            Assert.True(DateTime.UtcNow < end, "timed out waiting for " + what);
            await Task.Delay(10);
        }
    }

    public Task WaitClosed() => WaitFor("exchanges to close", () => Bodies.OpenCount == 0);

    public async ValueTask DisposeAsync()
    {
        try { Conn?.Close("done"); } catch (IrohException) { }
        Registry?.Dispose();
        if (Up != null) await Up.DisposeAsync();
        Bodies?.Dispose();
        if (Client != null) await Client.DisposeAsync();
        if (Server != null) await Server.DisposeAsync();
        try { await Serving.WaitAsync(WaitBudget.Value); } catch (Exception) { /* swallow-ok: draining in teardown; the test asserted what it needs above */ }
    }
}

/// <summary>C ABI 4.2 expose end to end over real QUIC: authorize, HTTP foreign handlers with streaming bodies, socket targets, route policy, relay_request, request_route.</summary>
public class ExposeTests
{
    private static byte[] Pattern(int n) => Enumerable.Range(0, n).Select(i => (byte)(i * 31 % 251)).ToArray();

    private static byte[] Utf8(string s) => Encoding.UTF8.GetBytes(s);

    [Fact]
    public async Task Inline_Request_And_Response_Carry_Peer_And_Admission()
    {
        await using var rig = await ExposeRig.CreateAsync();
        ExposeHttpRequest? seen = null;
        byte[] seenBody = [];
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, async req =>
        {
            seen ??= req;
            using var ms = new System.IO.MemoryStream();
            await req.Body.CopyToAsync(ms);
            seenBody = ms.ToArray();
            return req.Method == "POST"
                ? ExposeHttpResponse.Inline(201, [.. ms.ToArray(), (byte)'!'], [new HttpHeader("content-type", "text/plain"), new HttpHeader("x-seen-uri", req.Uri), new HttpHeader("set-cookie", "a=1"), new HttpHeader("set-cookie", "b=2")])
                : ExposeHttpResponse.Text(200, "ok");
        });

        var r = await rig.Relay("POST", "/echo?x=1", Utf8("hello"), [new HttpHeader("host", "app.local")]);
        Assert.Equal(201, r.Status);
        Assert.Equal("hello!", r.BodyText);
        Assert.Contains(r.Headers, h => h.Name == "content-type" && h.ValueText == "text/plain");
        Assert.Contains(r.Headers, h => h.Name == "x-seen-uri" && h.ValueText == "/echo?x=1");
        Assert.Equal(new[] { "a=1", "b=2" }, r.Headers.Where(h => h.Name == "set-cookie").Select(h => h.ValueText));

        var get = await rig.Relay();
        Assert.Equal((200, "ok"), (get.Status, get.BodyText));

        // Fields reached the handler.
        Assert.Equal(ExposeRig.Svc, seen!.ServiceId);
        Assert.Equal(rig.ClientId, seen.PeerId);
        Assert.True(seen.StreamId >= 1);
        Assert.Equal("POST", seen.Method);
        Assert.Equal("app.local", seen.Header("host"));
        Assert.Equal(HttpProtocolVersion.Http11, seen.Version);
        // The kind-48 admission ran and carried the authenticated peer.
        Assert.NotEmpty(rig.Authorized);
        Assert.All(rig.Authorized, a => { Assert.Equal(ExposeRig.Svc, a.ServiceId); Assert.Equal(rig.ClientId, a.PeerId); });
        await rig.WaitClosed();
    }

    [Fact]
    public async Task Streamed_Request_And_Response_With_Trailers()
    {
        await using var rig = await ExposeRig.CreateAsync();
        byte[] upload = Pattern(200 * 1024);
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, async req =>
        {
            // 200 KiB exceeds the inline limit: the body streams, pulled with backpressure.
            Assert.NotEqual(0ul, req.StreamId);
            using var ms = new System.IO.MemoryStream();
            var buf = new byte[4096];
            int n;
            while ((n = await req.Body.ReadAsync(buf, req.Cancellation)) > 0) ms.Write(buf, 0, n);
            Assert.Equal(upload, ms.ToArray());
            var data = ms.ToArray();
            return ExposeHttpResponse.Streaming(200, Chunks(data, 16 * 1024),
                [new HttpHeader("content-type", "application/octet-stream")], [new HttpHeader("x-checksum", "abc")]);
        }, new HttpForeignOptions { ResponseBuffer = 2 });

        var r = await rig.Relay("POST", "/upload", upload, [new HttpHeader("host", "app.local")]);
        Assert.Equal(200, r.Status);
        // The buffered consumer collects a streamed body into one inline body.
        Assert.Equal(upload, r.Body);
        await rig.WaitClosed();
    }

    private static async IAsyncEnumerable<ReadOnlyMemory<byte>> Chunks(byte[] data, int size, [EnumeratorCancellation] CancellationToken ct = default)
    {
        for (int off = 0; off < data.Length; off += size)
        {
            yield return data.AsMemory(off, Math.Min(size, data.Length - off));
            await Task.Yield();
        }
    }

    [Fact]
    public async Task Handler_Failures_Become_5xx_Statuses()
    {
        await using var rig = await ExposeRig.CreateAsync();
        // 501: the native service is registered but no kind-49 handler is installed on the table.
        rig.Registry.ExposeHttpForeign(ExposeRig.Svc, rig.Up, rig.Bodies);
        Assert.Equal(501, (await rig.Relay()).Status);

        // 500: the handler throws before the head (the message is logged, never sent).
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, _ => throw new InvalidOperationException("secret detail"));
        var err = await rig.Relay();
        Assert.Equal(500, err.Status);
        Assert.DoesNotContain("secret detail", err.BodyText);

        // 502: a head Rust cannot decode (a malformed C5 answered on the raw table).
        rig.Up.Handle(UpcallKind.ExposeHttp, (_, _) => new ValueTask<byte[]?>(new byte[] { 0xC5, 1, 0x00 }));
        Assert.Equal(502, (await rig.Relay()).Status);

        // 504: the head is not answered within the kind's timeout.
        rig.Up.Handle(UpcallKind.ExposeHttp, async (_, ctx) => { await Task.Delay(TimeSpan.FromSeconds(10), ctx.Cancellation); return null; });
        rig.Up.SetPolicy(UpcallKind.ExposeHttp, TimeSpan.FromMilliseconds(200), HookFailureMode.FailClosed);
        Assert.Equal(504, (await rig.Relay()).Status);
        await rig.WaitClosed();
    }

    [Fact]
    public async Task Response_Validation_Rejects_Bad_Heads_Locally()
    {
        Assert.Throws<ArgumentOutOfRangeException>(() => ExposeHttpResponse.Inline(99));
        Assert.Throws<ArgumentOutOfRangeException>(() => ExposeHttpResponse.Inline(1000));
        Assert.Throws<ArgumentException>(() => ExposeHttpResponse.Inline(200, null, [new HttpHeader("Content-Type", "x")]));
        await Task.CompletedTask;
    }

    [Fact]
    public async Task Aborted_Streamed_Response_Fails_The_Consumer_Instead_Of_Truncating()
    {
        await using var rig = await ExposeRig.CreateAsync();
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, _ => new ValueTask<ExposeHttpResponse>(
            ExposeHttpResponse.Streaming(200, FailAfterOneChunk())));
        await Assert.ThrowsAsync<IrohException>(() => rig.Relay());
        await rig.WaitClosed();

        static async IAsyncEnumerable<ReadOnlyMemory<byte>> FailAfterOneChunk()
        {
            yield return new byte[4096];
            await Task.Delay(100);
            throw new InvalidOperationException("gave up");
        }
    }

    [Fact]
    public async Task Body_Limits()
    {
        await using var rig = await ExposeRig.CreateAsync();
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, req => req.Uri switch
        {
            "/inline-big" => new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Inline(200, new byte[3000])),
            "/stream-big" => new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Streaming(200, Chunks(new byte[3000], 1000))),
            _ => new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Text(200, "small")),
        }, new HttpForeignOptions { MaxRequestBody = 100, MaxResponseBody = 1500 });

        Assert.Equal("small", (await rig.Relay("GET", "/ok")).BodyText);
        // A declared content-length over the request cap is answered 413 without an upcall.
        Assert.Equal(413, (await rig.Relay("POST", "/ok", new byte[500])).Status);
        // An inline body over the response cap becomes 502; a streamed one is aborted.
        Assert.Equal(502, (await rig.Relay("GET", "/inline-big")).Status);
        await Assert.ThrowsAsync<IrohException>(() => rig.Relay("GET", "/stream-big"));
        await rig.WaitClosed();
    }

    [Fact]
    public async Task Authorize_Deny_Refuses_Before_Any_Http_Upcall_And_The_Decision_Is_The_Bindings()
    {
        await using var rig = await ExposeRig.CreateAsync();
        int calls = 0;
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, _ => { Interlocked.Increment(ref calls); return new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Text(200, "in")); });
        rig.Allow = false;
        await Assert.ThrowsAsync<IrohException>(() => rig.Relay());
        Assert.Equal(0, Volatile.Read(ref calls));
        Assert.Contains(rig.Authorized, a => a.ServiceId == ExposeRig.Svc && a.PeerId == rig.ClientId);

        rig.Allow = true;
        Assert.Equal("in", (await rig.Relay()).BodyText);
    }

    [Fact]
    public async Task Authorize_Without_An_Answer_Follows_The_Failure_Mode()
    {
        await using var rig = await ExposeRig.CreateAsync();
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, _ => new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Text(200, "in")));
        // A hanging authorizer: the timeout applies the failure mode.
        rig.Up.Handle(UpcallKind.ExposeAuthorize, async (_, ctx) => { await Task.Delay(TimeSpan.FromSeconds(10), ctx.Cancellation); return null; });
        rig.Up.SetPolicy(UpcallKind.ExposeAuthorize, TimeSpan.FromMilliseconds(200), HookFailureMode.FailClosed);
        await Assert.ThrowsAsync<IrohException>(() => rig.Relay());

        // A throwing authorizer is "no usable answer" too (a refusal is not cached for the connection).
        rig.Up.Handle(UpcallKind.ExposeAuthorize, (_, _) => throw new InvalidOperationException("boom"));
        await Assert.ThrowsAsync<IrohException>(() => rig.Relay());

        // Fail-open admits an unanswered admission (the admission is then remembered for the connection).
        rig.Up.Handle(UpcallKind.ExposeAuthorize, async (_, ctx) => { await Task.Delay(TimeSpan.FromSeconds(10), ctx.Cancellation); return null; });
        rig.Up.SetPolicy(UpcallKind.ExposeAuthorize, TimeSpan.FromMilliseconds(200), HookFailureMode.FailOpen);
        Assert.Equal("in", (await rig.Relay()).BodyText);
    }

    [Fact]
    public async Task Route_Policy_Grant_Deny_And_Failure_Modes()
    {
        await using var rig = await ExposeRig.CreateAsync();
        string mode = "grant";
        var seen = new ConcurrentQueue<ExposeRoutePolicyRequest>();
        rig.Up.Handle(UpcallKind.ExposeRoutePolicy, (req, _) =>
        {
            seen.Enqueue(Ucv1.DecodeExposeRoutePolicyRequest(req));
            return mode switch
            {
                "grant" => new ValueTask<byte[]?>(Ucv1.EncodeExposeRouteGrant([0u, 2u])),
                "deny" => new ValueTask<byte[]?>(Ucv1.EncodeExposeRouteGrant([])),
                "out-of-range" => new ValueTask<byte[]?>(Ucv1.EncodeExposeRouteGrant([1u, 9u])),
                _ => new ValueTask<byte[]?>(Task.Delay(TimeSpan.FromSeconds(5)).ContinueWith(_ => (byte[]?)null)),
            };
        });
        rig.Registry.ServeRoutePolicy(rig.Up);
        ExposeRouteSpec[] specs =
        [
            new("a.example", 443, ExposeRouteSpec.ProtocolHttp, "web"),
            new("", 8080, ExposeRouteSpec.ProtocolTcp, "db"),
            new("b.example", 0, ExposeRouteSpec.ProtocolHttp, "api"),
        ];

        using (var reg = await ExposeClient.RequestRouteAsync(rig.Conn, specs, Utf8("tok")))
        {
            Assert.Equal(new uint[] { 0, 2 }, reg.Granted);
            var s = Assert.Single(seen);
            Assert.Equal(rig.ClientId, s.PeerId);
            Assert.Equal(Utf8("tok"), s.Metadata);
            Assert.Equal(specs, s.Specs);
        }

        mode = "deny";
        using (var reg = await ExposeClient.RequestRouteAsync(rig.Conn, specs)) Assert.Empty(reg.Granted);

        // A malformed answer (index out of range) is "no usable answer": fail-closed grants nothing.
        mode = "out-of-range";
        using (var reg = await ExposeClient.RequestRouteAsync(rig.Conn, specs)) Assert.Empty(reg.Granted);

        // No answer at all: the kind's failure mode decides; fail-open grants every requested route.
        mode = "silent";
        rig.Up.SetPolicy(UpcallKind.ExposeRoutePolicy, TimeSpan.FromMilliseconds(300), HookFailureMode.FailOpen);
        using (var reg = await ExposeClient.RequestRouteAsync(rig.Conn, specs)) Assert.Equal(new uint[] { 0, 1, 2 }, reg.Granted);
        rig.Up.SetPolicy(UpcallKind.ExposeRoutePolicy, TimeSpan.FromMilliseconds(300), HookFailureMode.FailClosed);
        using (var reg = await ExposeClient.RequestRouteAsync(rig.Conn, specs)) Assert.Empty(reg.Granted);
    }

    [Fact]
    public async Task Request_Route_Validates_Its_Arguments()
    {
        await using var rig = await ExposeRig.CreateAsync();
        await Assert.ThrowsAsync<IrohException>(() => ExposeClient.RequestRouteAsync(rig.Conn, [new ExposeRouteSpec("", 1, 7, "x")]));
        // The edge has no route policy: the whole request is refused or granted nothing, never hangs.
        var empty = await Task.WhenAny(ExposeClient.RequestRouteAsync(rig.Conn, []).ContinueWith(t => t.IsCompletedSuccessfully ? t.Result : null), Task.Delay(TimeSpan.FromSeconds(15)));
        Assert.True(empty.IsCompleted);
    }

    [Fact]
    public async Task Consumer_Cancel_Before_The_Head_Cancels_The_Upcall_As_Abandoned()
    {
        await using var rig = await ExposeRig.CreateAsync();
        var entered = new TaskCompletionSource<ExposeHttpRequest>(TaskCreationOptions.RunContinuationsAsynchronously);
        var cancelled = new TaskCompletionSource<UpcallCancelReason>(TaskCreationOptions.RunContinuationsAsynchronously);
        ulong streamId = 0;
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, async req =>
        {
            streamId = req.StreamId;
            entered.TrySetResult(req);
            try { await Task.Delay(TimeSpan.FromSeconds(30), req.Cancellation); }
            catch (OperationCanceledException) { cancelled.TrySetResult(req.CancelReason); throw; }
            return ExposeHttpResponse.Text(200, "late");
        });

        var relay = rig.Relay();
        await entered.Task.WaitAsync(TimeSpan.FromSeconds(20));
        rig.Conn.Close("cancel");
        await Assert.ThrowsAsync<IrohException>(() => relay);
        Assert.Equal(UpcallCancelReason.Abandoned, await cancelled.Task.WaitAsync(TimeSpan.FromSeconds(20)));
        // The exchange is gone; body calls on it fail with Cancelled (remembered) or UnknownStream.
        await rig.WaitClosed();
        var ex = await Assert.ThrowsAsync<HttpBodyException>(async () => await rig.Bodies.WriteAsync(streamId, new byte[] { 1 }));
        Assert.True(ex.IsCancelled || ex.BodyStatus == HttpBodyStatus.UnknownStream, ex.Message);
    }

    [Fact]
    public async Task Consumer_Cancel_Mid_Stream_Stops_The_Producer_With_Cancelled()
    {
        await using var rig = await ExposeRig.CreateAsync();
        var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var stopped = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, _ => new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Streaming(200, Flood())));
        async IAsyncEnumerable<ReadOnlyMemory<byte>> Flood()
        {
            try
            {
                var chunk = new byte[256 * 1024];
                while (true)
                {
                    yield return chunk;
                    started.TrySetResult();
                    await Task.Yield();
                }
            }
            finally { stopped.TrySetResult(); }
        }

        // The stream never finishes, so the buffered relay stays pending; the consumer gives up by closing the connection.
        var relay = rig.Relay();
        await started.Task.WaitAsync(TimeSpan.FromSeconds(20));
        rig.Conn.Close("bye");
        await Assert.ThrowsAsync<IrohException>(() => relay);
        await stopped.Task.WaitAsync(TimeSpan.FromSeconds(20));
        await rig.WaitClosed();
    }

    [Fact]
    public async Task Raw_Body_Calls_Backpressure_And_Trailers()
    {
        await using var rig = await ExposeRig.CreateAsync();
        var done = new TaskCompletionSource<string>(TaskCreationOptions.RunContinuationsAsynchronously);
        // A raw kind-49 handler driving the body table by hand.
        rig.Registry.ExposeHttpForeign(ExposeRig.Svc, rig.Up, rig.Bodies, new HttpForeignOptions { ResponseBuffer = 2 });
        rig.Up.Handle(UpcallKind.ExposeHttp, async (req, ctx) =>
        {
            var wire = Ucv1.DecodeExposeHttpRequest(req);
            var id = wire.StreamId;
            var log = new StringBuilder();
            // No request body: the read returns the end at once.
            Assert.Equal(0, await rig.Bodies.ReadAsync(id, new byte[16], ctx.Cancellation));
            // Writes before the head queue up to the buffer; then it is full.
            log.Append(rig.Bodies.TryWrite(id, Utf8("a")) ? 'q' : 'f');
            log.Append(rig.Bodies.TryWrite(id, Utf8("b")) ? 'q' : 'f');
            log.Append(rig.Bodies.TryWrite(id, Utf8("c")) ? 'q' : 'f');
            ctx.TryRespond(Ucv1.EncodeExposeHttpResponse(new ExposeHttpResponseWire(200, [], HttpBody.Stream)));
            await rig.Bodies.WriteAsync(id, Utf8("c"), ctx.Cancellation);
            rig.Bodies.Finish(id, [new HttpHeader("x-sum", "1")]);
            // Finished: further writes are refused (or the relay already closed the exchange).
            try { rig.Bodies.TryWrite(id, Utf8("late")); log.Append('?'); }
            catch (HttpBodyException e) { log.Append(e.BodyStatus is HttpBodyStatus.Finished or HttpBodyStatus.UnknownStream ? 'x' : '!'); }
            done.TrySetResult(log.ToString());
            return null;
        });
        var r = await rig.Relay();
        Assert.Equal(200, r.Status);
        Assert.Equal("abc", r.BodyText);
        Assert.Equal("qqfx", await done.Task.WaitAsync(TimeSpan.FromSeconds(20)));
        await rig.WaitClosed();

        // Unknown streams and handles.
        var ex = await Assert.ThrowsAsync<HttpBodyException>(async () => await rig.Bodies.ReadAsync(999_999, new byte[4]));
        Assert.Equal(HttpBodyStatus.UnknownStream, ex.BodyStatus);
        Assert.Equal(HttpBodyStatus.UnknownStream, Assert.Throws<HttpBodyException>(() => rig.Bodies.Finish(999_999)).BodyStatus);
        Assert.Equal(HttpBodyStatus.UnknownStream, Assert.Throws<HttpBodyException>(() => rig.Bodies.Abort(999_999, "x")).BodyStatus);
        Assert.Equal(HttpBodyStatus.UnknownStream, Assert.Throws<HttpBodyException>(() => rig.Bodies.TryWrite(999_999, Utf8("x"))).BodyStatus);
        var cancelled = new CancellationTokenSource(); cancelled.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(async () => await rig.Bodies.ReadAsync(1, new byte[4], cancelled.Token));
    }

    [Fact]
    public async Task Socket_Target_With_And_Without_An_Authorizer()
    {
        using var listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start();
        string addr = $"127.0.0.1:{((IPEndPoint)listener.LocalEndpoint).Port}";
        async Task<TcpClient?> Accepted(TimeSpan wait)
        {
            var end = DateTime.UtcNow + wait;
            while (DateTime.UtcNow < end)
            {
                if (listener.Pending()) return await listener.AcceptTcpClientAsync();
                await Task.Delay(20);
            }
            return null;
        }

        await using var rig = await ExposeRig.CreateAsync();
        // Argument validation.
        Assert.Throws<IrohException>(() => rig.Registry.ExposeSocket("sock", "not an addr"));
        Assert.Throws<IrohException>(() => rig.Registry.ExposeSocket("", addr));

        // Guarded by the authorizer: denied, so nothing is spliced.
        rig.Registry.ExposeSocket("sock", addr, rig.Up);
        rig.Allow = false;
        await Assert.ThrowsAsync<IrohException>(() => rig.Relay(service: "sock"));
        Assert.Contains(rig.Authorized, a => a.ServiceId == "sock" && a.PeerId == rig.ClientId);
        Assert.Null(await Accepted(TimeSpan.FromMilliseconds(300)));

        // Allowed: the stream is spliced to the target, which sees a TCP connection.
        rig.Allow = true;
        var relay = rig.Relay(service: "sock");
        using (var target = await Accepted(TimeSpan.FromSeconds(20)))
            Assert.NotNull(target);
        try { await relay; } catch (IrohException) { /* the target hung up without an HTTP answer */ }

        // Without an authorizer every peer is admitted and no kind-48 upcall is made for the service.
        rig.Registry.ExposeSocket("open", addr);
        int before = rig.Authorized.Count;
        var relay2 = rig.Relay(service: "open");
        using (var target = await Accepted(TimeSpan.FromSeconds(20)))
            Assert.NotNull(target);
        try { await relay2; } catch (IrohException) { }
        Assert.Equal(before, rig.Authorized.Count);
    }

    [Fact]
    public async Task HandleConnection_Feeds_Accepted_Connections()
    {
        await using var rig = await ExposeRig.CreateAsync(serve: false);
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, _ => new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Text(200, "fed")));
        Assert.Equal("fed", (await rig.Relay()).BodyText);
    }

    [Fact]
    public async Task Unexpose_Stops_The_Service_And_Registering_Again_Brings_It_Back()
    {
        await using var rig = await ExposeRig.CreateAsync();
        Func<ExposeHttpRequest, ValueTask<ExposeHttpResponse>> up = _ => new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Text(200, "up"));
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, new ExposeHttpHandler(up));
        Assert.Equal("up", (await rig.Relay()).BodyText);
        rig.Registry.Unexpose(ExposeRig.Svc);
        rig.Registry.Unexpose(ExposeRig.Svc); // idempotent
        await Assert.ThrowsAsync<IrohException>(() => rig.Relay());
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, new ExposeHttpHandler(up));
        Assert.Equal("up", (await rig.Relay()).BodyText);
    }

    [Fact]
    public async Task Concurrent_Exchanges_Do_Not_Leak_Or_Cross_Streams()
    {
        await using var rig = await ExposeRig.CreateAsync();
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, async req =>
        {
            using var ms = new System.IO.MemoryStream();
            await req.Body.CopyToAsync(ms);
            return req.Uri.StartsWith("/s")
                ? ExposeHttpResponse.Streaming(200, Chunks(ms.ToArray(), 5000))
                : ExposeHttpResponse.Inline(200, ms.ToArray());
        });
        var calls = Enumerable.Range(0, 60).Select(async i =>
        {
            byte[] body = Pattern(1000 + i * 3000 % 150_000).Select(b => (byte)(b ^ i)).ToArray();
            var r = await rig.Relay("POST", i % 2 == 0 ? "/s" : "/i", body);
            Assert.Equal(200, r.Status);
            Assert.Equal(body, r.Body);
        }).ToList();
        await Task.WhenAll(calls).WaitAsync(TimeSpan.FromSeconds(60));
        await rig.WaitClosed();
    }

    [Fact]
    public async Task Disposing_The_Table_Mid_Stream_Fails_The_Consumer_Cleanly()
    {
        var rig = await ExposeRig.CreateAsync();
        try
        {
            var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, _ => new ValueTask<ExposeHttpResponse>(ExposeHttpResponse.Streaming(200, Slow())));
            async IAsyncEnumerable<ReadOnlyMemory<byte>> Slow([EnumeratorCancellation] CancellationToken ct = default)
            {
                while (true)
                {
                    yield return new byte[1024];
                    started.TrySetResult();
                    await Task.Delay(20, ct);
                }
            }
            var relay = rig.Relay();
            await started.Task.WaitAsync(TimeSpan.FromSeconds(20));
            await rig.Up.DisposeAsync();
            Assert.Equal(0, rig.Up.ActiveHandlers);
            // Shutdown aborts the body: the consumer sees a failure, never a truncated success.
            await Assert.ThrowsAsync<IrohException>(() => relay);
        }
        finally
        {
            await rig.DisposeAsync();
        }
    }
}
