using System.Collections.Concurrent;
using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;
using Aster.Upcall;
namespace Aster.Tests;

public class RawWebTransportTests
{
    private static RawWTOptions Options()
    {
        var cert = TlsCertificates.GenerateWebTransportCertificate("raw-test", ["127.0.0.1"]);
        return new() { CertificatePem = cert.CertificatePem, PrivateKeyPem = cert.PrivateKeyPem, Path = "/raw" };
    }
    private sealed class Peer : IAsyncDisposable
    {
        private readonly Process _process;
        private readonly Task<string> _output, _error;
        internal Peer(string url, string scenario, params string[] extra)
        {
            var root = new DirectoryInfo(AppContext.BaseDirectory);
            while (root is not null && !File.Exists(Path.Combine(root.FullName, "ffi", "aster_ffi.h"))) root = root.Parent;
            if (root is null) throw new InvalidOperationException("cannot locate raw WT test peer source");
            string? python = Environment.GetEnvironmentVariable("ASTER_TEST_PYTHON");
            if (string.IsNullOrEmpty(python)) { python = Path.Combine(root.FullName, ".venv", "bin", "python"); if (!File.Exists(python)) python = "python3"; }
            var start = new ProcessStartInfo(python) { RedirectStandardOutput = true, RedirectStandardError = true, UseShellExecute = false };
            foreach (var arg in new[] { Path.Combine(root.FullName, "tests", "python", "raw_wt_peer.py"), "--url", url, "--scenario", scenario }.Concat(extra)) start.ArgumentList.Add(arg);
            _process = Process.Start(start) ?? throw new InvalidOperationException("raw WT peer did not start");
            _output = _process.StandardOutput.ReadToEndAsync(); _error = _process.StandardError.ReadToEndAsync();
        }
        internal async Task Pass()
        {
            await _process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(30));
            string output = await _output, error = await _error;
            Assert.True(_process.ExitCode == 0, $"peer exit {_process.ExitCode}: {output}\n{error}");
            Assert.Contains("\"result\": \"PASS\"", output);
        }
        public async ValueTask DisposeAsync()
        {
            if (!_process.HasExited) { _process.Kill(true); await _process.WaitForExitAsync(); }
            _process.Dispose();
        }
    }
    private static async Task<byte[]> ReadAll(RawWTStream stream, CancellationToken ct)
    {
        using var bytes = new MemoryStream(); while (await stream.ReadAsync(1024, ct) is { } chunk) bytes.Write(chunk); return bytes.ToArray();
    }
    private static async Task Echo(RawWTSession session, CancellationToken ct)
    {
        using var bi = await session.AcceptBiAsync(ct); Assert.True(bi.CanSend); Assert.True(bi.CanReceive);
        Assert.Equal("control"u8.ToArray(), await ReadAll(bi, ct));
        await bi.SetPriorityAsync(-10, ct); Assert.Equal(-10, await bi.PriorityAsync(ct));
        await bi.WriteAsync("CONTROL"u8.ToArray(), ct); await bi.FinishAsync(ct); Assert.True(await bi.TransportAckedAsync(ct));
        using var input = await session.AcceptUniAsync(ct); Assert.False(input.CanSend); Assert.True(input.CanReceive);
        Assert.Equal("input"u8.ToArray(), await ReadAll(input, ct));
        using var frame = await session.OpenUniAsync(ct); Assert.True(frame.CanSend); Assert.False(frame.CanReceive);
        await frame.WriteAsync("frame"u8.ToArray(), ct); await frame.FinishAsync(ct); Assert.True(await frame.TransportAckedAsync(ct));
        Assert.Equal("feedback"u8.ToArray(), await session.ReceiveDatagramAsync(ct)); Assert.True(session.MaxDatagramSize > 0);
        Assert.Equal(1, session.Info.Version); Assert.NotEmpty(session.Info.PeerAddress);
        Assert.Equal(1, session.Stats.Version); Assert.True(session.Stats.UdpRxPackets > 0); Assert.True(session.Stats.UdpTxPackets > 0);
        session.SendDatagram("reply"u8);
    }
    [Fact]
    public void LayoutAndStrictPayloadContracts()
    {
        Assert.Equal(0u, Native.RequiredAbiMinor); Assert.Equal(104, Marshal.SizeOf<RawWTConfigNative>());
        foreach (var (name, offset) in new[] { ("struct_size", 0), ("flags", 4), ("bind_addr", 8), ("certificate_pem", 24), ("private_key_pem", 40), ("path", 56), ("max_sessions", 72), ("max_streams", 76), ("max_datagrams", 80), ("max_datagram_bytes", 84), ("max_read_bytes", 88), ("max_pending_auth", 92), ("max_auth_payload_bytes", 96), ("reserved", 100) }) Assert.Equal(offset, Marshal.OffsetOf<RawWTConfigNative>(name).ToInt32());
        Assert.Equal(121u, (uint)EventKind.RawWTSession); Assert.Equal(126u, (uint)EventKind.RawWTAcked);
        Assert.Null(RawWTStream.DecodePriority(0, [])); Assert.Equal(-10, RawWTStream.DecodePriority(1, [246, 255, 255, 255]));
        Assert.Throws<InvalidDataException>(() => RawWTStream.DecodePriority(0, [0, 0, 0, 0]));
        Assert.Throws<InvalidDataException>(() => RawWTStream.DecodePriority(1, [1, 0, 0]));
        Assert.Throws<InvalidDataException>(() => RawWTStream.DecodePriority(2, [1, 0, 0, 0]));
        Assert.Throws<InvalidDataException>(() => RawWTJson.Decode<RawWTCloseInfo>("{\"version\":2}"u8.ToArray()));
        var stats = RawWTJson.Decode<RawWTStats>("{\"version\":1,\"rtt_micros\":15,\"cwnd\":50,\"udp_tx_bytes\":100,\"udp_rx_packets\":7}"u8.ToArray());
        Assert.Equal(15UL, stats.RttMicros); Assert.Equal(50UL, stats.CongestionWindow); Assert.Equal(100UL, stats.UdpTxBytes); Assert.Equal(7UL, stats.UdpRxPackets);
    }
    [Fact]
    public async Task PublicRawNetworkStreamsDatagramsAckAndStats()
    {
        using var runtime = new Runtime(); using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(25));
        await using var listener = await RawWTListener.StartPublicAsync(runtime, Options(), timeout.Token);
        await using var peer = new Peer($"https://{listener.QuicAddress}/raw", "echo");
        using var session = await listener.AcceptAsync(timeout.Token); Assert.Null(session.Info.Principal); Assert.Empty(session.Info.Attributes);
        await Echo(session, timeout.Token); await peer.Pass(); Assert.Equal(1, (await session.ClosedAsync(timeout.Token)).Version);
    }
    [Fact]
    public async Task CancelAcceptDoesNotConsumeNextSessionAndStopReleasesSockets()
    {
        using var runtime = new Runtime(); await using var listener = await RawWTListener.StartPublicAsync(runtime, Options());
        using var cancel = new CancellationTokenSource(TimeSpan.FromMilliseconds(20));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => listener.AcceptAsync(cancel.Token));
        await using var peer = new Peer($"https://{listener.QuicAddress}/raw", "connect");
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(15)); using var session = await listener.AcceptAsync(timeout.Token);
        await peer.Pass(); await listener.DisposeAsync(); await listener.DisposeAsync();
        var address = IPEndPoint.Parse(listener.TcpAddress);
        using var tcp = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp); tcp.Bind(address); tcp.Listen();
        using var udp = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp); udp.Bind(address);
    }
    [Fact]
    public async Task AuthenticationBeforeUpgradeExactRouteAndRetainedIdentity()
    {
        using var runtime = new Runtime(); await using var upcalls = Upcalls.Create(runtime, 8);
        var seen = new ConcurrentQueue<RpcAuthRequest>();
        upcalls.ServeRpcAuthenticator((req, _) => { seen.Enqueue(req); return ValueTask.FromResult(req.Metadata.GetValueOrDefault("authorization") == "Bearer valid" ? new RpcAuthResponse(true, "alice", new Dictionary<string, string> { ["portal.role"] = "viewer" }) : RpcAuthResponse.Rejected()); });
        await using var listener = await RawWTListener.StartAsync(runtime, Options(), upcalls);
        await using (var denied = new Peer($"https://{listener.QuicAddress}/raw?cap=a&cap=b", "connect", "--token", "Bearer invalid", "--expected-status", "401")) await denied.Pass();
        await using (var wrong = new Peer($"https://{listener.QuicAddress}/raw-extra", "connect", "--token", "Bearer valid", "--expected-status", "404")) await wrong.Pass();
        var rejected = Assert.Single(seen); Assert.Equal("cap=a&cap=b", rejected.Metadata["aster.webtransport.query"]); Assert.Equal("/raw", rejected.Metadata["aster.webtransport.path"]); Assert.Equal("CONNECT", rejected.Method);
        using (var cancelled = new CancellationTokenSource(TimeSpan.FromMilliseconds(20))) await Assert.ThrowsAnyAsync<OperationCanceledException>(() => listener.AcceptAsync(cancelled.Token));
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(25));
        await using var peer = new Peer($"https://{listener.QuicAddress}/raw", "echo", "--token", "Bearer valid");
        using var session = await listener.AcceptAsync(timeout.Token); Assert.Equal("alice", session.Info.Principal); Assert.Equal("viewer", session.Info.Attributes["portal.role"]);
        await Echo(session, timeout.Token); await peer.Pass();
    }
    [Fact]
    public async Task CancellationPreservesReadTailResetServerBiAndCloseWake()
    {
        using var runtime = new Runtime(); using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(25)); var ct = timeout.Token;
        await using var listener = await RawWTListener.StartPublicAsync(runtime, Options(), ct);
        await using var peer = new Peer($"https://{listener.QuicAddress}/raw", "lifecycle");
        using var session = await listener.AcceptAsync(ct); using var pending = await session.AcceptBiAsync(ct);
        Assert.Equal("pending"u8.ToArray(), await pending.ReadAsync(1024, ct));
        using (var cancel = new CancellationTokenSource(TimeSpan.FromMilliseconds(20))) await Assert.ThrowsAnyAsync<OperationCanceledException>(() => pending.ReadAsync(1024, cancel.Token));
        session.SendDatagram("continue"u8); Assert.Equal("-tail"u8.ToArray(), await ReadAll(pending, ct));
        using var reset = await session.AcceptBiAsync(ct); Assert.Equal("reset-me"u8.ToArray(), await reset.ReadAsync(1024, ct));
        await reset.ResetAsync(7, ct); await reset.StopAsync(9, ct); Assert.False(await reset.TransportAckedAsync(ct));
        using var bi = await session.OpenBiAsync(ct); await bi.WriteAsync("server-bi"u8.ToArray(), ct); await bi.FinishAsync(ct);
        Assert.Equal("SERVER-BI"u8.ToArray(), await ReadAll(bi, ct)); Assert.True(await bi.TransportAckedAsync(ct));
        Assert.Empty(await session.ReceiveDatagramAsync(ct)); Assert.Equal("lifecycle"u8.ToArray(), await session.ReceiveDatagramAsync(ct));
        Assert.Throws<IrohException>(() => session.SendDatagram(new byte[checked((int)session.MaxDatagramSize!.Value + 1)]));
        var blocked = session.AcceptUniAsync(ct);
        session.SendDatagram("done"u8); Assert.Equal("ready-close"u8.ToArray(), await session.ReceiveDatagramAsync(ct)); session.CloseConnection(42, "test complete"u8);
        await Assert.ThrowsAsync<IrohException>(() => blocked.WaitAsync(TimeSpan.FromSeconds(2)));
        Assert.Equal(1, (await session.ClosedAsync(ct)).Version); await peer.Pass();
    }
    [Fact]
    public async Task ConfigurationAndPrecancelFailWithoutOwners()
    {
        using var runtime = new Runtime(); var options = Options(); using var canceled = new CancellationTokenSource(); canceled.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => RawWTListener.StartPublicAsync(runtime, options, canceled.Token));
        await Assert.ThrowsAsync<IrohException>(() => RawWTListener.StartPublicAsync(runtime, options with { Path = "/aster/wt" }));
        await Assert.ThrowsAsync<ArgumentException>(() => RawWTListener.StartPublicAsync(runtime, options with { MaxPendingAuth = 1 }));
        await using var upcalls = Upcalls.Create(runtime, 8);
        await Assert.ThrowsAsync<InvalidOperationException>(() => RawWTListener.StartAsync(runtime, options, upcalls));
    }
}
