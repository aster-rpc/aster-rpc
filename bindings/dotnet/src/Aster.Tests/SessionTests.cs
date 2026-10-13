using System;
using System.Collections.Concurrent;
using System.Threading;
using System.Threading.Tasks;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster.Tests;

/// <summary>Session-scoped services: Rust routes sessions per connection; .NET keeps the per-session state.</summary>
public class SessionTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    public sealed class SessionLog
    {
        public readonly ConcurrentDictionary<int, CounterState> Created = new();
        public int Disposed;
        public readonly TaskCompletionSource<bool> AllDisposed = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public int Expected = int.MaxValue;
    }

    public sealed class CounterState(string peer, SessionLog log) : IDisposable
    {
        public string Peer { get; } = peer;
        public int Total;

        public void Dispose()
        {
            if (Interlocked.Increment(ref log.Disposed) >= log.Expected)
                log.AllDisposed.TrySetResult(true);
        }
    }

    public static ServiceDefinition Counter(SessionLog log) => ServiceDefinition.Create("Counter", 1, ServiceScope.Session)
        .SessionFactory(peer => new CounterState(peer, log))
        .Unary<EchoRequest, EchoReply>("add", (req, ctx) =>
        {
            var state = (CounterState)ctx.Session!;
            log.Created.TryAdd(ctx.SessionId, state);
            return ValueTask.FromResult(new EchoReply
            {
                Count = Interlocked.Add(ref state.Total, req.Count),
                Message = ctx.SessionKey.ToString(System.Globalization.CultureInfo.InvariantCulture),
                ServerLang = state.Peer,
            });
        })
        .Build();

    private static async Task<(AsterServer, AsterClient, RpcConnection)> Start(SessionLog log, int maxSessions = 1024)
    {
        var server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = Loopback,
            Services = { Counter(log) },
            MaxSessionsPerConnection = maxSessions,
        });
        var client = await AsterClient.StartAsync(Loopback);
        client.RegisterTypes(Counter(log));
        var conn = await client.ConnectAsync(await server.TicketAsync());
        return (server, client, conn);
    }

    private static Task<EchoReply> Add(RpcConnection conn, int session, int n)
        => conn.UnaryAsync<EchoRequest, EchoReply>("Counter", "add", new EchoRequest { Count = n }, new CallOptions { SessionId = session });

    [Fact]
    public async Task Sessions_Keep_Separate_State_Per_Session_Id()
    {
        var log = new SessionLog();
        var (server, client, conn) = await Start(log);
        await using var _s = server;
        await using var _c = client;

        int a = conn.NextSessionId(), b = conn.NextSessionId();
        Assert.NotEqual(0, a);
        Assert.NotEqual(a, b);
        Assert.Equal(1, (await Add(conn, a, 1)).Count);
        Assert.Equal(10, (await Add(conn, b, 10)).Count);
        var again = await Add(conn, a, 2);
        Assert.Equal(3, again.Count);
        Assert.Equal(client.Node.NodeId(), again.ServerLang);
        Assert.Equal(12, (await Add(conn, b, 2)).Count);
        Assert.NotEqual((await Add(conn, a, 0)).Message, (await Add(conn, b, 0)).Message);
        Assert.Equal(2, server.ActiveSessions);

        // A session-scoped service needs a session id.
        var ex = await Assert.ThrowsAsync<RpcError>(() => Add(conn, 0, 1));
        Assert.NotEqual(StatusCode.Ok, ex.Code);
    }

    [Fact]
    public async Task Connection_Close_Reaps_Sessions_And_Disposes_State()
    {
        var log = new SessionLog { Expected = 2 };
        var (server, client, conn) = await Start(log);
        await using var _s = server;
        var closedSessions = new ConcurrentBag<ulong>();
        var connectionClosed = new TaskCompletionSource<ulong>(TaskCreationOptions.RunContinuationsAsynchronously);
        server.SessionClosed += key => closedSessions.Add(key);
        server.ConnectionClosed += id => connectionClosed.TrySetResult(id);

        await Add(conn, conn.NextSessionId(), 1);
        await Add(conn, conn.NextSessionId(), 1);
        Assert.Equal(2, server.ActiveSessions);

        await client.DisposeAsync();
        Assert.True(await log.AllDisposed.Task.WaitAsync(TimeSpan.FromSeconds(20)));
        await connectionClosed.Task.WaitAsync(TimeSpan.FromSeconds(20));
        Assert.Equal(2, closedSessions.Count);
        Assert.Equal(0, server.ActiveSessions);
    }

    [Fact]
    public async Task Session_Cap_Is_Enforced_By_Rust()
    {
        var log = new SessionLog();
        var (server, client, conn) = await Start(log, maxSessions: 2);
        await using var _s = server;
        await using var _c = client;

        await Add(conn, conn.NextSessionId(), 1);
        await Add(conn, conn.NextSessionId(), 1);
        var ex = await Assert.ThrowsAsync<RpcError>(() => Add(conn, conn.NextSessionId(), 1));
        Assert.Equal(StatusCode.ResourceExhausted, ex.Code);
    }
}
