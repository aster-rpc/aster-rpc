using System.Buffers.Binary;
using System.Net.Http.Headers;
using System.Text.Json;
using Aster.Codec;
using Aster.Interceptors;
using Aster.Rpc;
using Aster.Upcall;

namespace Aster.Tests;

public class TopologyTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
    private static async Task Until(Func<Task<bool>> check, CancellationToken ct)
    {
        while (!await check()) await Task.Delay(50, ct);
    }

    [Fact]
    public async Task NoMonitoringAndPrejoin()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(15)); var ct = timeout.Token;
        await using var node = await Node.StartAsync(Loopback, cancellationToken: ct);
        Assert.False(node.Topology.HasMonitoring); Assert.Empty(node.Topology.Peers());
        Assert.Null(node.Topology.Peer(new string('0', 64)));
        Assert.Equal(Status.InvalidArgument, Assert.Throws<IrohException>(() => node.Topology.Peer("bad")).Status);
        await Assert.ThrowsAsync<IrohException>(() => node.Topology.SnapshotAsync(ct));
        await Assert.ThrowsAsync<IrohException>(() => node.Topology.EnableSharedAsync(new byte[32], ct: ct));
    }

    [Fact]
    public async Task MeasuredPeersSharedSnapshotAndOwnership()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(40)); var ct = timeout.Token;
        var config = Loopback with { EnableMonitoring = true };
        await using var a = await Node.StartAsync(config, extraAlpns: ["pc16/probe"], cancellationToken: ct);
        await using var b = await Node.StartAsync(config, extraAlpns: ["pc16/probe"], cancellationToken: ct);
        await using var c = await Node.StartAsync(config, extraAlpns: ["pc16/probe"], cancellationToken: ct);
        Node[] nodes = [a, b, c]; string[] ids = nodes.Select(n => n.NodeId()).ToArray();
        foreach (var node in nodes) foreach (var other in nodes) if (node != other) node.AddNodeAddr(other.NodeAddr());
        var accept = b.AcceptAsync(ct);
        using var outbound = await a.ConnectAsync(await b.TicketAsync(TimeSpan.FromSeconds(3), ct), "pc16/probe", ct);
        using var inbound = await accept;
        await Until(() => Task.FromResult(a.Topology.Peer(ids[1]) is { Samples: > 0 }), ct);
        var peer = a.Topology.Peer(ids[1])!;
        Assert.Equal(TopologyLadderLevel.SameHost, peer.Level); Assert.True(peer.Direct); Assert.NotNull(peer.RemoteAddr);
        Assert.True(peer.LastMeasuredUnixMilliseconds > 0); Assert.Contains(a.Topology.Peers(), p => p.NodeId == ids[1]);
        using var up = Upcalls.Create(a.Runtime, 8);
        using var filter = AdmittedFilterHandle.Create(up, ttl: TimeSpan.FromHours(1));
        foreach (string id in ids) filter.Set(id, true);
        var options = new TopologySwarmOptions { PositionRefreshMilliseconds = 100, MinHoldMilliseconds = 0, LivenessTtlMilliseconds = 60_000 };
        byte[] secret = Enumerable.Repeat((byte)37, 32).ToArray();
        for (int i = 0; i < nodes.Length; i++) await nodes[i].Topology.EnableSharedAsync(secret, i == 0 ? filter : null, ids, options, ct);
        await Until(async () => { using var s = await a.Topology.SnapshotAsync(ct); return s.Clusters() is { Count: 1 } clusters && clusters[0].Members.Count == 3; }, ct);
        Assert.Single(await a.Topology.ClustersAsync(ct));
        Assert.Equal(3, (await a.Topology.MyClusterAsync(ct))!.Members.Count);
        Assert.Equal(TopologySeparation.Connected, await a.Topology.SeparatedAsync(ids[0], ids[1], ct));
        using var captured = await a.Topology.SnapshotAsync(ct);
        Assert.Equal(3, captured.ClusterOf(ids[1])!.Members.Count);
        Assert.Equal(TopologySeparation.Connected, captured.Separated(ids[0], ids[1]));
        Assert.Equal(TopologySeparation.Unknown, captured.Separated(ids[0], new string('0', 64)));
        Assert.Equal(Status.InvalidArgument, Assert.Throws<IrohException>(() => captured.Separated("bad", ids[0])).Status);
        filter.Set(ids[2], false);
        await Until(async () => { using var s = await a.Topology.SnapshotAsync(ct); return s.ClusterOf(ids[2]) is null; }, ct);
        Assert.Equal(3, captured.ClusterOf(ids[2])!.Members.Count);
        var detached = captured.Clusters(); ((IList<string>)detached[0].Members)[0] = "mutated host copy";
        Assert.DoesNotContain("mutated host copy", captured.Clusters()[0].Members);
        using var preCanceled = new CancellationTokenSource(); preCanceled.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => a.Topology.SnapshotAsync(preCanceled.Token));
        for (int i = 0; i < 8; i++)
        {
            using var racing = CancellationTokenSource.CreateLinkedTokenSource(ct);
            var pending = a.Topology.SnapshotAsync(racing.Token); racing.Cancel();
            try { using var observation = await pending; Assert.NotNull(observation.Clusters()); }
            catch (OperationCanceledException) { }
        }
        while (a.Runtime.PendingOperationCount != 0) await Task.Delay(1, ct);
        outbound.Dispose(); inbound.Dispose();
        await a.DisposeAsync();
        Assert.Single(captured.Clusters());
        captured.Dispose();
        Assert.Throws<ObjectDisposedException>(() => captured.Clusters());
    }

    [Fact]
    public void PeerJsonPreservesFullUnsigned64AndNull()
    {
        const string json = """{"node_id":"id","level":4,"rtt_us":18446744073709551615,"jitter_us":4294967297,"throughput_bps":18446744073709551615,"samples":4294967298,"remote_addr":null}""";
        var peer = JsonSerializer.Deserialize<TopologyPeerView>(json, TopologyView.JsonOptions)!;
        Assert.Equal(ulong.MaxValue, peer.RttMicroseconds); Assert.Equal(4294967297UL, peer.JitterMicroseconds);
        Assert.Equal(ulong.MaxValue, peer.ThroughputBps); Assert.Equal(4294967298UL, peer.Samples); Assert.Null(peer.RemoteAddr);
        foreach (string bad in new[] { """{"rtt_us":18446744073709551616}""", """{"rtt_us":-1}""", """{"rtt_us":"1"}""", "{} {}" })
            Assert.Throws<JsonException>(() => JsonSerializer.Deserialize<TopologyPeerView>(bad, TopologyView.JsonOptions));
        Assert.Equal(120u, (uint)EventKind.TopologySnapshot); Assert.Equal(9u, Native.RequiredAbiMinor);
    }

    [Fact]
    public async Task RpcOptInMonitoringAndNativeRequirement()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(20)); var ct = timeout.Token;
        await Assert.ThrowsAsync<IrohException>(() => AsterServer.StartAsync(new AsterServerOptions { Config = Loopback, Topology = true }, ct));
        foreach (bool enabled in new[] { false, true })
        {
            await using var server = await AsterServer.StartAsync(new AsterServerOptions
            { Config = Loopback with { EnableMonitoring = true }, Topology = enabled, TopologyRequirement = CapabilityRequirement.Role("observer") }, ct);
            var cert = TlsCertificates.GenerateSelfSignedCertificate();
            await using var listener = await server.StartPublicHttpListenerAsync(new HttpListenerOptions { CertificatePem = cert.CertificatePem, PrivateKeyPem = cert.PrivateKeyPem }, ct);
            using var client = new HttpClient(new HttpClientHandler { ServerCertificateCustomValidationCallback = HttpClientHandler.DangerousAcceptAnyServerCertificateValidator });
            using var request = new HttpRequestMessage(HttpMethod.Post, $"https://{listener.TcpAddress}/aster/aster.net.Topology/peers")
            { Content = new ByteArrayContent(AsterFraming.EncodeFrame([], AsterFraming.FlagEndStream)) };
            request.Content.Headers.ContentType = new MediaTypeHeaderValue("application/aster-frames");
            using var response = await client.SendAsync(request, ct); response.EnsureSuccessStatusCode();
            byte[] body = await response.Content.ReadAsByteArrayAsync(ct); RpcStatus? status = null;
            for (int pos = 0; pos < body.Length;)
            {
                int size = checked((int)BinaryPrimitives.ReadUInt32LittleEndian(body.AsSpan(pos)));
                byte flags = body[pos + 4]; byte[] payload = body.AsSpan(pos + 5, size - 1).ToArray(); pos += size + 4;
                if ((flags & AsterFraming.FlagTrailer) != 0) status = AsterEnvelope.DecodeRpcStatus(payload);
            }
            Assert.NotNull(status); Assert.Equal(enabled ? StatusCode.PermissionDenied : StatusCode.NotFound, status!.Code);
        }
    }
}
