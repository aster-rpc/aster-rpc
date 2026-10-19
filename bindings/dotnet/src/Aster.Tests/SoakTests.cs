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
/// These run as regular unit tests but can be extended to run for hours. Endpoints are
/// loopback-only (<see cref="LoopbackEndpoints"/>): no relay, no n0 discovery, dialled by address,
/// so an iteration costs milliseconds and a failure fails the test instead of hiding in a timeout.
/// </summary>
public class SoakTests
{
    private const string ALPN = "soak-test";
    private static readonly TimeSpan ShortTimeout = TimeSpan.FromSeconds(15);

    [Fact(Timeout = 300_000)]
    public async Task Soak_ConnectAccept_100Iterations()
    {
        // Run 100 iterations of connect/accept to detect any cumulative issues
        for (int i = 0; i < 100; i++)
        {
            using var runtimeA = new Runtime();
            using var runtimeB = new Runtime();

            var config = LoopbackEndpoints.Config(ALPN);
            var epA = await Endpoint.CreateAsync(runtimeA, config);
            var epB = await Endpoint.CreateAsync(runtimeB, config);

            using var cts = new CancellationTokenSource(ShortTimeout);
            var acceptTask = epA.AcceptAsync(cts.Token);
            var connB = await LoopbackEndpoints.ConnectAsync(epB, epA, ALPN, cts.Token);
            var connA = await acceptTask;

            Assert.NotNull(connA);
            Assert.NotNull(connB);

            connB.Close();
            connA.Close();

            epA.Dispose();
            epB.Dispose();
        }
    }

    [Fact(Timeout = 300_000)]
    public async Task Soak_DoubleDispose_100Iterations()
    {
        // Ensure double dispose is handled gracefully
        for (int i = 0; i < 100; i++)
        {
            using var runtime = new Runtime();
            var config = LoopbackEndpoints.Config(ALPN);
            var endpoint = await Endpoint.CreateAsync(runtime, config);

            endpoint.Dispose();
            endpoint.Dispose(); // Idempotent
        }

        using var runtime2 = new Runtime();
        runtime2.Dispose();
        runtime2.Dispose(); // Idempotent
    }
}
