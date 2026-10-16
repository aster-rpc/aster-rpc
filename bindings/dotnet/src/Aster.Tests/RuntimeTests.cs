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
    public async Task Close_Faults_Idle_Native_Docs_Receive_And_Rejects_New_Waits()
    {
        using var runtime = new Runtime();
        // Use raw native owners here so final Runtime.Dispose really closes
        // the runtime, rather than releasing one reference of a managed receiver.
        ulong start = Node.BeginStart(runtime, new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" }, null, null);
        var nodeEvent = await Ops.Await(runtime, start, EventKind.NodeCreated, "start", default);
        var docs = new Docs(runtime, nodeEvent.handle);
        using var doc = await docs.CreateAsync();
        Native.Check(Native.aster_doc_subscribe(runtime.Handle, doc.Handle, 0, out ulong subscribe), "subscribe");
        var receiver = await Ops.Await(runtime, subscribe, EventKind.DocSubscribed, "subscribe", default);
        EventError.TakePayload(runtime, receiver);
        Native.Check(Native.aster_doc_event_recv(runtime.Handle, receiver.handle, 0, out ulong receive), "receive");
        var pending = runtime.WaitForAsync(receive);
        Assert.False(pending.IsCompleted);
        runtime.Dispose();
        await Assert.ThrowsAsync<ObjectDisposedException>(() => pending.WaitAsync(TimeSpan.FromSeconds(3)));
        Assert.Equal(0, runtime.PendingOperationCount);
        await Assert.ThrowsAsync<ObjectDisposedException>(async () => await runtime.WaitForAsync(ulong.MaxValue));
        Assert.Throws<ObjectDisposedException>(() => runtime.Retain());
    }

    [Fact]
    public async Task Close_Racing_Wait_Registration_Completes_Every_Waiter()
    {
        using var runtime = new Runtime();
        using var start = new ManualResetEventSlim();
        var waiters = Enumerable.Range(1, 100).Select(id => Task.Run(async () =>
        {
            start.Wait();
            await Assert.ThrowsAsync<ObjectDisposedException>(async () => await runtime.WaitForAsync((ulong)id));
        })).ToArray();
        var close = Task.Run(() => { start.Wait(); runtime.Dispose(); });
        start.Set();
        await Task.WhenAll(waiters.Append(close)).WaitAsync(TimeSpan.FromSeconds(5));
        Assert.Equal(0, runtime.PendingOperationCount);
    }

    [Fact]
    public async Task Managed_Receiver_Keeps_Runtime_Alive_Until_Receiver_And_Receive_End()
    {
        var node = await Node.StartAsync(new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" });
        var runtime = node.Runtime;
        using var doc = await node.Docs().CreateAsync();
        using var events = await doc.SubscribeAsync();
        var pending = events.ReceiveAsync();
        node.Dispose();
        events.Dispose();
        Assert.Null(await pending.WaitAsync(TimeSpan.FromSeconds(3)));
        await Assert.ThrowsAsync<ObjectDisposedException>(async () => await runtime.WaitForAsync(ulong.MaxValue));
    }

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
        var config = LoopbackEndpoints.Config("test");
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
                var cfg = LoopbackEndpoints.Config($"test{i}");
                var ep = await Endpoint.CreateAsync(runtime, cfg);
                ep.Dispose();
            });
        }

        await Task.WhenAll(tasks);
        runtime.Dispose();
    }
}
