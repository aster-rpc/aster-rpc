using System.Buffers.Binary;
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Net;
using System.Net.Http.Headers;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Security.Cryptography.X509Certificates;
using System.Text;
using Aster.Codec;
using Aster.Interceptors;
using Aster.Rpc;
using Aster.Upcall;
using OwnedListener = Aster.AsterHttpListener;

namespace Aster.Tests;

public class HttpListenerTests
{
    private static readonly TimeSpan Budget = TimeSpan.FromSeconds(10);
    private static TaskCompletionSource Signal() => new(TaskCreationOptions.RunContinuationsAsynchronously);

    private static HttpListenerOptions Pem(bool webTransport = false)
    {
        using var key = ECDsa.Create(ECCurve.NamedCurves.nistP256);
        var request = new CertificateRequest("CN=localhost", key, HashAlgorithmName.SHA256);
        var names = new SubjectAlternativeNameBuilder();
        names.AddIpAddress(IPAddress.Loopback);
        request.CertificateExtensions.Add(names.Build());
        using var cert = request.CreateSelfSigned(DateTimeOffset.UtcNow.AddMinutes(-5), DateTimeOffset.UtcNow.AddDays(1));
        return new HttpListenerOptions
        {
            CertificatePem = Encoding.ASCII.GetBytes(cert.ExportCertificatePem()),
            PrivateKeyPem = Encoding.ASCII.GetBytes(key.ExportPkcs8PrivateKeyPem()),
            WebTransport = webTransport,
        };
    }

    private sealed class Fixture : IAsyncDisposable
    {
        internal AsterServer Server = null!;
        internal Upcalls Upcalls = null!;
        internal readonly ForyCodec Codec = new();
        internal readonly ConcurrentQueue<ServerCallContext> Seen = new();
        internal readonly TaskCompletionSource HandlerEntered = Signal();
        internal readonly TaskCompletionSource HandlerCanceled = Signal();
        internal int SessionFactories;

        internal static async Task<Fixture> Create(bool publicService = false, bool sessionService = false)
        {
            var f = new Fixture();
            var builder = ServiceDefinition.Create("HTTPTest", scope: sessionService ? ServiceScope.Session : ServiceScope.Shared)
                .Unary<EchoRequest, EchoReply>("echo", (r, ctx) =>
                {
                    f.Seen.Enqueue(ctx);
                    return ValueTask.FromResult(new EchoReply { Message = r.Message, Count = r.Count + 1, Data = r.Data, ServerLang = ctx.PeerId });
                })
                .ServerStream<EchoRequest, EchoReply>("generate", (r, ctx) => Generate(r, ctx, f))
                .ClientStream<EchoRequest, EchoReply>("ingest", async (requests, ctx) =>
                {
                    f.Seen.Enqueue(ctx);
                    int count = 0;
                    await foreach (var r in requests) count += r.Count;
                    return new EchoReply { Count = count, ServerLang = ctx.PeerId };
                })
                .BidiStream<EchoRequest, EchoReply>("chat", (requests, ctx) => Chat(requests, ctx, f))
                .Unary<EchoRequest, EchoReply>("hold", async (_, ctx) =>
                {
                    f.HandlerEntered.TrySetResult();
                    try { await Task.Delay(Timeout.InfiniteTimeSpan, ctx.CancellationToken); }
                    finally { f.HandlerCanceled.TrySetResult(); }
                    return new EchoReply();
                })
                .Unary<EchoRequest, EchoReply>("restricted", (_, _) => throw new InvalidOperationException("public Gate3 bypass"), requires: CapabilityRequirement.Role("admin"));
            if (!publicService) builder.Requires(CapabilityRequirement.Role("admin"));
            if (sessionService) builder.SessionFactory(_ => { Interlocked.Increment(ref f.SessionFactories); return new object(); });
            var service = builder.Build();
            f.Server = await AsterServer.StartAsync(new AsterServerOptions
            {
                Config = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" },
                Codec = f.Codec,
                Services = { service },
            }).WaitAsync(Budget);
            f.Upcalls = Upcalls.Create(f.Server.Node.Runtime, 8);
            return f;
        }

        private static async IAsyncEnumerable<EchoReply> Generate(EchoRequest r, ServerCallContext ctx, Fixture f)
        {
            f.Seen.Enqueue(ctx);
            for (int i = 0; i < r.Count; i++) { await Task.Yield(); yield return new EchoReply { Count = i, Message = r.Message, ServerLang = ctx.PeerId }; }
        }

        private static async IAsyncEnumerable<EchoReply> Chat(IAsyncEnumerable<EchoRequest> requests, ServerCallContext ctx, Fixture f)
        {
            f.Seen.Enqueue(ctx);
            await foreach (var r in requests) yield return new EchoReply { Message = r.Message.ToUpperInvariant(), Count = r.Count, ServerLang = ctx.PeerId };
        }

        internal void Authenticate() => Upcalls.ServeRpcAuthenticator((req, _) =>
        {
            Assert.Equal("HTTPTest", req.Service);
            return req.Metadata.GetValueOrDefault("authorization") switch
            {
                "Bearer alice" => ValueTask.FromResult(Accepted("alice", "one")),
                "Bearer bob" => ValueTask.FromResult(Accepted("bob", "two")),
                "Bearer low" => ValueTask.FromResult(new RpcAuthResponse(true, "low", new Dictionary<string, string>())),
                "Bearer backend" => throw new InvalidOperationException("secret credential backend failure"),
                _ => ValueTask.FromResult(RpcAuthResponse.Rejected()),
            };
        });

        public async ValueTask DisposeAsync()
        {
            await Upcalls.DisposeAsync();
            await Server.DisposeAsync();
        }
    }

    private static RpcAuthResponse Accepted(string principal = "alice", string tenant = "one")
        => new(true, principal, new Dictionary<string, string> { ["aster.role"] = "admin", ["tenant"] = tenant });

    private static HttpClient Client() => new(new HttpClientHandler { ServerCertificateCustomValidationCallback = HttpClientHandler.DangerousAcceptAnyServerCertificateValidator }) { Timeout = Budget };

    private static async Task<(List<EchoReply> Replies, RpcStatus Status)> Call(Fixture f, HttpClient client, OwnedListener listener,
        string method = "echo", string? token = "Bearer alice", int sessionId = 0, params EchoRequest[] requests)
    {
        if (requests.Length == 0) requests = [new EchoRequest { Message = "hello", Count = 41, Data = [1, 2, 255] }];
        using var bytes = new MemoryStream();
        for (int i = 0; i < requests.Length; i++)
            bytes.Write(AsterFraming.EncodeFrame(f.Codec.Encode(requests[i]), i == requests.Length - 1 ? AsterFraming.FlagEndStream : (byte)0));
        using var request = new HttpRequestMessage(HttpMethod.Post, $"https://{listener.TcpAddress}/aster/HTTPTest/{method}") { Content = new ByteArrayContent(bytes.ToArray()) };
        request.Content.Headers.ContentType = new MediaTypeHeaderValue("application/aster-frames");
        if (token is not null) request.Headers.TryAddWithoutValidation("Authorization", token);
        if (sessionId != 0) request.Headers.TryAddWithoutValidation("aster-session-id", sessionId.ToString(System.Globalization.CultureInfo.InvariantCulture));
        using var response = await client.SendAsync(request);
        response.EnsureSuccessStatusCode();
        byte[] body = await response.Content.ReadAsByteArrayAsync();
        var replies = new List<EchoReply>();
        int pos = 0;
        while (pos < body.Length)
        {
            Assert.True(body.Length - pos >= 5, "truncated response frame");
            int size = checked((int)BinaryPrimitives.ReadUInt32LittleEndian(body.AsSpan(pos)));
            Assert.InRange(size, 1, body.Length - pos - 4);
            byte flags = body[pos + 4];
            byte[] payload = body.AsSpan(pos + 5, size - 1).ToArray();
            pos += size + 4;
            if ((flags & AsterFraming.FlagTrailer) != 0)
            {
                Assert.Equal(body.Length, pos);
                return (replies, AsterEnvelope.DecodeRpcStatus(payload));
            }
            replies.Add(f.Codec.Decode<EchoReply>(AsterFraming.PayloadOf(payload, flags)));
        }
        throw new InvalidDataException("missing RPC trailer");
    }

    private static async Task AssertStopped(Task<(List<EchoReply> Replies, RpcStatus Status)> call)
    {
        try { Assert.NotEqual(StatusCode.Ok, (await call.WaitAsync(Budget)).Status.Code); }
        catch (HttpRequestException) { }
        catch (IOException) { }
        catch (InvalidDataException) { } // Forced transport shutdown can omit the RPC trailer.
    }

    private static void Released(string address, bool quic)
    {
        var endpoint = IPEndPoint.Parse(address);
        using var tcp = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
        tcp.Bind(endpoint); tcp.Listen();
        if (quic) { using var udp = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp); udp.Bind(endpoint); }
    }

    [Fact]
    public void NativeLayoutMatchesAbi411()
    {
        Assert.Equal(7u, Native.RequiredAbiMinor);
        Assert.Equal(64, Marshal.SizeOf<HttpServerConfigNative>());
        foreach (var (field, offset) in new[] { ("struct_size", 0), ("flags", 4), ("bind_addr", 8), ("cert_pem", 24), ("key_pem", 40), ("max_pending_auth", 56), ("max_auth_payload_bytes", 60) })
            Assert.Equal(offset, Marshal.OffsetOf<HttpServerConfigNative>(field).ToInt32());
        Assert.Equal(1u, Native.HttpServerWebTransport);
    }

    [Fact]
    public async Task AllPatternsAndPerCallPrincipalsUseAuthenticatedHttps()
    {
        await using var f = await Fixture.Create();
        f.Authenticate();
        f.Upcalls.SetPolicy(UpcallKind.RpcAuthenticate, TimeSpan.FromSeconds(2), HookFailureMode.FailOpen);
        // Enrollment must not override foreign authentication's per-call attributes.
        f.Server.SetPeerAttributes("alice", new Dictionary<string, string> { ["aster.role"] = "wrong", ["tenant"] = "enrollment" });
        await using var listener = await f.Server.StartHttpListenerAsync(Pem(true), f.Upcalls);
        Assert.Equal(listener.TcpAddress, listener.QuicAddress);
        using (var udp = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp))
            Assert.Throws<SocketException>(() => udp.Bind(IPEndPoint.Parse(listener.QuicAddress!)));
        using var client = Client();
        foreach (var (token, code) in new[] { ((string?)null, StatusCode.Unauthenticated), ("Bearer low", StatusCode.PermissionDenied), ("Bearer backend", StatusCode.Unavailable) })
        {
            var (_, status) = await Call(f, client, listener, token: token);
            Assert.Equal(code, status.Code);
            Assert.DoesNotContain("secret", status.Message);
            Assert.Empty(f.Seen);
        }
        foreach (var principal in new[] { "alice", "bob", "alice" })
        {
            var (replies, status) = await Call(f, client, listener, token: "Bearer " + principal);
            Assert.Equal(StatusCode.Ok, status.Code);
            var reply = Assert.Single(replies);
            Assert.Equal("hello", reply.Message); Assert.Equal(42, reply.Count); Assert.Equal(new byte[] { 1, 2, 255 }, reply.Data);
            Assert.Equal(principal, reply.ServerLang);
            Assert.True(f.Seen.TryDequeue(out var context));
            Assert.Equal(principal, context!.PeerId); Assert.False(context.IrohAuthenticated);
            // ABI 7.5: the authenticator's principal is apart from the transport peer (the HTTP caller's address).
            Assert.Equal(principal, context.Principal); Assert.True(context.HasPrincipal);
            Assert.NotEqual(principal, context.Peer); Assert.False(string.IsNullOrEmpty(context.Peer));
            Assert.Equal(principal == "alice" ? "one" : "two", context.Attributes["tenant"]);
        }
        var generated = await Call(f, client, listener, "generate", requests: [new EchoRequest { Count = 3, Message = "stream" }]);
        Assert.Equal(StatusCode.Ok, generated.Status.Code); Assert.Equal(new[] { 0, 1, 2 }, generated.Replies.Select(r => r.Count));
        var ingested = await Call(f, client, listener, "ingest", requests: [new EchoRequest { Count = 4 }, new EchoRequest { Count = 5 }]);
        Assert.Equal(StatusCode.Ok, ingested.Status.Code); Assert.Equal(9, Assert.Single(ingested.Replies).Count);
        var chat = await Call(f, client, listener, "chat", "Bearer bob", requests: [new EchoRequest { Message = "first" }, new EchoRequest { Message = "second" }]);
        Assert.Equal(StatusCode.Ok, chat.Status.Code); Assert.Equal(new[] { "FIRST", "SECOND" }, chat.Replies.Select(r => r.Message));
        Assert.All(chat.Replies, r => Assert.Equal("bob", r.ServerLang));
        await listener.StopAsync(TimeSpan.FromMilliseconds(20));
        await listener.DisposeAsync();
        Released(listener.TcpAddress, true);
    }

    [Fact]
    public async Task StartupValidationAndPartialBindCleanup()
    {
        await using var f = await Fixture.Create();
        var options = Pem(true);
        await Assert.ThrowsAsync<InvalidOperationException>(() => f.Server.StartHttpListenerAsync(options, f.Upcalls));
        f.Authenticate();
        using var otherRuntime = new Runtime();
        await using var otherUpcalls = Upcalls.Create(otherRuntime);
        otherUpcalls.ServeRpcAuthenticator((_, _) => ValueTask.FromResult(Accepted()));
        await Assert.ThrowsAsync<ArgumentException>(() => f.Server.StartHttpListenerAsync(options, otherUpcalls));
        await Assert.ThrowsAsync<IrohException>(() => f.Server.StartHttpListenerAsync(options with { PrivateKeyPem = "invalid"u8.ToArray() }, f.Upcalls));
        using var canceled = new CancellationTokenSource(); canceled.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => f.Server.StartHttpListenerAsync(options, f.Upcalls, canceled.Token));
        using var udp = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        udp.Bind(new IPEndPoint(IPAddress.Loopback, 0));
        string address = udp.LocalEndPoint!.ToString()!;
        await Assert.ThrowsAsync<IrohException>(() => f.Server.StartHttpListenerAsync(options with { BindAddress = address }, f.Upcalls));
        Released(address, false);
    }

    [Fact]
    public async Task StopCancelsPendingAuthenticationAndEnforcesPendingBound()
    {
        await using var f = await Fixture.Create();
        var entered = Signal(); var canceled = Signal();
        f.Upcalls.ServeRpcAuthenticator(async (_, token) =>
        {
            entered.TrySetResult();
            try { await Task.Delay(Timeout.InfiniteTimeSpan, token); }
            finally { canceled.TrySetResult(); }
            return RpcAuthResponse.Rejected();
        });
        await using var listener = await f.Server.StartHttpListenerAsync(Pem() with { MaxPendingAuth = 1 }, f.Upcalls);
        using var client = Client();
        var first = Call(f, client, listener);
        await entered.Task.WaitAsync(Budget);
        var overflow = await Call(f, client, listener);
        Assert.Equal(StatusCode.ResourceExhausted, overflow.Status.Code);
        await listener.DisposeAsync();
        await canceled.Task.WaitAsync(Budget);
        await AssertStopped(first);
        Assert.Empty(f.Seen);
        Released(listener.TcpAddress, false);
    }

    [Fact]
    public async Task PayloadBoundRejectsBeforeCallingAuthenticator()
    {
        await using var f = await Fixture.Create();
        int called = 0;
        f.Upcalls.ServeRpcAuthenticator((_, _) => { Interlocked.Increment(ref called); return ValueTask.FromResult(Accepted()); });
        await using var listener = await f.Server.StartHttpListenerAsync(Pem() with { MaxAuthPayloadBytes = 1 }, f.Upcalls);
        Assert.Null(listener.QuicAddress);
        using var client = Client();
        var (_, status) = await Call(f, client, listener);
        Assert.Equal(StatusCode.ResourceExhausted, status.Code); Assert.Equal(0, called); Assert.Empty(f.Seen);
    }

    [Fact]
    public async Task GraceThenCancellationEndsManagedHandlerAndNativeOwner()
    {
        await using var f = await Fixture.Create(); f.Authenticate();
        await using var listener = await f.Server.StartHttpListenerAsync(Pem(), f.Upcalls);
        using var client = Client();
        var call = Call(f, client, listener, "hold");
        await f.HandlerEntered.Task.WaitAsync(Budget);
        var elapsed = Stopwatch.StartNew();
        await listener.StopAsync(TimeSpan.FromMilliseconds(50));
        Assert.True(elapsed.Elapsed >= TimeSpan.FromMilliseconds(35));
        await f.HandlerCanceled.Task.WaitAsync(Budget);
        await AssertStopped(call);
        Assert.Equal(0UL, f.Server.LiveCalls);
        Released(listener.TcpAddress, false);
    }

    [Fact]
    public async Task ServerDisposalStopsChildAndConcurrentStopIsIdempotent()
    {
        await using var f = await Fixture.Create(); f.Authenticate();
        await using var listener = await f.Server.StartHttpListenerAsync(Pem(true), f.Upcalls);
        await f.Server.DisposeAsync();
        Released(listener.TcpAddress, true);
        await Task.WhenAll(listener.StopAsync(TimeSpan.Zero), listener.DisposeAsync().AsTask());
    }

    [Fact]
    public async Task CanceledStopPreservesAUsableOwner()
    {
        await using var f = await Fixture.Create(); f.Authenticate();
        await using var listener = await f.Server.StartHttpListenerAsync(Pem(), f.Upcalls);
        using var cancellation = new CancellationTokenSource(); cancellation.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => listener.StopAsync(TimeSpan.Zero, cancellation.Token));
        using var client = Client();
        Assert.Equal(StatusCode.Ok, (await Call(f, client, listener)).Status.Code);
        await listener.DisposeAsync();
        Released(listener.TcpAddress, false);
    }

    [Fact]
    public async Task ExplicitPublicModeServesFourPatternsAndPreservesGate3()
    {
        await using var f = await Fixture.Create(publicService: true);
        var generated = TlsCertificates.GenerateWebTransportCertificate("http-test", ["127.0.0.1"]);
        var options = new HttpListenerOptions { CertificatePem = generated.CertificatePem, PrivateKeyPem = generated.PrivateKeyPem, WebTransport = true };
        await Assert.ThrowsAsync<InvalidOperationException>(() => f.Server.StartHttpListenerAsync(options, f.Upcalls));
        int authenticationCalls = 0;
        f.Upcalls.ServeRpcAuthenticator((_, _) => { Interlocked.Increment(ref authenticationCalls); return ValueTask.FromResult(Accepted()); });
        await using var listener = await f.Server.StartPublicHttpListenerAsync(options);
        using var client = Client();
        Assert.Equal(StatusCode.PermissionDenied, (await Call(f, client, listener, "restricted")).Status.Code);
        Assert.Empty(f.Seen);
        var unary = await Call(f, client, listener, token: null);
        Assert.Equal(StatusCode.Ok, unary.Status.Code); Assert.Equal(42, Assert.Single(unary.Replies).Count);
        var generatedReplies = await Call(f, client, listener, "generate", token: null, requests: [new EchoRequest { Count = 3 }]);
        Assert.Equal(StatusCode.Ok, generatedReplies.Status.Code); Assert.Equal(new[] { 0, 1, 2 }, generatedReplies.Replies.Select(r => r.Count));
        var ingested = await Call(f, client, listener, "ingest", token: null, requests: [new EchoRequest { Count = 4 }, new EchoRequest { Count = 5 }]);
        Assert.Equal(StatusCode.Ok, ingested.Status.Code); Assert.Equal(9, Assert.Single(ingested.Replies).Count);
        var chat = await Call(f, client, listener, "chat", token: null, requests: [new EchoRequest { Message = "first" }, new EchoRequest { Message = "second" }]);
        Assert.Equal(StatusCode.Ok, chat.Status.Code); Assert.Equal(new[] { "FIRST", "SECOND" }, chat.Replies.Select(r => r.Message));
        Assert.All(f.Seen, ctx => { Assert.False(ctx.IrohAuthenticated); Assert.Empty(ctx.Attributes); Assert.Equal(0UL, ctx.ConnectionId); });
        Assert.Equal(0, authenticationCalls);
        await listener.DisposeAsync();
        Released(listener.TcpAddress, true);
    }

    [Fact]
    public async Task ExplicitPublicModeRejectsSessionFactoriesAndAuthenticationLimits()
    {
        await using var f = await Fixture.Create(publicService: true, sessionService: true);
        var options = Pem();
        await Assert.ThrowsAsync<ArgumentException>(() => f.Server.StartPublicHttpListenerAsync(options with { MaxPendingAuth = 1 }));
        await Assert.ThrowsAsync<ArgumentException>(() => f.Server.StartPublicHttpListenerAsync(options with { MaxAuthPayloadBytes = 1 }));
        using var canceled = new CancellationTokenSource(); canceled.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => f.Server.StartPublicHttpListenerAsync(options, canceled.Token));
        await using var listener = await f.Server.StartPublicHttpListenerAsync(options);
        using var client = Client();
        Assert.Equal(StatusCode.Unauthenticated, (await Call(f, client, listener, sessionId: 42)).Status.Code);
        Assert.Equal(0, f.SessionFactories); Assert.Empty(f.Seen);
        await f.Server.DisposeAsync();
        Released(listener.TcpAddress, false);
    }
}
