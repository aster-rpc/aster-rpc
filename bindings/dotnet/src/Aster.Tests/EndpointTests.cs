using System;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Tests;

/// <summary>
/// Endpoint Integration Tests (5b.6)
///
/// Tests real endpoint lifecycle: create, connect, accept, close.
/// These use the real native library — no mocking.
/// </summary>
public class EndpointTests : IDisposable
{
    private const string ALPN = "test-alpn";
    private static readonly TimeSpan Timeout = TimeSpan.FromSeconds(10);

    // Keep runtimes alive for the lifetime of the test
    private Runtime? _runtimeA;
    private Runtime? _runtimeB;

    private async Task<(Endpoint epA, Endpoint epB, string epAId)> CreateEndpoints(
        CancellationTokenSource cts)
    {
        _runtimeA = new Runtime();
        _runtimeB = new Runtime();

        var configA = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var configB = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);

        var epA = await Endpoint.CreateAsync(_runtimeA, configA);
        var epB = await Endpoint.CreateAsync(_runtimeB, configB);

        return (epA, epB, epA.NodeId());
    }

    private async Task<(Connection connA, Connection connB)> CreateConnectedPair(
        CancellationTokenSource cts)
    {
        var (epA, epB, epAId) = await CreateEndpoints(cts);

        var acceptTask = epA.AcceptAsync();
        await Task.Delay(50);

        var connB = await epB.ConnectAsync(epAId, ALPN, cts.Token);
        var connA = await acceptTask;

        return (connA, connB);
    }

    public void Dispose()
    {
        _runtimeA?.Dispose();
        _runtimeB?.Dispose();
    }

    [Fact]
    public async Task Create_NodeId_IsConsistent()
    {
        using var runtime = new Runtime();
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var endpoint = await Endpoint.CreateAsync(runtime, config);

        string nodeId1 = endpoint.NodeId();
        string nodeId2 = endpoint.NodeId();

        Assert.Equal(nodeId1, nodeId2);
        Assert.NotEmpty(nodeId1);
        Assert.True(nodeId1.Length >= 32); // 32 hex chars for 16 bytes

        endpoint.Dispose();
    }

    [Fact]
    public async Task Create_TwoEndpoints_HaveUniqueNodeIds()
    {
        using var runtimeA = new Runtime();
        using var runtimeB = new Runtime();

        var configA = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var configB = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);

        var epA = await Endpoint.CreateAsync(runtimeA, configA);
        var epB = await Endpoint.CreateAsync(runtimeB, configB);

        string idA = epA.NodeId();
        string idB = epB.NodeId();

        Assert.NotEqual(idA, idB);

        epA.Dispose();
        epB.Dispose();
    }

    [Fact]
    public async Task Connect_Accept_Completes()
    {
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var (epA, epB, epAId) = await CreateEndpoints(cts);

        // A accepts in background
        var acceptTask = epA.AcceptAsync();
        await Task.Delay(50);

        // B connects to A
        var connB = await epB.ConnectAsync(epAId, ALPN, cts.Token);

        // Wait for A
        var connA = await acceptTask;

        Assert.NotNull(connA);
        Assert.NotNull(connB);

        // Cleanup
        connB.Close();
        connA.Close();
        epA.Dispose();
        epB.Dispose();
    }

    [Fact]
    public async Task Accept_Cancel_BeforeConnect_ReturnsCanceled()
    {
        using var runtime = new Runtime();
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var ep = await Endpoint.CreateAsync(runtime, config);

        using var cts = new CancellationTokenSource();
        cts.Cancel();

        var acceptTask = ep.AcceptAsync(cts.Token);

        try
        {
            await acceptTask;
            Assert.Fail("Expected OperationCanceledException");
        }
        catch (OperationCanceledException)
        {
            // Expected
        }

        ep.Dispose();
    }

    [Fact]
    public async Task Close_Endpoint_WhilePendingAccept_IsClean()
    {
        using var runtime = new Runtime();
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var ep = await Endpoint.CreateAsync(runtime, config);

        // Start accept without timeout
        var acceptTask = ep.AcceptAsync();

        // Close endpoint while accept is pending
        ep.Dispose();
        runtime.Dispose();

        // acceptTask should complete (possibly with an error, but not hang)
        try
        {
            await acceptTask;
        }
        catch
        {
            // Any exception is acceptable — just shouldn't hang
        }
    }

    [Fact]
    public async Task NodeId_IsValidHexString()
    {
        using var runtime = new Runtime();
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var ep = await Endpoint.CreateAsync(runtime, config);

        string nodeId = ep.NodeId();

        // Should be a valid hex string of reasonable length (32-128 chars)
        Assert.True(nodeId.Length >= 32, $"NodeId too short: {nodeId.Length}");
        Assert.True(nodeId.Length <= 128, $"NodeId too long: {nodeId.Length}");

        foreach (char c in nodeId)
        {
            Assert.True(char.IsAsciiHexDigit(c), $"Invalid hex char: {c}");
        }

        ep.Dispose();
    }

    [Fact]
    public async Task TwoConnections_Independent()
    {
        using var runtimeA = new Runtime();
        using var runtimeB = new Runtime();
        using var runtimeC = new Runtime();

        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);

        var epA = await Endpoint.CreateAsync(runtimeA, config);
        var epB = await Endpoint.CreateAsync(runtimeB, config);
        var epC = await Endpoint.CreateAsync(runtimeC, config);

        string epAId = epA.NodeId();

        // B connects to A
        var acceptTask1 = epA.AcceptAsync();
        await Task.Delay(50);
        using var cts = new CancellationTokenSource((int)Timeout.TotalMilliseconds);
        var connB = await epB.ConnectAsync(epAId, ALPN, cts.Token);
        var connA1 = await acceptTask1;

        // C connects to A
        var acceptTask2 = epA.AcceptAsync();
        await Task.Delay(50);
        var connC = await epC.ConnectAsync(epAId, ALPN, cts.Token);
        var connA2 = await acceptTask2;

        // Both connections should be independent
        Assert.NotNull(connA1);
        Assert.NotNull(connA2);
        Assert.NotNull(connB);
        Assert.NotNull(connC);

        // Close all
        connB.Close();
        connC.Close();
        connA1.Close();
        connA2.Close();
        epA.Dispose();
        epB.Dispose();
        epC.Dispose();
    }
}
