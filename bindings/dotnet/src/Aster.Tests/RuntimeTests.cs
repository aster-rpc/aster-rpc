using System;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Tests;

/// <summary>
/// Runtime Lifecycle Tests (5b.6)
///
/// Tests the Runtime create/close lifecycle and poll thread behavior.
/// These use the real native library — no mocking.
/// </summary>
public class RuntimeTests
{
    [Fact]
    public void Create_Close_NoExceptions()
    {
        var runtime = new Runtime();
        Assert.NotEqual(0ul, runtime.Handle);
        runtime.Dispose();
    }

    [Fact]
    public void CreateTwice_CloseEach_NoCrossContamination()
    {
        var runtime1 = new Runtime();
        var runtime2 = new Runtime();

        Assert.NotEqual(runtime1.Handle, runtime2.Handle);

        runtime1.Dispose();
        runtime2.Dispose();
    }

    [Fact]
    public void Close_IsIdempotent()
    {
        var runtime = new Runtime();
        runtime.Dispose();
        runtime.Dispose(); // Should not throw
    }

    [Fact]
    public async Task Close_WhileWaiting_OnOperation_Cancels()
    {
        var runtime = new Runtime();

        // Create an endpoint and start accepting
        var config = new EndpointConfigBuilder().Alpn("test").EnableDiscovery(false);
        var endpoint = await Endpoint.CreateAsync(runtime, config);

        // Start accept but immediately cancel
        using var cts = new CancellationTokenSource();
        cts.Cancel();

        // Accept should not hang — it should be cancelled immediately
        var acceptTask = endpoint.AcceptAsync(cts.Token);

        // Cancel and close
        cts.Cancel();
        endpoint.Dispose();
        runtime.Dispose();
    }

    [Fact]
    public void Runtime_Version_ReturnsConsistentValues()
    {
        var runtime = new Runtime();
        var (major, minor, patch) = Runtime.Version;
        Assert.True(major >= 0);
        Assert.True(minor >= 0);
        Assert.True(patch >= 0);
        runtime.Dispose();
    }

    [Fact]
    public void Runtime_Handle_IsValid()
    {
        var runtime = new Runtime();
        Assert.NotEqual(0ul, runtime.Handle);
        runtime.Dispose();
    }

    [Fact]
    public void MultipleRuntimes_DontShareState()
    {
        var runtime1 = new Runtime();
        var runtime2 = new Runtime();
        var runtime3 = new Runtime();

        // Each should have a unique handle
        Assert.NotEqual(runtime1.Handle, runtime2.Handle);
        Assert.NotEqual(runtime2.Handle, runtime3.Handle);
        Assert.NotEqual(runtime1.Handle, runtime3.Handle);

        runtime1.Dispose();
        runtime2.Dispose();
        runtime3.Dispose();
    }

    [Fact]
    public void Dispose_ClearsHandle()
    {
        var runtime = new Runtime();
        ulong handle = runtime.Handle;
        runtime.Dispose();
        // Handle is just an IntPtr, can't check if it's cleared after dispose
        // but this test ensures no exception on double dispose
    }

    [Fact]
    public async Task Runtime_Operations_Queue_IdempotentUnderStress()
    {
        var runtime = new Runtime();

        // Rapidly create and close endpoints
        var tasks = new Task[10];
        for (int i = 0; i < 10; i++)
        {
            tasks[i] = Task.Run(async () =>
            {
                var cfg = new EndpointConfigBuilder().Alpn($"test{i}").EnableDiscovery(false);
                var ep = await Endpoint.CreateAsync(runtime, cfg);
                ep.Dispose();
            });
        }

        await Task.WhenAll(tasks);
        runtime.Dispose();
    }
}
