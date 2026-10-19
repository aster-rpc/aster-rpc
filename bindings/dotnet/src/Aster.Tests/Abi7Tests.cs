using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster.Tests;

/// <summary>C ABI 7.1 – 7.7 (ffi/RENAME_MAP.md "New in 7.x"): layouts, config, metrics, drain, call identity (gateway identity: GatewayIdentityTests).</summary>
public class Abi7Tests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
    private static readonly TimeSpan Budget = TimeSpan.FromSeconds(30);

    [Fact]
    public void Layouts_Constants_And_Exports_Match_The_Header()
    {
        Assert.Equal(7u, Native.RequiredAbiMinor);
        Assert.Equal(64, Marshal.SizeOf<DrainReportNative>());
        Assert.Equal(96, Marshal.SizeOf<NetworkConfigNative>());
        Assert.Equal(16, Marshal.SizeOf<MetricsConfigNative>());
        Assert.Equal(168, Marshal.SizeOf<BlobTransferStatsNative>());
        Assert.Equal(64, Marshal.SizeOf<DispatchIdentityNative>());
        Assert.Equal(24, Marshal.OffsetOf<NetworkConfigNative>(nameof(NetworkConfigNative.advertise_addrs)).ToInt32());
        Assert.Equal(56, Marshal.OffsetOf<NetworkConfigNative>(nameof(NetworkConfigNative.relay_ca_only)).ToInt32());
        Assert.Equal(56, Marshal.OffsetOf<DispatchIdentityNative>(nameof(DispatchIdentityNative.buffer)).ToInt32());
        Assert.Equal(128u, (uint)EventKind.AuditOutboxAcknowledgements);
        Assert.Equal(0x1000u, (uint)DispatchFlags.HasPrincipal);
        Assert.Equal(503UL, AsterServer.DrainCloseCode);
        Assert.Equal(2u, Native.FetchReportV2);
        Assert.Equal(new uint[] { 0, 1, 2, 3 }, Enum.GetValues<DiscoveryMode>().Select(m => (uint)m).ToArray());
        // 7.6 / 7.7 gateway identity
        Assert.Equal(96, Marshal.SizeOf<IdentityConfigNative>());
        Assert.Equal(16, Marshal.OffsetOf<IdentityConfigNative>(nameof(IdentityConfigNative.edge_token_max_lifetime_secs)).ToInt32());
        Assert.Equal(40, Marshal.OffsetOf<IdentityConfigNative>(nameof(IdentityConfigNative.trusted_issuers_json)).ToInt32());
        Assert.Equal(56, Marshal.OffsetOf<IdentityConfigNative>(nameof(IdentityConfigNative.publisher_json)).ToInt32());
        Assert.Equal(72, Marshal.OffsetOf<IdentityConfigNative>(nameof(IdentityConfigNative.remote_config_lock)).ToInt32());
        Assert.Equal(88, Marshal.OffsetOf<IdentityConfigNative>(nameof(IdentityConfigNative.sources)).ToInt32());
        Assert.Equal(0x2000u, (uint)DispatchFlags.OnBehalfOf);
        Assert.Equal(6u, (uint)DispatchServerOption.AcceptOnBehalfOf);
        Assert.Equal(129u, (uint)EventKind.NodeConfigResult);
        Assert.Equal(2u, Native.DispatchIdentityActor);
        Assert.Equal((1u, 2u, 4u), (Native.IdentitySetEdgeTokenMaxLifetime, Native.IdentitySetClockLeeway, Native.IdentitySetMaxKeySetAge));
        Assert.Equal((0u, 1u, 2u, 3u), (Native.NodeConfigGet, Native.NodeConfigSet, Native.NodeConfigClear, Native.NodeConfigAck));
        AbiContractTests.AssertExports([
            "aster_server_drain", "aster_config_set_network", "aster_config_network", "aster_config_set_metrics",
            "aster_config_metrics", "aster_blobs_transfer_stats", "aster_dispatch_identity",
            "aster_config_set_identity", "aster_config_identity", "aster_server_set_audit_outbox", "aster_dispatch_on_behalf_of",
            "aster_server_node_properties", "aster_server_node_config_generation", "aster_enrollment_mint_user_token",
            "aster_enrollment_publisher_notice", "aster_enrollment_publisher_notice_token", "aster_enrollment_publisher_status",
            "aster_node_config_call",
        ]);
        Assert.True(NativeLibrary.TryLoad("aster_transport_ffi", typeof(Native).Assembly, null, out IntPtr lib));
        Assert.True(NativeLibrary.TryGetExport(lib, "aster_audit_outbox_acknowledgements", out _));
    }

    // ─── 7.3 / 7.4 config ───────────────────────────────────────────────────

    [Fact]
    public void Network_And_Metrics_Settings_Round_Trip_Through_The_Native_Config()
    {
        var d = AsterConfig.Default();
        Assert.Equal(DiscoveryMode.Default, d.Discovery);
        Assert.Null(d.BindAddrV6);
        Assert.Empty(d.AdvertiseAddrs);
        Assert.False(d.MetricsPeerLabels);
        Assert.Equal(64u, d.MetricsPeerLabelCap);

        var c = new AsterConfig
        {
            BindAddrV6 = "[::]:7842",
            AdvertiseAddrs = ["203.0.113.10:31842", "198.51.100.7:31842"],
            RelayCaCerts = ["/etc/aster/relay-ca.pem"],
            RelayCaOnly = true,
            Discovery = DiscoveryMode.None,
            MetricsPeerLabels = true,
            MetricsPeerLabelCap = 3,
        };
        AsterConfig back;
        using (var scope = new NativeScope())
        {
            var n = c.ToNative(scope);
            Assert.NotEqual(IntPtr.Zero, n.storage);
            back = AsterConfig.FromNative(in n, new AsterConfig());
        }
        Assert.Equal("[::]:7842", back.BindAddrV6);
        Assert.Equal(c.AdvertiseAddrs, back.AdvertiseAddrs);
        Assert.Equal(c.RelayCaCerts, back.RelayCaCerts);
        Assert.True(back.RelayCaOnly);
        Assert.Equal(DiscoveryMode.None, back.Discovery);
        Assert.True(back.MetricsPeerLabels);
        Assert.Equal(3u, back.MetricsPeerLabelCap);

        // Default discovery with custom servers reads back as Custom (Rust's rule).
        using (var scope = new NativeScope())
        {
            var n = new AsterConfig { DiscoveryPkarrRelays = ["https://dns.example.internal/pkarr"] }.ToNative(scope);
            back = AsterConfig.FromNative(in n, new AsterConfig());
        }
        Assert.Equal(DiscoveryMode.Custom, back.Discovery);
        Assert.Single(back.DiscoveryPkarrRelays);

        // Nothing set: no library storage is allocated.
        using (var scope = new NativeScope())
            Assert.Equal(IntPtr.Zero, Loopback.ToNative(scope).storage);

        using (var scope = new NativeScope())
        {
            var bad = new AsterConfig { Discovery = DiscoveryMode.None, DiscoveryDnsOrigin = "dns.example.internal" };
            Assert.Throws<IrohException>(() => bad.ToNative(scope));
        }
        using (var scope = new NativeScope())
            Assert.Throws<ArgumentException>(() => new AsterConfig { Discovery = (DiscoveryMode)9 }.ToNative(scope));
    }

    [Fact]
    public void Network_And_Metrics_Settings_Load_From_Env_And_Toml()
    {
        string[] keys = ["ASTER_BIND_ADDR_V6", "ASTER_ADVERTISE_ADDRS", "ASTER_RELAY_CA_CERTS", "ASTER_RELAY_CA_ONLY", "ASTER_DISCOVERY",
            "ASTER_METRICS_PEER_LABELS", "ASTER_METRICS_PEER_LABEL_CAP"];
        lock (EnvLock)
        {
            // Rust reads the native environment; on Unix Environment.SetEnvironmentVariable does not reach getenv.
            if (!OperatingSystem.IsWindows())
            try
            {
                Env("ASTER_BIND_ADDR_V6", "[::]:7842");
                Env("ASTER_ADVERTISE_ADDRS", "203.0.113.10:31842, 198.51.100.7:31842");
                Env("ASTER_RELAY_CA_CERTS", "/etc/aster/relay-ca.pem");
                Env("ASTER_RELAY_CA_ONLY", "1");
                Env("ASTER_DISCOVERY", "none");
                Env("ASTER_METRICS_PEER_LABELS", "true");
                Env("ASTER_METRICS_PEER_LABEL_CAP", "5");
                var env = AsterConfig.FromEnv();
                Assert.Equal("[::]:7842", env.BindAddrV6);
                Assert.Equal(["203.0.113.10:31842", "198.51.100.7:31842"], env.AdvertiseAddrs);
                Assert.Equal(["/etc/aster/relay-ca.pem"], env.RelayCaCerts);
                Assert.True(env.RelayCaOnly);
                Assert.Equal(DiscoveryMode.None, env.Discovery);
                Assert.True(env.MetricsPeerLabels);
                Assert.Equal(5u, env.MetricsPeerLabelCap);
            }
            finally
            {
                foreach (var k in keys) unsetenv(k);
            }

            string path = Path.Combine(Path.GetTempPath(), $"aster-abi7-{Guid.NewGuid():N}.toml");
            try
            {
                File.WriteAllText(path, "[network]\nbind_addr_v6 = \"[::]:7843\"\nadvertise_addrs = [\"203.0.113.10:31843\"]\n" +
                    "discovery_dns_origin = \"dns.example.internal\"\ndiscovery_pkarr_relays = [\"https://dns.example.internal/pkarr\"]\n" +
                    "\n[metrics]\npeer_labels = true\npeer_label_cap = 7\n");
                var file = AsterConfig.FromToml(path);
                Assert.Equal("[::]:7843", file.BindAddrV6);
                Assert.Equal(DiscoveryMode.Custom, file.Discovery);
                Assert.Equal("dns.example.internal", file.DiscoveryDnsOrigin);
                Assert.Single(file.DiscoveryPkarrRelays);
                Assert.Single(file.AdvertiseAddrs);
                Assert.True(file.MetricsPeerLabels);
                Assert.Equal(7u, file.MetricsPeerLabelCap);
            }
            finally { File.Delete(path); }
        }
    }

    internal static readonly object EnvLock = new();

    internal static void Env(string name, string value) => Assert.Equal(0, setenv(name, value, 1));

    [DllImport("libc", SetLastError = true)]
    private static extern int setenv(string name, string value, int overwrite);

    [DllImport("libc", SetLastError = true)]
    internal static extern int unsetenv(string name);

    [Fact]
    public async Task Node_Starts_With_Discovery_None_And_Advertised_Addresses()
    {
        const string advertised = "203.0.113.10:31842";
        await using var node = await Node.StartAsync(Loopback with { Discovery = DiscoveryMode.None, AdvertiseAddrs = [advertised] }).WaitAsync(Budget);
        var addr = node.NodeAddr();
        for (int i = 0; i < 100 && !addr.DirectAddresses.Contains(advertised); i++) { await Task.Delay(50); addr = node.NodeAddr(); }
        Assert.Contains(advertised, addr.DirectAddresses);
    }

    // ─── 7.4 transfer metrics ───────────────────────────────────────────────

    [Fact]
    public void Fetch_Report_V1_And_V2_Parse_And_Truncation_Is_Refused()
    {
        byte[] node = Enumerable.Range(0, 32).Select(i => (byte)i).ToArray();
        byte[] v1 = new byte[16 + 44 + 3];
        v1[0] = 1;
        BinaryPrimitives.WriteUInt32LittleEndian(v1.AsSpan(4), 1);
        BinaryPrimitives.WriteUInt64LittleEndian(v1.AsSpan(8), 99);
        node.CopyTo(v1, 16);
        BinaryPrimitives.WriteUInt32LittleEndian(v1.AsSpan(16 + 32), 2);
        BinaryPrimitives.WriteUInt32LittleEndian(v1.AsSpan(16 + 36), 1);
        BinaryPrimitives.WriteUInt32LittleEndian(v1.AsSpan(16 + 40), 1);
        var r1 = Ops.ParseFetchReport(v1, out int c1);
        Assert.Equal(60, c1);
        Assert.Equal(99UL, r1.BytesTransferred);
        var p1 = Assert.Single(r1.Providers);
        Assert.Equal(Convert.ToHexString(node).ToLowerInvariant(), p1.NodeId);
        Assert.Equal((2u, 1u, 1u, 0u, 0u, 0UL, (ulong?)null), (p1.Attempts, p1.Failures, p1.CompletedRequests, p1.InFlight, p1.PlainFallbacks, p1.BytesTransferred, p1.WireBytes));

        byte[] v2 = new byte[16 + 72 * 2 + 5];
        v2[0] = 2;
        BinaryPrimitives.WriteUInt32LittleEndian(v2.AsSpan(4), 2);
        BinaryPrimitives.WriteUInt64LittleEndian(v2.AsSpan(8), 300);
        for (int i = 0; i < 2; i++)
        {
            var r = v2.AsSpan(16 + 72 * i, 72);
            node.CopyTo(r);
            BinaryPrimitives.WriteUInt32LittleEndian(r[32..], 3);
            BinaryPrimitives.WriteUInt32LittleEndian(r[40..], 1);
            BinaryPrimitives.WriteUInt32LittleEndian(r[44..], (uint)i);
            BinaryPrimitives.WriteUInt32LittleEndian(r[48..], (uint)(1 - i));
            BinaryPrimitives.WriteUInt32LittleEndian(r[52..], (uint)i); // HAS_WIRE_BYTES on the second
            BinaryPrimitives.WriteUInt64LittleEndian(r[56..], 100UL + (ulong)(100 * i));
            BinaryPrimitives.WriteUInt64LittleEndian(r[64..], 77);
        }
        var r2 = Ops.ParseFetchReport(v2, out int c2);
        Assert.Equal(16 + 144, c2);
        Assert.Equal((0u, 1u, 100UL, (ulong?)null), (r2.Providers[0].InFlight, r2.Providers[0].PlainFallbacks, r2.Providers[0].BytesTransferred, r2.Providers[0].WireBytes));
        Assert.Equal((1u, 0u, 200UL, (ulong?)77), (r2.Providers[1].InFlight, r2.Providers[1].PlainFallbacks, r2.Providers[1].BytesTransferred, r2.Providers[1].WireBytes));
        Assert.Single(r2.InFlight());
        Assert.Single(r2.PlainOnly());

        Assert.Throws<IrohException>(() => Ops.ParseFetchReport(v2.AsSpan(0, 16 + 72 + 10), out _));
        Assert.Throws<IrohException>(() => Ops.ParseFetchReport(v1.AsSpan(0, 8), out _));
        byte[] v3 = (byte[])v1.Clone();
        v3[0] = 3;
        Assert.Throws<IrohException>(() => Ops.ParseFetchReport(v3, out _));
        byte[] huge = (byte[])v1.Clone();
        BinaryPrimitives.WriteUInt32LittleEndian(huge.AsSpan(4), uint.MaxValue);
        Assert.Throws<IrohException>(() => Ops.ParseFetchReport(huge, out _));
    }

    [Fact]
    public void Peer_View_Parses_Quic_Traffic_And_Blob_Transfer()
    {
        const string json = """
            {"node_id":"id","level":0,"quic_traffic":{"blobs":{"quic_bytes_sent":1,"quic_bytes_received":2},
             "blobs-zstd":{"quic_bytes_sent":3,"quic_bytes_received":4},"docs":{"quic_bytes_sent":0,"quic_bytes_received":0},
             "gossip":{"quic_bytes_sent":0,"quic_bytes_received":0},"rpc":{"quic_bytes_sent":10,"quic_bytes_received":20},
             "other":{"quic_bytes_sent":0,"quic_bytes_received":0}},
             "blob_transfer":{"fetched_payload_bytes":5,"fetched_zstd_payload_bytes":6,"fetched_wire_bytes":7,"fetched_plain_bytes":8,
             "served_payload_bytes":9,"served_zstd_payload_bytes":10,"served_wire_bytes":11,"served_plain_bytes":12,"plain_fallbacks":13}}
            """;
        var peer = JsonSerializer.Deserialize<TopologyPeerView>(json, TopologyView.JsonOptions)!;
        Assert.Equal(new TopologyQuicBytes(3, 4), peer.QuicTraffic!.BlobsZstd);
        Assert.Equal(new TopologyQuicBytes(14, 26), peer.QuicTraffic.Total());
        Assert.Equal(13UL, peer.BlobTransfer!.PlainFallbacks);
        Assert.Equal(7UL, peer.BlobTransfer.FetchedWireBytes);
        Assert.Null(JsonSerializer.Deserialize<TopologyPeerView>("""{"node_id":"id"}""", TopologyView.JsonOptions)!.QuicTraffic);
    }

    [Fact]
    public async Task Fetch_Reports_Per_Provider_Bytes_And_Node_Counters_And_Peer_Traffic()
    {
        var config = Loopback with { EnableMonitoring = true };
        await using var provider = await Node.StartAsync(config).WaitAsync(Budget);
        await using var fetcher = await Node.StartAsync(config).WaitAsync(Budget);
        byte[] data = Encoding.UTF8.GetBytes(string.Concat(Enumerable.Repeat("aster transfer ", 4000)));
        var (hash, _) = await provider.Blobs().AddBytesAsync(data);
        var addr = provider.NodeAddr();
        for (int i = 0; i < 100 && addr.DirectAddresses.Count == 0; i++) { await Task.Delay(50); addr = provider.NodeAddr(); }
        fetcher.AddNodeAddr(addr);
        var before = fetcher.Blobs().TransferStats();

        var result = await fetcher.Blobs().DownloadHashFromAsync(hash, [provider.NodeId()]).WaitAsync(Budget);
        Assert.Equal(data, result.Data);
        var stats = Assert.Single(result.Report.Providers);
        Assert.Equal(result.Report.BytesTransferred, stats.BytesTransferred);
        Assert.True(stats.BytesTransferred > 0);
        Assert.Equal(1u, stats.CompletedRequests);
        Assert.Equal(0u, stats.InFlight);
        Assert.Single(result.Report.Served());

        var after = fetcher.Blobs().TransferStats();
        Assert.True(after.FetchedPayloadBytes - before.FetchedPayloadBytes >= (ulong)data.Length);
        Assert.True(after.FetchZstdOk + after.FetchPlainOk > before.FetchZstdOk + before.FetchPlainOk);
        Assert.True(provider.Blobs().TransferStats().ServedPayloadBytes >= (ulong)data.Length);

        TopologyPeerView? peer = null;
        for (int i = 0; i < 100 && peer?.BlobTransfer is not { FetchedPayloadBytes: > 0 }; i++)
        {
            peer = fetcher.Topology.Peer(provider.NodeId());
            if (peer?.BlobTransfer is not { FetchedPayloadBytes: > 0 }) await Task.Delay(50);
        }
        Assert.NotNull(peer?.QuicTraffic);
        Assert.True(peer!.BlobTransfer!.FetchedPayloadBytes >= (ulong)data.Length);
        Assert.True(peer.QuicTraffic!.Total().Received > 0);
    }

    // ─── 7.2 drain and 7.5 call identity ────────────────────────────────────

    private sealed class Rig : IAsyncDisposable
    {
        public AsterServer Server = null!;
        public AsterClient Client = null!;
        public RpcConnection Conn = null!;
        public readonly TaskCompletionSource SlowEntered = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource<(ServerCallContext Ctx, ServerCallContext? Current, ServerCallContext? InTask, bool CancelledAfter)> Seen
            = new(TaskCreationOptions.RunContinuationsAsynchronously);

        public static async Task<Rig> Start()
        {
            var rig = new Rig();
            var service = ServiceDefinition.Create("Abi7", 1)
                .Unary<EchoRequest, EchoReply>("echo", (r, _) => ValueTask.FromResult(new EchoReply { Message = r.Message }))
                .Unary<EchoRequest, EchoReply>("slow", async (r, _) =>
                {
                    rig.SlowEntered.TrySetResult();
                    await Task.Delay(TimeSpan.FromMilliseconds(r.Count));
                    return new EchoReply { Message = "slow" };
                })
                .Unary<EchoRequest, EchoReply>("context", async (r, ctx) =>
                {
                    var inTask = await Task.Run(() => ServerCallContext.Current);
                    bool cancelled = false;
                    if (r.Count > 0)
                    {
                        await ctx.CancelledAsync().WaitAsync(TimeSpan.FromSeconds(10));
                        cancelled = ctx.IsCancelled;
                    }
                    rig.Seen.TrySetResult((ctx, ServerCallContext.Current, inTask, cancelled));
                    return new EchoReply { Message = ctx.Peer, ServerLang = ctx.Principal };
                })
                .Build();
            rig.Server = await AsterServer.StartAsync(new AsterServerOptions { Config = Loopback, Services = { service } }).WaitAsync(Budget);
            rig.Client = await AsterClient.StartAsync(Loopback);
            rig.Client.RegisterTypes(service);
            rig.Conn = await rig.Client.ConnectAsync(await rig.Server.TicketAsync()).WaitAsync(Budget);
            return rig;
        }

        public async ValueTask DisposeAsync()
        {
            await Client.DisposeAsync();
            await Server.DisposeAsync();
        }
    }

    [Fact]
    public async Task Handler_Context_Carries_Transport_Peer_Principal_And_Call_Identity()
    {
        await using var rig = await Rig.Start();
        Assert.Null(ServerCallContext.Current);
        var reply = await rig.Conn.UnaryAsync<EchoRequest, EchoReply>("Abi7", "context", new EchoRequest(), new CallOptions
        {
            Deadline = TimeSpan.FromSeconds(20),
            Metadata = [new("k", "v1"), new("k", "v2")],
        }).WaitAsync(Budget);
        var (ctx, current, inTask, _) = await rig.Seen.Task.WaitAsync(Budget);
        string clientId = rig.Client.Node.NodeId();
        Assert.Same(ctx, current);
        Assert.Same(ctx, inTask);
        Assert.Equal(clientId, ctx.Peer);
        Assert.Equal(clientId, ctx.Principal);
        Assert.Equal(clientId, ctx.PeerId);
        Assert.False(ctx.HasPrincipal);
        Assert.True(ctx.IrohAuthenticated);
        Assert.NotEqual(0UL, ctx.CallId);
        Assert.Equal("v2", ctx.MetadataValue("k"));
        Assert.Null(ctx.MetadataValue("missing"));
        Assert.NotNull(ctx.Deadline);
        Assert.InRange(ctx.Remaining!.Value, TimeSpan.Zero, TimeSpan.FromSeconds(20));
        Assert.Equal(clientId, reply.Message);
        Assert.Equal(clientId, reply.ServerLang);
        Assert.Null(ServerCallContext.Current);
        Assert.Throws<InvalidOperationException>(() => ServerCallContext.Require());
    }

    [Fact]
    public async Task Drain_Lets_In_Flight_Calls_Finish_Refuses_New_Ones_And_Reports()
    {
        await using var rig = await Rig.Start();
        Assert.Equal("x", (await rig.Conn.UnaryAsync<EchoRequest, EchoReply>("Abi7", "echo", new EchoRequest { Message = "x" })).Message);
        var slow = rig.Conn.UnaryAsync<EchoRequest, EchoReply>("Abi7", "slow", new EchoRequest { Count = 1500 });
        await rig.SlowEntered.Task.WaitAsync(Budget);
        Assert.False(rig.Server.IsDraining);
        var drain = rig.Server.DrainAsync(TimeSpan.FromSeconds(10));
        // IsDraining is the managed flag, set before the native drain runs on the pool: it does not
        // witness the server refusing. Wait for the refusal itself, as a client sees it.
        Assert.True(rig.Server.IsDraining);
        var refused = await RefusedAsDraining(rig.Conn).WaitAsync(Budget);
        Assert.True(RpcError.IsDrainingError(refused), refused.ToString());
        Assert.Equal("slow", (await slow.WaitAsync(Budget)).Message);
        var report = await drain.WaitAsync(Budget);
        // The slow call, plus at most one polling echo that was in flight when the native drain began.
        Assert.InRange(report.CallsInFlight, 1UL, 2UL);
        Assert.True(report.CallsCompleted >= 1);
        Assert.Equal(0UL, report.CallsCancelled);
        Assert.True(report.CallsRefused >= 1);
        Assert.False(report.DeadlineReached);
        Assert.True(report.Elapsed > TimeSpan.Zero);
        await Assert.ThrowsAsync<ObjectDisposedException>(() => rig.Server.DrainAsync(TimeSpan.Zero));
    }

    /// <summary>
    /// Calls echo until the server refuses one (the native drain has started); returns that error.
    /// Calls before then succeed. Bounded by the caller's WaitAsync.
    /// </summary>
    private static async Task<Exception> RefusedAsDraining(RpcConnection conn)
    {
        while (true)
        {
            try
            {
                await conn.UnaryAsync<EchoRequest, EchoReply>("Abi7", "echo", new EchoRequest()).WaitAsync(Budget);
            }
            catch (Exception e)
            {
                return e;
            }
            await Task.Delay(10);
        }
    }

    [Fact]
    public async Task Drain_Grace_Cancels_What_Still_Runs()
    {
        await using var rig = await Rig.Start();
        var slow = rig.Conn.UnaryAsync<EchoRequest, EchoReply>("Abi7", "slow", new EchoRequest { Count = 20_000 });
        await rig.SlowEntered.Task.WaitAsync(Budget);
        // short-wait-ok: the token ends the drain grace early; its cancellation is what the test drives
        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(200));
        var report = await rig.Server.DrainAsync(TimeSpan.FromSeconds(20), cts.Token).WaitAsync(Budget);
        Assert.True(report.DeadlineReached);
        Assert.Equal(1UL, report.CallsCancelled);
        Assert.True(report.Elapsed < TimeSpan.FromSeconds(15));
        await Assert.ThrowsAnyAsync<Exception>(() => slow.WaitAsync(Budget));
    }

    [Fact]
    public void Transport_Peer_Of_A_Call_Already_Over_Is_Not_The_Principal()
    {
        // HAS_PRINCIPAL and aster_dispatch_identity read the transport peer: the call is dispatched with it.
        Assert.Equal("gateway-node", AsterServer.ResolveTransportPeer(true, "https://idp|alice", "gateway-node"));
        // NOT_FOUND (the call is over): no transport peer, so the call is dropped rather than reporting
        // the user as the transport peer and, on a forwarded call, as the actor.
        Assert.Null(AsterServer.ResolveTransportPeer(true, "https://idp|alice", null));
        // Without HAS_PRINCIPAL the event's peer is the transport peer.
        Assert.Equal("node-a", AsterServer.ResolveTransportPeer(false, "node-a", null));
    }

    [Fact]
    public void Drain_Grace_Rounds_Up_To_Whole_Milliseconds()
    {
        Assert.Equal(0u, AsterServer.DrainMillis(TimeSpan.Zero));
        Assert.Equal(2u, AsterServer.DrainMillis(TimeSpan.FromTicks(10_001)));
        Assert.Equal(uint.MaxValue, AsterServer.DrainMillis(TimeSpan.FromDays(100)));
    }

    [Fact]
    public void Draining_Status_Is_Recognised()
    {
        var e = new RpcError(StatusCode.Unavailable, "draining", new() { [RpcError.DrainingDetailKey] = "true" });
        Assert.True(e.IsDraining);
        Assert.True(RpcError.IsDrainingError(new AggregateException(e)) || RpcError.IsDrainingError(new InvalidOperationException("x", e)));
        Assert.False(new RpcError(StatusCode.Unavailable, "down").IsDraining);
        Assert.False(new RpcError(StatusCode.Internal, "x", new() { [RpcError.DrainingDetailKey] = "true" }).IsDraining);
    }
}
