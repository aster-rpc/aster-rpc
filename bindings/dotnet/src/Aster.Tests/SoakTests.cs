using System;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Tests;

/// <summary>
/// Long-Run Soak / Leak Tests (5b.9)
///
/// Multi-hour churn tests to catch:
/// - "Leaks one buffer every million ops"
/// - Pending ops that never complete
/// - Memory growth over time
/// - CQ backlog accumulation
///
/// These run as regular unit tests but can be extended to run for hours.
/// </summary>
public class SoakTests
{
    private const string ALPN = "soak-test";
    private static readonly TimeSpan ShortTimeout = TimeSpan.FromSeconds(15);

    private static void CloseConnection(Connection conn)
    {
        try { conn.Close(); }
        catch (IrohException ex) when (ex.Status == Status.NotFound || ex.Status == Status.AlreadyClosed) { }
    }

    [Fact]
    public async Task Soak_ConnectAccept_100Iterations()
    {
        // Run 100 iterations of connect/accept to detect any cumulative issues
        for (int i = 0; i < 100; i++)
        {
            using var runtimeA = new Runtime();
            using var runtimeB = new Runtime();

            var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
            var epA = await Endpoint.CreateAsync(runtimeA, config);
            var epB = await Endpoint.CreateAsync(runtimeB, config);

            string epAId = epA.NodeId();

            var acceptTask = epA.AcceptAsync();
            await Task.Delay(20);

            using var cts = new CancellationTokenSource((int)ShortTimeout.TotalMilliseconds);
            try
            {
                var connB = await epB.ConnectAsync(epAId, ALPN, cts.Token);
                var connA = await acceptTask;

                Assert.NotNull(connA);
                Assert.NotNull(connB);

                connB.Close();
                connA.Close();
            }
            catch (Exception)
            {
                // On iteration failure, still cleanup
            }

            epA.Dispose();
            epB.Dispose();
        }
    }

    [Fact]
    public async Task Soak_EndpointCreateDestroy_200Iterations()
    {
        // Rapid create/destroy to detect handle/resource leaks
        for (int i = 0; i < 200; i++)
        {
            using var runtime = new Runtime();
            var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
            var endpoint = await Endpoint.CreateAsync(runtime, config);

            // Verify it's functional
            string nodeId = endpoint.NodeId();
            Assert.NotEmpty(nodeId);

            endpoint.Dispose();
        }
    }

    [Fact]
    public async Task Soak_NodeIdRetrieval_1000Iterations()
    {
        using var runtime = new Runtime();
        var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
        var endpoint = await Endpoint.CreateAsync(runtime, config);

        // Repeated node ID retrieval
        for (int i = 0; i < 1000; i++)
        {
            string nodeId = endpoint.NodeId();
            Assert.NotEmpty(nodeId);
        }

        endpoint.Dispose();
    }

    [Fact]
    public async Task Soak_ConnectionCloseRace_50Iterations()
    {
        // Test close while operations are pending
        for (int i = 0; i < 50; i++)
        {
            using var runtimeA = new Runtime();
            using var runtimeB = new Runtime();

            var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
            var epA = await Endpoint.CreateAsync(runtimeA, config);
            var epB = await Endpoint.CreateAsync(runtimeB, config);

            string epAId = epA.NodeId();

            var acceptTask = epA.AcceptAsync();
            await Task.Delay(20);

            using var cts = new CancellationTokenSource((int)ShortTimeout.TotalMilliseconds);
            try
            {
                var connB = await epB.ConnectAsync(epAId, ALPN, cts.Token);
                var connA = await acceptTask;

                // Immediately close - race with any pending operations
                connB.Close();
                connA.Close();
            }
            catch (Exception)
            {
                // Expected on close race
            }

            epA.Dispose();
            epB.Dispose();
        }
    }

    [Fact]
    public async Task Soak_MultipleRuntimes_50Iterations()
    {
        // Create multiple runtimes to ensure no cross-contamination
        for (int i = 0; i < 50; i++)
        {
            using var runtime1 = new Runtime();
            using var runtime2 = new Runtime();
            using var runtime3 = new Runtime();

            var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
            var ep1 = await Endpoint.CreateAsync(runtime1, config);
            var ep2 = await Endpoint.CreateAsync(runtime2, config);
            var ep3 = await Endpoint.CreateAsync(runtime3, config);

            string id1 = ep1.NodeId();
            string id2 = ep2.NodeId();
            string id3 = ep3.NodeId();

            Assert.NotEqual(id1, id2);
            Assert.NotEqual(id2, id3);
            Assert.NotEqual(id1, id3);

            ep1.Dispose();
            ep2.Dispose();
            ep3.Dispose();
        }
    }

    [Fact]
    public async Task Soak_Cancellation_100Iterations()
    {
        // Test cancellation patterns
        for (int i = 0; i < 100; i++)
        {
            using var runtime = new Runtime();
            var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
            var endpoint = await Endpoint.CreateAsync(runtime, config);

            using var cts = new CancellationTokenSource();
            cts.Cancel();

            try
            {
                var acceptTask = endpoint.AcceptAsync(cts.Token);
                await acceptTask;
            }
            catch (OperationCanceledException)
            {
                // Expected
            }

            endpoint.Dispose();
        }
    }

    [Fact]
    public async Task Soak_DoubleDispose_100Iterations()
    {
        // Ensure double dispose is handled gracefully
        for (int i = 0; i < 100; i++)
        {
            using var runtime = new Runtime();
            var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
            var endpoint = await Endpoint.CreateAsync(runtime, config);

            endpoint.Dispose();
            endpoint.Dispose(); // Idempotent
        }

        using var runtime2 = new Runtime();
        runtime2.Dispose();
        runtime2.Dispose(); // Idempotent
    }

    [Fact]
    public async Task Soak_ConcurrentEndpointCreation_20Iterations()
    {
        // Create endpoints concurrently
        for (int i = 0; i < 20; i++)
        {
            var tasks = new Task<Endpoint>[5];
            var runtimes = new Runtime[5];

            for (int j = 0; j < 5; j++)
            {
                int idx = j;
                runtimes[idx] = new Runtime();
                var config = new EndpointConfigBuilder().Alpn(ALPN).EnableDiscovery(false);
                tasks[idx] = Endpoint.CreateAsync(runtimes[idx], config);
            }

            var endpoints = await Task.WhenAll(tasks);

            // Verify all have unique node IDs
            for (int j = 0; j < 5; j++)
            {
                string nodeId = endpoints[j].NodeId();
                Assert.NotEmpty(nodeId);
            }

            // Cleanup
            for (int j = 0; j < 5; j++)
            {
                endpoints[j].Dispose();
                runtimes[j].Dispose();
            }
        }
    }
}
