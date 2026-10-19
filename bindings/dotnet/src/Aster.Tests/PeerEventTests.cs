using System;
using System.Threading;
using System.Threading.Tasks;
using Aster.Rpc;

namespace Aster.Tests;

/// <summary>Server peer events (C ABI 7.8): a thin mapping test over <c>aster_server_peer_events</c> / <c>_poll</c> / <c>_release</c>; semantics are tested in Rust.</summary>
public class PeerEventTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    private static async Task<T> Next<T>(IAsyncEnumerator<PeerEvent> events) where T : PeerEvent
    {
        Assert.True(await events.MoveNextAsync().AsTask().WaitAsync(WaitBudget.Value), "the stream ended before the event");
        return Assert.IsType<T>(events.Current);
    }

    [Fact]
    public async Task Client_Connect_Then_Close_Reports_Connected_Then_Disconnected_Closed()
    {
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = Loopback });
        await using var stream = await server.PeerEventsAsync();
        Assert.Empty(stream.Snapshot);
        await using var events = stream.GetAsyncEnumerator();

        var client = await AsterClient.StartAsync(Loopback);
        var clientId = client.Node.NodeId();
        await using var conn = await client.ConnectAsync(await server.TicketAsync());

        var connected = await Next<PeerConnected>(events);
        Assert.Equal(clientId, connected.Peer);
        Assert.Equal(clientId, connected.Identity.EndpointId);
        Assert.Null(connected.Identity.Principal);
        Assert.Empty(connected.Identity.Roles);

        // A stream opened now sees the connected peer in its snapshot.
        await using (var later = await server.PeerEventsAsync())
        {
            var peer = Assert.Single(later.Snapshot);
            Assert.Equal(clientId, peer.Peer);
            Assert.Equal(1, peer.Connections);
        }

        await client.DisposeAsync();
        var disconnected = await Next<PeerDisconnected>(events);
        Assert.Equal(clientId, disconnected.Peer);
        Assert.Equal(DisconnectReason.Closed, disconnected.Reason);
    }

    [Fact]
    public async Task Disposing_The_Stream_Ends_The_Enumeration_And_A_Cancelled_Token_Releases_It()
    {
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = Loopback });

        var stream = await server.PeerEventsAsync();
        var pending = Task.Run(async () =>
        {
            await foreach (var _ in stream) { }
        });
        await stream.DisposeAsync();
        await pending.WaitAsync(WaitBudget.Value);

        using var lifetime = new CancellationTokenSource();
        await using var bound = await server.PeerEventsAsync(lifetime.Token);
        var ended = Task.Run(async () =>
        {
            await foreach (var _ in bound) { }
        });
        await lifetime.CancelAsync();
        await ended.WaitAsync(WaitBudget.Value);
    }
}
