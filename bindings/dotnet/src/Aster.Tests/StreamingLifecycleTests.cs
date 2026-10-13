using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster.Tests;

public class StreamingLifecycleTests
{
    private sealed class ParkedRequests : IAsyncEnumerable<EchoRequest>, IAsyncEnumerator<EchoRequest>
    {
        public readonly TaskCompletionSource<bool> Entered = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource<bool> Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public int Disposals;
        private int _next;
        public EchoRequest Current => new() { Message = "first" };
        public IAsyncEnumerator<EchoRequest> GetAsyncEnumerator(CancellationToken cancellationToken = default) => this;
        public async ValueTask<bool> MoveNextAsync()
        {
            if (Interlocked.Increment(ref _next) == 1) return true;
            Entered.TrySetResult(true);
            return await Release.Task;
        }
        public ValueTask DisposeAsync() { Interlocked.Increment(ref Disposals); return ValueTask.CompletedTask; }
    }

    [Theory]
    [InlineData(false, false)]
    [InlineData(false, true)]
    [InlineData(true, false)]
    [InlineData(true, true)]
    public async Task Early_Response_Settles_While_Request_Producer_Is_Parked(bool bidi, bool reject)
    {
        var requests = new ParkedRequests();
        var delivered = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        async ValueTask<EchoReply> Answer(IAsyncEnumerable<EchoRequest> reqs, ServerCallContext ctx)
        {
            await using var iterator = reqs.GetAsyncEnumerator();
            Assert.True(await iterator.MoveNextAsync());
            Assert.Equal("first", iterator.Current.Message);
            delivered.TrySetResult(true);
            await requests.Entered.Task;
            if (reject) throw new RpcError(StatusCode.InvalidArgument, "early rejection");
            return new EchoReply { Message = "early answer" };
        }
        async IAsyncEnumerable<EchoReply> Bidi(IAsyncEnumerable<EchoRequest> reqs, ServerCallContext ctx)
        {
            yield return await Answer(reqs, ctx);
        }
        var service = ServiceDefinition.Create("Lifecycle", 1)
            .ClientStream<EchoRequest, EchoReply>("upload", Answer)
            .BidiStream<EchoRequest, EchoReply>("chat", Bidi)
            .Unary<EchoRequest, EchoReply>("echo", (req, _) => ValueTask.FromResult(new EchoReply { Message = req.Message }))
            .Build();
        var config = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = config, Services = { service } });
        await using var client = await AsterClient.StartAsync(config);
        client.RegisterTypes(service);
        await using var conn = await client.ConnectAsync(await server.TicketAsync());
        async Task<EchoReply> ConsumeBidi()
        {
            EchoReply? last = null;
            await foreach (var reply in conn.BidiStreamAsync<EchoRequest, EchoReply>("Lifecycle", "chat", requests)) last = reply;
            return last!;
        }
        var result = bidi ? ConsumeBidi() : conn.ClientStreamAsync<EchoRequest, EchoReply>("Lifecycle", "upload", requests);
        try
        {
            await delivered.Task.WaitAsync(TimeSpan.FromSeconds(3));
            await requests.Entered.Task.WaitAsync(TimeSpan.FromSeconds(3));
            // The actual native dispatch call has sent its terminal response and gone.
            await Task.Run(async () => { while (server.LiveCalls != 0) await Task.Delay(5); }).WaitAsync(TimeSpan.FromSeconds(3));
            if (reject)
            {
                var error = await Assert.ThrowsAsync<RpcError>(() => result.WaitAsync(TimeSpan.FromSeconds(2)));
                Assert.Equal(StatusCode.InvalidArgument, error.Code);
            }
            else Assert.Equal("early answer", (await result.WaitAsync(TimeSpan.FromSeconds(2))).Message);
            Assert.Equal(0, requests.Disposals); // no concurrent Dispose/MoveNext
            if (reject) requests.Release.TrySetException(new InvalidOperationException("late source failure"));
            else requests.Release.TrySetResult(true); // late value must never be sent
            await Task.Run(async () => { while (requests.Disposals == 0) await Task.Delay(5); }).WaitAsync(TimeSpan.FromSeconds(3));
            Assert.Equal(1, requests.Disposals);
            Assert.Equal(0UL, server.LiveCalls);
            Assert.Equal("after", (await conn.UnaryAsync<EchoRequest, EchoReply>("Lifecycle", "echo", new EchoRequest { Message = "after" }).WaitAsync(TimeSpan.FromSeconds(3))).Message);
        }
        finally
        {
            requests.Release.TrySetResult(false);
            try { await result.WaitAsync(TimeSpan.FromSeconds(3)); } catch { }
        }
    }
    private sealed class ThrowingRequests(Exception error, string phase) : IAsyncEnumerable<EchoRequest>, IAsyncEnumerator<EchoRequest>
    {
        private int _next;
        public EchoRequest Current => new() { Message = "first" };
        public IAsyncEnumerator<EchoRequest> GetAsyncEnumerator(CancellationToken cancellationToken = default)
            => phase == "get" ? throw error : this;
        public ValueTask<bool> MoveNextAsync()
        {
            if (++_next == 1) return ValueTask.FromResult(true);
            return phase == "next" ? ValueTask.FromException<bool>(error) : ValueTask.FromResult(false);
        }
        public ValueTask DisposeAsync() => phase == "dispose" ? ValueTask.FromException(error) : ValueTask.CompletedTask;
    }

    [Theory]
    [InlineData(false, "get")]
    [InlineData(true, "get")]
    [InlineData(false, "next")]
    [InlineData(true, "next")]
    [InlineData(false, "dispose")]
    [InlineData(true, "dispose")]
    public async Task Local_Request_Failure_Cancels_And_Preserves_Original_Error(bool bidi, string phase)
    {
        async ValueTask<EchoReply> Hold(IAsyncEnumerable<EchoRequest> requests, ServerCallContext ctx)
        {
            await foreach (var _ in requests) { }
            await Task.Delay(Timeout.Infinite, ctx.CancellationToken);
            return new EchoReply();
        }
        async IAsyncEnumerable<EchoReply> HoldBidi(IAsyncEnumerable<EchoRequest> requests, ServerCallContext ctx)
        { yield return await Hold(requests, ctx); }
        var service = ServiceDefinition.Create("LocalFailure", 1)
            .ClientStream<EchoRequest, EchoReply>("upload", Hold)
            .BidiStream<EchoRequest, EchoReply>("chat", HoldBidi).Build();
        var config = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = config, Services = { service } });
        await using var client = await AsterClient.StartAsync(config);
        client.RegisterTypes(service);
        await using var conn = await client.ConnectAsync(await server.TicketAsync());
        var error = new InvalidOperationException("source " + phase);
        var requests = new ThrowingRequests(error, phase);
        async Task Consume()
        {
            if (bidi) { await foreach (var _ in conn.BidiStreamAsync<EchoRequest, EchoReply>("LocalFailure", "chat", requests)) { } }
            else await conn.ClientStreamAsync<EchoRequest, EchoReply>("LocalFailure", "upload", requests);
        }
        Assert.Same(error, await Assert.ThrowsAsync<InvalidOperationException>(() => Consume().WaitAsync(TimeSpan.FromSeconds(3))));
        await Task.Run(async () => { while (server.LiveCalls != 0) await Task.Delay(5); }).WaitAsync(TimeSpan.FromSeconds(3));
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public async Task Empty_Request_Stream_Is_Refused_Without_Handler_Dispatch(bool bidi)
    {
        int dispatched = 0;
        ValueTask<EchoReply> Upload(IAsyncEnumerable<EchoRequest> _, ServerCallContext ctx)
        { Interlocked.Increment(ref dispatched); return ValueTask.FromResult(new EchoReply()); }
        async IAsyncEnumerable<EchoReply> Chat(IAsyncEnumerable<EchoRequest> requests, ServerCallContext ctx)
        { yield return await Upload(requests, ctx); }
        static async IAsyncEnumerable<EchoRequest> Empty() { await Task.CompletedTask; yield break; }
        var service = ServiceDefinition.Create("Empty", 1).ClientStream<EchoRequest, EchoReply>("upload", Upload)
            .BidiStream<EchoRequest, EchoReply>("chat", Chat)
            .Unary<EchoRequest, EchoReply>("echo", (req, _) => ValueTask.FromResult(new EchoReply { Message = req.Message })).Build();
        var config = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = config, Services = { service } });
        await using var client = await AsterClient.StartAsync(config);
        client.RegisterTypes(service);
        await using var conn = await client.ConnectAsync(await server.TicketAsync());
        async Task Consume()
        {
            if (bidi) { await foreach (var _ in conn.BidiStreamAsync<EchoRequest, EchoReply>("Empty", "chat", Empty())) { } }
            else await conn.ClientStreamAsync<EchoRequest, EchoReply>("Empty", "upload", Empty());
        }
        var error = await Assert.ThrowsAsync<RpcError>(() => Consume().WaitAsync(TimeSpan.FromSeconds(3)));
        // The asynchronous finish/reset race can report native refusal or cancellation.
        // Neither may report OK or dispatch the protected streaming handler.
        Assert.Contains(error.Code, new[] { StatusCode.InvalidArgument, StatusCode.Cancelled });
        Assert.Equal(0, Volatile.Read(ref dispatched));
        Assert.Equal(0UL, server.LiveCalls);
        Assert.Equal("after", (await conn.UnaryAsync<EchoRequest, EchoReply>("Empty", "echo", new EchoRequest { Message = "after" }).WaitAsync(TimeSpan.FromSeconds(3))).Message);
    }

    [Theory]
    [InlineData(false, false)]
    [InlineData(true, false)]
    [InlineData(false, true)]
    [InlineData(true, true)]
    public async Task Cancellation_And_Deadline_Settle_With_Parked_Request_Producer(bool bidi, bool deadline)
    {
        var requests = new ParkedRequests();
        var delivered = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        var cancelled = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        async ValueTask<EchoReply> Hold(IAsyncEnumerable<EchoRequest> source, ServerCallContext ctx)
        {
            await using var e = source.GetAsyncEnumerator();
            Assert.True(await e.MoveNextAsync());
            delivered.TrySetResult(true);
            try { await Task.Delay(Timeout.Infinite, ctx.CancellationToken); }
            catch (OperationCanceledException) { cancelled.TrySetResult(true); throw; }
            return new EchoReply();
        }
        async IAsyncEnumerable<EchoReply> Chat(IAsyncEnumerable<EchoRequest> source, ServerCallContext ctx)
        { yield return await Hold(source, ctx); }
        var service = ServiceDefinition.Create("CancelParked", 1).ClientStream<EchoRequest, EchoReply>("upload", Hold)
            .BidiStream<EchoRequest, EchoReply>("chat", Chat).Build();
        var config = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = config, Services = { service } });
        await using var client = await AsterClient.StartAsync(config);
        client.RegisterTypes(service);
        await using var conn = await client.ConnectAsync(await server.TicketAsync());
        using var ct = new CancellationTokenSource();
        var options = deadline ? new CallOptions { Deadline = TimeSpan.FromMilliseconds(500) } : null;
        async Task Consume()
        {
            if (bidi) { await foreach (var _ in conn.BidiStreamAsync<EchoRequest, EchoReply>("CancelParked", "chat", requests, options, ct.Token)) { } }
            else await conn.ClientStreamAsync<EchoRequest, EchoReply>("CancelParked", "upload", requests, options, ct.Token);
        }
        var result = Consume();
        try
        {
            await delivered.Task.WaitAsync(TimeSpan.FromSeconds(3));
            await requests.Entered.Task.WaitAsync(TimeSpan.FromSeconds(3));
            if (!deadline) ct.Cancel();
            if (deadline)
            {
                var error = await Assert.ThrowsAsync<RpcError>(() => result.WaitAsync(TimeSpan.FromSeconds(3)));
                Assert.Equal(StatusCode.DeadlineExceeded, error.Code);
            }
            else await Assert.ThrowsAnyAsync<OperationCanceledException>(() => result.WaitAsync(TimeSpan.FromSeconds(3)));
            await cancelled.Task.WaitAsync(TimeSpan.FromSeconds(3));
            await Task.Run(async () => { while (server.LiveCalls != 0) await Task.Delay(5); }).WaitAsync(TimeSpan.FromSeconds(3));
            Assert.Equal(0, requests.Disposals);
        }
        finally
        {
            requests.Release.TrySetResult(false);
            try { await result.WaitAsync(TimeSpan.FromSeconds(3)); } catch { }
            await Task.Run(async () => { while (requests.Disposals == 0) await Task.Delay(5); }).WaitAsync(TimeSpan.FromSeconds(3));
        }
        Assert.Equal(1, requests.Disposals);
    }

    [Fact]
    public async Task Abandoning_Bidi_Response_Does_Not_Wait_For_Parked_Producer()
    {
        var requests = new ParkedRequests();
        var cancelled = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        async IAsyncEnumerable<EchoReply> Chat(IAsyncEnumerable<EchoRequest> source, ServerCallContext ctx)
        {
            await using var e = source.GetAsyncEnumerator();
            Assert.True(await e.MoveNextAsync());
            await requests.Entered.Task;
            yield return new EchoReply { Message = "first reply" };
            try { await Task.Delay(Timeout.Infinite, ctx.CancellationToken); }
            catch (OperationCanceledException) { cancelled.TrySetResult(true); throw; }
        }
        var service = ServiceDefinition.Create("Abandon", 1).BidiStream<EchoRequest, EchoReply>("chat", Chat).Build();
        var config = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = config, Services = { service } });
        await using var client = await AsterClient.StartAsync(config);
        client.RegisterTypes(service);
        await using var conn = await client.ConnectAsync(await server.TicketAsync());
        var replies = conn.BidiStreamAsync<EchoRequest, EchoReply>("Abandon", "chat", requests).GetAsyncEnumerator();
        try
        {
            Assert.True(await replies.MoveNextAsync().AsTask().WaitAsync(TimeSpan.FromSeconds(3)));
            Assert.Equal("first reply", replies.Current.Message);
            await replies.DisposeAsync().AsTask().WaitAsync(TimeSpan.FromSeconds(2));
            await cancelled.Task.WaitAsync(TimeSpan.FromSeconds(3));
            await Task.Run(async () => { while (server.LiveCalls != 0) await Task.Delay(5); }).WaitAsync(TimeSpan.FromSeconds(3));
            Assert.Equal(0, requests.Disposals);
        }
        finally
        {
            requests.Release.TrySetResult(false);
            await replies.DisposeAsync();
            await Task.Run(async () => { while (requests.Disposals == 0) await Task.Delay(5); }).WaitAsync(TimeSpan.FromSeconds(3));
        }
        Assert.Equal(1, requests.Disposals);
    }


    [Fact]
    public async Task Flow_Controlled_Send_Cancellation_Drains_Then_Healthy_Call()
    {
        var entered = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        int completed = 0;
        async ValueTask<EchoReply> Hold(IAsyncEnumerable<EchoRequest> requests, ServerCallContext ctx)
        {
            await using var iterator = requests.GetAsyncEnumerator();
            Assert.True(await iterator.MoveNextAsync());
            entered.TrySetResult(true);
            await Task.Delay(Timeout.InfiniteTimeSpan, ctx.CancellationToken);
            return new EchoReply();
        }
        var service = ServiceDefinition.Create("Pressure", 1)
            .ClientStream<EchoRequest, EchoReply>("upload", Hold)
            .Unary<EchoRequest, EchoReply>("echo", (req, _) => ValueTask.FromResult(new EchoReply { Message = req.Message }))
            .Build();
        var config = new AsterConfig { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = config, Services = { service } });
        await using var client = await AsterClient.StartAsync(config);
        client.RegisterTypes(service);
        await using var conn = await client.ConnectAsync(await server.TicketAsync());
        var data = new byte[2 * 1024 * 1024];
        uint state = 0x12345678;
        for (int i = 0; i < data.Length; i++)
        { state ^= state << 13; state ^= state >> 17; state ^= state << 5; data[i] = (byte)state; }
        var request = new EchoRequest { Data = data };
        async IAsyncEnumerable<EchoRequest> Produce()
        {
            await Task.CompletedTask;
            for (int i = 0; i < 32; i++)
            {
                yield return request;
                Interlocked.Increment(ref completed);
            }
        }
        using var cancel = new CancellationTokenSource();
        var result = conn.ClientStreamAsync<EchoRequest, EchoReply>("Pressure", "upload", Produce(), cancellationToken: cancel.Token);
        try
        {
            await entered.Task.WaitAsync(TimeSpan.FromSeconds(3));
            int previous = Volatile.Read(ref completed);
            bool stalled = false;
            for (int i = 0; i < 30; i++)
            {
                await Task.Delay(20);
                int now = Volatile.Read(ref completed);
                if (now == previous && now > 0) { stalled = true; break; }
                previous = now;
            }
            cancel.Cancel();
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => result.WaitAsync(TimeSpan.FromSeconds(3)));
            Assert.True(stalled, "bounded producer never reached backpressure");
            Assert.InRange(Volatile.Read(ref completed), 1, 31);
            Console.WriteLine($"completed payloads before cancellation: {Volatile.Read(ref completed)} (2 MiB each, cap 32)");
            Assert.Equal("after", (await conn.UnaryAsync<EchoRequest, EchoReply>("Pressure", "echo", new EchoRequest { Message = "after" }).WaitAsync(TimeSpan.FromSeconds(3))).Message);
        }
        finally
        {
            cancel.Cancel();
            try { await result.WaitAsync(TimeSpan.FromSeconds(3)); } catch { }
        }
    }
}
