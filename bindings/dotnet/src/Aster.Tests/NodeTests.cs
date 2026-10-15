using System;
using System.IO;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;
using Aster.Crypto;
using Aster.Interceptors;

namespace Aster.Tests;

/// <summary>ABI 3.2: aster_config_t, aster_node_start, Gate 0, admission events, tickets.</summary>
public class NodeTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    [Fact]
    public unsafe void Native_Struct_Sizes_Match_Header()
    {
        Assert.Equal(96, sizeof(StreamHeaderNative));
        Assert.Equal(72, sizeof(CallHeaderNative));
        Assert.Equal(64, sizeof(RpcStatusNative));
        Assert.Equal(288, sizeof(ConfigNative));
        Assert.Equal(104, sizeof(GrantContextNative));
        Assert.Equal(72, sizeof(GrantRecordNative));
        Assert.Equal(72, sizeof(GrantOutcomeNative));
        Assert.Equal(72, sizeof(PathInfoNative));
        Assert.Equal(32, sizeof(PathListNative));
    }

    [Fact]
    public void Abi_Is_5_x()
    {
        var (major, minor, _) = Runtime.Version;
        Assert.Equal(5, major);
        Assert.True(minor >= (int)Native.RequiredAbiMinor, $"native library ABI 5.{minor} < 5.{Native.RequiredAbiMinor}");
    }

    [Fact]
    public void Bytes_FromArray_Is_Gone()
        => Assert.Null(typeof(Bytes).GetMethod("FromArray"));

    [Fact]
    public void Config_Round_Trips_Through_Native()
    {
        var c = new AsterConfig
        {
            RelayMode = "disabled",
            BindAddr = "127.0.0.1:0",
            Alpns = ["a/1"],
            SecretKey = Ids.GenerateSecretKey(),
            StoragePath = "/tmp/x",
            GcIntervalMs = 1234,
            HookTimeoutMs = 777,
            HookFailureMode = HookFailureMode.FailClosed,
            EnableHooks = true,
            Portmapper = Portmapper.Disabled,
            Transport = new TransportConfig { MaxConcurrentBidiStreams = 99, SendFairness = false, InitialMtu = 1300 },
        };
        using var scope = new NativeScope();
        var n = c.ToNative(scope, ["b/2"]);
        Assert.Equal((uint)RelayMode.Disabled, n.relay_mode);
        Assert.Equal(new[] { "a/1", "b/2" }, NativeScope.ReadUtf8List(n.alpns));
        Assert.Equal("127.0.0.1:0", n.bind_addr.ToUtf8String());
        Assert.Equal(c.SecretKey, n.secret_key.ToArray());
        Assert.Equal(1234ul, n.gc_interval_ms);
        Assert.Equal(1u, n.hook_failure_mode);
        Assert.Equal(2u, n.portmapper);
        Assert.Equal(99ul, n.transport_max_concurrent_bidi_streams);
        Assert.Equal((1u << 0) | (1u << 7) | (1u << 10), n.transport_set);
    }

    [Fact]
    public void Config_Relay_Url_Means_Custom()
    {
        using var scope = new NativeScope();
        var n = new AsterConfig { RelayMode = "https://relay.example" }.ToNative(scope);
        Assert.Equal((uint)RelayMode.Custom, n.relay_mode);
        Assert.Equal(new[] { "https://relay.example" }, NativeScope.ReadUtf8List(n.relay_urls));
    }

    [Fact]
    public void Config_FromToml_Reads_Node_And_Binding_Sections()
    {
        string path = Path.Combine(Path.GetTempPath(), $"aster-{Guid.NewGuid():N}.toml");
        File.WriteAllText(path, """
            [network]
            relay_mode = "disabled"
            bind_addr = "127.0.0.1:0"

            [connect]
            endpoint_addr = "aster1example"
            """);
        try
        {
            var c = AsterConfig.FromToml(path);
            Assert.Equal("disabled", c.RelayMode);
            Assert.Equal("127.0.0.1:0", c.BindAddr);
            Assert.Equal("aster1example", c.EndpointAddr);
            Assert.False(c.IsEnrollmentEnabled);
        }
        finally { File.Delete(path); }
    }

    [Fact]
    public void Config_Default_And_FromEnv_Load()
    {
        Assert.NotNull(AsterConfig.Default().RelayMode);
        Assert.NotNull(AsterConfig.FromEnv());
        Assert.DoesNotContain(Convert.ToHexString(new byte[] { 1, 2, 3 }),
            (new AsterConfig { SecretKey = [1, 2, 3] }).ToString());
    }

    [Fact]
    public async Task Start_Node_With_Secret_Key_Has_Matching_Id_And_Direct_Addresses()
    {
        byte[] key = Ids.GenerateSecretKey();
        await using var node = await Node.StartAsync(Loopback with { SecretKey = key });
        Assert.Equal(Ids.NodeId(key), node.NodeId());
        var ticket = await node.TicketAsync(TimeSpan.FromSeconds(5));
        Assert.Equal(node.NodeId(), ticket.EndpointId);
        Assert.NotEmpty(ticket.DirectAddrs);
        Assert.All(ticket.DirectAddrs, a => Assert.StartsWith("127.0.0.1:", a));
    }

    [Fact]
    public void Ticket_Round_Trips()
    {
        string id = Ids.NodeId(Ids.GenerateSecretKey());
        var t = new AsterTicket(id, null, ["127.0.0.1:4000", "10.0.0.1:5"]);
        var parsed = AsterTicket.Parse(t.ToString());
        Assert.Equal(id, parsed.EndpointId);
        Assert.Equal(t.DirectAddrs, parsed.DirectAddrs);
        Assert.StartsWith("aster1", t.ToString());
        Assert.False(AsterTicket.TryParse("aster1nope", out _));
    }

    [Fact]
    public async Task Invalid_Config_Is_Rejected_Before_Start()
    {
        var ex = await Assert.ThrowsAsync<IrohException>(() => Node.StartAsync(Loopback with { BindAddr = "not an address" }));
        Assert.Contains("aster_node_start", ex.Message);
    }

    [Fact]
    public async Task Nodes_Connect_And_Exchange_Bytes()
    {
        await using var a = await Node.StartAsync(Loopback with { Alpns = ["test/1"] });
        await using var b = await Node.StartAsync(Loopback with { Alpns = ["test/1"] });
        var accept = b.AcceptAsync();
        using var conn = await a.ConnectAsync(await b.TicketAsync(TimeSpan.FromSeconds(5)), "test/1");
        using var inbound = await accept.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.Equal(a.NodeId(), inbound.RemoteId());
        Assert.NotEmpty(conn.Paths());
        Assert.NotNull(conn.SelectedPath());

        var (send, _) = await conn.OpenBiAsync();
        await send.SetPriorityAsync(7);
        Assert.Equal(7, await send.GetPriorityAsync());
    }

    [Fact]
    public void Gate_Policy_Admit_Revoke()
    {
        using var gate = Gate.Create().SetReject(4403, "go away");
        string peer = Ids.NodeId(Ids.GenerateSecretKey());
        Assert.False(gate.IsAdmitted(peer));
        Assert.False(gate.ShouldAllow(peer, "aster/1"));
        gate.Admit(peer.ToUpperInvariant());
        Assert.True(gate.IsAdmitted(peer));
        Assert.True(gate.ShouldAllow(peer, "aster/1"));
        gate.Revoke(peer);
        Assert.False(gate.IsAdmitted(peer));

        gate.SetAdmissionAlpns(["admit/1"]);
        Assert.True(gate.ShouldAllow(peer, "admit/1"));
        gate.SetTrustMode(TrustMode.OpenDev);
        Assert.True(gate.ShouldAllow(peer, "aster/1"));
    }

    [Fact]
    public async Task Protected_Gate_Rejects_Until_Admitted()
    {
        using var gate = Gate.Create();
        await using var server = await Node.StartAsync(Loopback with { Alpns = ["svc/1"] }, gate);
        await using var client = await Node.StartAsync(Loopback with { Alpns = ["svc/1"] });
        var ticket = await server.TicketAsync(TimeSpan.FromSeconds(5));

        await Assert.ThrowsAnyAsync<Exception>(async () =>
        {
            using var c = await client.ConnectAsync(ticket, "svc/1").WaitAsync(TimeSpan.FromSeconds(15));
            // The gate closes after the handshake; a stream open then fails.
            await c.OpenBiAsync().WaitAsync(TimeSpan.FromSeconds(10));
            var (s, r) = await c.OpenBiAsync();
            await s.SendAsync([1]);
            await r.ReadAsync(1).WaitAsync(TimeSpan.FromSeconds(10));
        });

        gate.Admit(client.NodeId());
        var accept = server.AcceptAsync();
        using var ok = await client.ConnectAsync(ticket, "svc/1").WaitAsync(TimeSpan.FromSeconds(15));
        using var inbound = await accept.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.Equal(client.NodeId(), inbound.RemoteId());
    }

    [Fact]
    public async Task Admission_Events_Are_Answered_From_Dotnet()
    {
        await using var server = await Node.StartAsync(Loopback with { Alpns = ["svc/1"], EnableHooks = true });
        await using var client = await Node.StartAsync(Loopback with { Alpns = ["svc/1"] });
        var ticket = await server.TicketAsync(TimeSpan.FromSeconds(5));

        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var answered = Task.Run(async () =>
        {
            await foreach (var req in server.Admissions(cts.Token))
            {
                if (req.Inbound)
                {
                    Assert.Equal(client.NodeId(), req.PeerId);
                    Assert.Equal("svc/1", req.Alpn);
                    req.Allow();
                    return req;
                }
                req.Allow();
            }
            throw new InvalidOperationException("no inbound admission");
        });
        var accept = server.AcceptAsync();
        using var conn = await client.ConnectAsync(ticket, "svc/1").WaitAsync(TimeSpan.FromSeconds(15));
        var request = await answered.WaitAsync(TimeSpan.FromSeconds(15));
        using var inbound = await accept.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.Throws<InvalidOperationException>(() => request.Allow());
    }

    [Fact]
    public async Task Admissions_Need_Hooks()
    {
        await using var node = await Node.StartAsync(Loopback);
        Assert.Throws<InvalidOperationException>(() => node.Admissions());
    }

    [Fact]
    public async Task Gate_Owned_Runtime_Outlives_Gate_Until_Node_Closes()
    {
        var gate = Gate.Create();
        var node = await Node.StartAsync(Loopback, gate);
        gate.Dispose();
        Assert.Equal(64, node.NodeId().Length);
        await node.DisposeAsync();
    }

    [Fact]
    public async Task Endpoint_Is_A_Node_Handle()
    {
        // ABI 4.0 removed bare endpoints: Endpoint.CreateAsync starts a node, so the node
        // functions accept its handle and report the same id.
        using var runtime = new Runtime();
        var ep = await Endpoint.CreateAsync(runtime, new EndpointConfigBuilder().Alpn("ep-node").EnableDiscovery(false));
        IntPtr buf = System.Runtime.InteropServices.Marshal.AllocHGlobal(65);
        try
        {
            Native.Check(Native.aster_node_id(runtime.Handle, ep.Handle.Value, buf, (UIntPtr)65, out UIntPtr len), "aster_node_id");
            Assert.Equal(ep.NodeId(), System.Runtime.InteropServices.Marshal.PtrToStringUTF8(buf, (int)len));
        }
        finally { System.Runtime.InteropServices.Marshal.FreeHGlobal(buf); }
        await ep.CloseAsync();
    }

    [Fact]
    public void EndpointConfigBuilder_Maps_To_AsterConfig()
    {
        var cfg = new EndpointConfigBuilder().Alpn("a").Alpns(["b"]).SetRelayMode(RelayMode.Disabled)
            .EnableDiscovery(false).ToAsterConfig();
        Assert.Equal(new[] { "a", "b" }, cfg.Alpns);
        Assert.Equal("disabled", cfg.RelayMode);
        Assert.False(cfg.LocalDiscovery);
        Assert.Null(new EndpointConfigBuilder().ToAsterConfig().RelayMode);
    }
}
