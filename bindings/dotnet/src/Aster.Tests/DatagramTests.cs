using System;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Tests;

/// <summary>
/// Datagram Tests (5b.6)
///
/// Tests datagram send/receive operations on connections.
/// Uses real native library with real Runtime/Endpoint/Connection.
/// </summary>
public class DatagramTests : IDisposable
{
    private const string ALPN = "dgram-test";
    private static readonly TimeSpan Timeout = TimeSpan.FromSeconds(10);

    // Keep runtimes alive for the lifetime of the test
    private Runtime? _runtimeA;
    private Runtime? _runtimeB;

    private async Task<(Connection connA, Connection connB)> CreateConnectedPair(
        CancellationTokenSource cts)
    {
        _runtimeA = new Runtime();
        _runtimeB = new Runtime();

        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);

        var epA = await Endpoint.CreateAsync(_runtimeA, config);
        var epB = await Endpoint.CreateAsync(_runtimeB, config);

        string epAId = epA.NodeId();

        var acceptTask = epA.AcceptAsync();
        await Task.Delay(50);

        var connB = await epB.ConnectAsync(epAId, ALPN, cts.Token);
        var connA = await acceptTask;

        return (connA, connB);
    }

    private static void CloseConnection(Connection conn)
    {
        try
        {
            conn.Close();
        }
        catch (IrohException ex) when (ex.Status == Status.NotFound || ex.Status == Status.AlreadyClosed)
        {
            // Connection already closed — that's fine
        }
    }

    public void Dispose()
    {
        _runtimeA?.Dispose();
        _runtimeB?.Dispose();
    }

    [Fact]
    public async Task SendDatagram_FireAndForget()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (connA, connB) = await CreateConnectedPair(cts);

        try
        {
            // Send datagram (fire and forget)
            byte[] data = Encoding.UTF8.GetBytes("Datagram!");
            connB.SendDatagram(data);

            // Give it time to arrive
            await Task.Delay(50);
        }
        finally
        {
            CloseConnection(connB);
            CloseConnection(connA);
        }
    }

    [Fact]
    public async Task SendDatagram_ReceiveDatagram()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (connA, connB) = await CreateConnectedPair(cts);

        try
        {
            // B sends datagram
            byte[] data = Encoding.UTF8.GetBytes("Hello Datagram!");
            connB.SendDatagram(data);

            // A receives datagram
            byte[] received = await connA.ReadDatagramAsync(cts.Token);

            Assert.Equal("Hello Datagram!", Encoding.UTF8.GetString(received));
        }
        finally
        {
            CloseConnection(connB);
            CloseConnection(connA);
        }
    }

    [Fact]
    public async Task SendMultipleDatagrams()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (connA, connB) = await CreateConnectedPair(cts);

        try
        {
            // B sends multiple datagrams
            for (int i = 0; i < 3; i++)
            {
                byte[] data = Encoding.UTF8.GetBytes($"Datagram {i}");
                connB.SendDatagram(data);
            }

            // A receives them (order not guaranteed for datagrams)
            for (int i = 0; i < 3; i++)
            {
                byte[] received = await connA.ReadDatagramAsync(cts.Token);
                Assert.NotEmpty(received);
            }
        }
        finally
        {
            CloseConnection(connB);
            CloseConnection(connA);
        }
    }

    [Fact]
    public async Task EmptyDatagram()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (connA, connB) = await CreateConnectedPair(cts);

        try
        {
            connB.SendDatagram(Array.Empty<byte>());
            byte[] received = await connA.ReadDatagramAsync(cts.Token);
            Assert.NotNull(received);
            Assert.Empty(received);
        }
        finally
        {
            CloseConnection(connB);
            CloseConnection(connA);
        }
    }

    [Fact]
    public async Task Datagram_LargeData()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (connA, connB) = await CreateConnectedPair(cts);

        try
        {
            // 1KB datagram
            byte[] largeData = new byte[1024];
            new Random(42).NextBytes(largeData);
            connB.SendDatagram(largeData);

            byte[] received = await connA.ReadDatagramAsync(cts.Token);
            Assert.Equal(largeData, received);
        }
        finally
        {
            CloseConnection(connB);
            CloseConnection(connA);
        }
    }
}
