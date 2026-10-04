using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Expose;
using Aster.Upcall;

namespace Aster.Tests;

/// <summary>C ABI 4.3 streaming consumer relay over real QUIC (ports of ffi/tests/expose.rs relay_stream_*).</summary>
public class ExposeRelayTests
{
    private static readonly TimeSpan T = TimeSpan.FromSeconds(30);

    private static byte[] Pattern(int n) => Enumerable.Range(0, n).Select(i => (byte)(i * 31 % 251)).ToArray();

    private sealed class Handlers
    {
        public ConcurrentQueue<string> Log { get; } = new();
        public TaskCompletionSource Gate { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public bool Has(Func<string, bool> p) => Log.Any(p);

        public async ValueTask<ExposeHttpResponse> Handle(ExposeHttpRequest req)
        {
            switch (req.Uri)
            {
                case "/early":
                    // Answers without reading the body: an early 413 while the upload is still going.
                    return ExposeHttpResponse.Text(413, "too big");
                case "/gate":
                    Log.Enqueue("start /gate");
                    await DrainAsync(req);
                    await Gate.Task.WaitAsync(req.Cancellation);
                    return ExposeHttpResponse.Text(200, "gate");
                case "/duplex":
                    return ExposeHttpResponse.Streaming(200, Duplex(req));
                default:
                    return ExposeHttpResponse.Streaming(200, Duplex(req), [new HttpHeader("x-uri", req.Uri)]);
            }
        }

        private async Task DrainAsync(ExposeHttpRequest req)
        {
            var buf = new byte[16 * 1024];
            // A reset before the head either fails the read or cancels the (abandoned) upcall: both are "the body failed".
            try { while (await req.Body.ReadAsync(buf, req.Cancellation) > 0) { } }
            catch (Exception) { Log.Enqueue("read err"); throw; }
        }

        private async IAsyncEnumerable<ReadOnlyMemory<byte>> Duplex(ExposeHttpRequest req)
        {
            var buf = new byte[64 * 1024];
            while (true)
            {
                int n;
                try { n = await req.Body.ReadAsync(buf, req.Cancellation); }
                catch (Exception e) when (e is not OperationCanceledException) { Log.Enqueue("read err"); throw; }
                if (n == 0) break;
                yield return buf.AsMemory(0, n).ToArray();
            }
            Log.Enqueue("duplex done");
        }
    }

    private static async Task<(ExposeRig Rig, Handlers H)> Setup()
    {
        var rig = await ExposeRig.CreateAsync();
        var h = new Handlers();
        rig.Registry.ExposeHttp(ExposeRig.Svc, rig.Up, rig.Bodies, h.Handle);
        return (rig, h);
    }

    private static async Task Eventually(Func<bool> cond, string what)
    {
        var end = DateTime.UtcNow + T;
        while (!cond())
        {
            Assert.True(DateTime.UtcNow < end, "timed out: " + what);
            await Task.Delay(10);
        }
    }

    private static async IAsyncEnumerable<ReadOnlyMemory<byte>> Pieces(byte[] data, int size)
    {
        for (int off = 0; off < data.Length; off += size)
        {
            yield return data.AsMemory(off, Math.Min(size, data.Length - off));
            await Task.Yield();
        }
    }

    private static async Task<byte[]> ReadAll(Stream s)
    {
        using var ms = new MemoryStream();
        await s.CopyToAsync(ms).WaitAsync(T);
        return ms.ToArray();
    }

    [Fact]
    public async Task Round_Trip_Streams_Both_Ways_Under_Backpressure()
    {
        var (rig, _) = await Setup();
        await using var _r = rig;
        byte[] data = Pattern(3 * 1024 * 1024 + 17);
        await using (var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "POST", "/echo",
            [new HttpHeader("host", "app.local")], Pieces(data, 100_000), new ExposeRelayOptions { MaxChunk = 64 * 1024, UploadCapacity = 2 }).WaitAsync(T))
        {
            Assert.Equal(200, resp.Status);
            Assert.Contains(resp.Headers, h => h.Name == "x-uri" && h.ValueText == "/echo");
            Assert.Equal(data, await ReadAll(resp.Body));
            Assert.True(await resp.Upload.WaitAsync(T));
            // The end is sticky.
            Assert.Equal(0, await resp.Body.ReadAsync(new byte[10]));
        }
        await rig.WaitClosed();

        // An empty upload, and a Stream source.
        await using (var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "POST", "/echo").WaitAsync(T))
        {
            Assert.Equal(200, resp.Status);
            Assert.Empty(await ReadAll(resp.Body));
        }
        await using (var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "POST", "/echo", null, new MemoryStream(Pattern(300_000))).WaitAsync(T))
            Assert.Equal(Pattern(300_000), await ReadAll(resp.Body));
        await rig.WaitClosed();
    }

    [Fact]
    public async Task Failures_Have_Stable_Codes_And_Denied_Is_Permission_Denied()
    {
        var (rig, h) = await Setup();
        await using var _r = rig;

        var unknown = await Assert.ThrowsAsync<ExposeRelayException>(() => ExposeClient.RelayStreamAsync(rig.Conn, "nope", "PUT", "/", null, Pieces(new byte[10], 10)).WaitAsync(T));
        Assert.Equal(HttpBodyStatus.UnknownService, unknown.RelayStatus);
        Assert.Equal(Status.NotFound, unknown.Status);
        Assert.Contains("not exposed", unknown.Message);

        // Denied: the relay code and the reserved PERMISSION_DENIED status, streaming and buffered alike.
        rig.Allow = false;
        var denied = await Assert.ThrowsAsync<ExposeRelayException>(() => ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "PUT", "/echo").WaitAsync(T));
        Assert.Equal(HttpBodyStatus.Denied, denied.RelayStatus);
        Assert.Equal(Status.PermissionDenied, denied.Status);
        var buffered = await Assert.ThrowsAsync<IrohException>(() => rig.Relay());
        Assert.Equal(Status.PermissionDenied, buffered.Status);
        rig.Allow = true;

        // A source that fails before the head: the caller's own exception surfaces, the handler sees a failed body.
        var boom = new InvalidOperationException("source broke");
        var err = await Assert.ThrowsAsync<InvalidOperationException>(() => ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "PUT", "/gate", null, Failing(boom)).WaitAsync(T));
        Assert.Same(boom, err);
        await Eventually(() => h.Has(l => l == "read err"), "handler saw a failed body");

        // Cancelled before the head: OCE, nothing leaks.
        using var cts = new CancellationTokenSource();
        var pending = ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "PUT", "/gate", null, (IAsyncEnumerable<ReadOnlyMemory<byte>>?)null, null, cts.Token);
        await Eventually(() => h.Log.Count(l => l == "start /gate") >= 2, "gate handler started");
        cts.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => pending.WaitAsync(T));
        h.Gate.SetResult();
        await rig.WaitClosed();

        static async IAsyncEnumerable<ReadOnlyMemory<byte>> Failing(Exception e)
        {
            yield return new byte[1000];
            await Task.Delay(50);
            throw e;
        }
    }

    [Fact]
    public async Task Connection_Closed_While_Awaiting_The_Head()
    {
        var (rig, h) = await Setup();
        await using var _r = rig;
        var pending = ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "PUT", "/gate");
        await Eventually(() => h.Has(l => l == "start /gate"), "handler");
        await Task.Delay(100);
        rig.Conn.Close("bye");
        var ex = await Assert.ThrowsAsync<ExposeRelayException>(() => pending.WaitAsync(T));
        Assert.Equal(HttpBodyStatus.ConnectionClosed, ex.RelayStatus);
        Assert.Equal(Status.AlreadyClosed, ex.Status);
        h.Gate.SetResult();
    }

    [Fact]
    public async Task Early_Head_Arrives_While_The_Upload_Is_Still_Going_Then_The_Upload_Stops()
    {
        var (rig, _) = await Setup();
        await using var _r = rig;
        int sent = 0;
        async IAsyncEnumerable<ReadOnlyMemory<byte>> Endless([EnumeratorCancellation] CancellationToken ct = default)
        {
            var chunk = new byte[64 * 1024];
            while (true) { Interlocked.Increment(ref sent); yield return chunk; await Task.Yield(); }
        }
        await using var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "PUT", "/early", null, Endless()).WaitAsync(T);
        Assert.Equal(413, resp.Status);
        Assert.Equal("too big", Encoding.UTF8.GetString(await ReadAll(resp.Body)));
        // The handler never reads: the upload ends "peer stopped reading", not as a failure.
        Assert.False(await resp.Upload.WaitAsync(T));
        Assert.True(sent > 0);
    }

    [Fact]
    public async Task Disposing_Before_The_End_Resets_The_Upload()
    {
        var (rig, h) = await Setup();
        await using var _r = rig;
        int ended = 0;
        async IAsyncEnumerable<ReadOnlyMemory<byte>> Slow()
        {
            try
            {
                yield return Encoding.UTF8.GetBytes("one");
                while (true) { await Task.Delay(10); yield return Encoding.UTF8.GetBytes("more"); }
            }
            finally { Interlocked.Exchange(ref ended, 1); }
        }
        var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "POST", "/duplex", null, Slow()).WaitAsync(T);
        var buf = new byte[16];
        int n = await resp.Body.ReadAsync(buf).AsTask().WaitAsync(T);
        Assert.StartsWith("one", Encoding.UTF8.GetString(buf, 0, n));
        await resp.DisposeAsync();
        // The pump stopped, the handler saw a failed body (never a clean end).
        Assert.Equal(1, Volatile.Read(ref ended));
        await Eventually(() => h.Has(l => l == "read err"), "handler saw the reset");
        Assert.False(h.Has(l => l == "duplex done"));
        await rig.WaitClosed();
    }

    [Fact]
    public async Task A_Body_Read_To_Its_Clean_End_Lets_The_Upload_Finish()
    {
        var (rig, h) = await Setup();
        await using var _r = rig;
        var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "POST", "/duplex", null, Pieces(Encoding.UTF8.GetBytes("three"), 5)).WaitAsync(T);
        Assert.Equal("three", Encoding.UTF8.GetString(await ReadAll(resp.Body)));
        await resp.DisposeAsync(); // waits for the upload to finish
        Assert.True(await resp.Upload);
        await Eventually(() => h.Has(l => l == "duplex done"), "handler done");
        await rig.WaitClosed();
    }

    [Fact]
    public async Task A_Source_Failure_After_The_Head_Is_Reported_On_Upload_Not_Thrown()
    {
        var (rig, h) = await Setup();
        await using var _r = rig;
        var boom = new InvalidOperationException("late failure");
        var gate = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        async IAsyncEnumerable<ReadOnlyMemory<byte>> Source()
        {
            yield return Encoding.UTF8.GetBytes("a");
            await gate.Task;
            throw boom;
        }
        await using var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "POST", "/duplex", null, Source()).WaitAsync(T);
        Assert.Equal(200, resp.Status); // the call returned at the head
        var buf = new byte[8];
        Assert.Equal(1, await resp.Body.ReadAsync(buf).AsTask().WaitAsync(T));
        gate.SetResult();
        Assert.Same(boom, await Assert.ThrowsAsync<InvalidOperationException>(() => resp.Upload.WaitAsync(T)));
        // The handler's body read failed, so its response fails: never a clean end.
        await Assert.ThrowsAsync<HttpBodyException>(async () => await ReadAll(resp.Body));
    }

    [Fact]
    public async Task Concurrent_Streaming_Relays()
    {
        var (rig, _) = await Setup();
        await using var _r = rig;
        var calls = Enumerable.Range(0, 20).Select(async i =>
        {
            byte[] data = Pattern(50_000 + i * 20_000).Select(b => (byte)(b ^ i)).ToArray();
            await using var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "POST", "/echo", null, Pieces(data, 30_000)).WaitAsync(T);
            Assert.Equal(data, await ReadAll(resp.Body));
        });
        await Task.WhenAll(calls).WaitAsync(TimeSpan.FromSeconds(90));
        await rig.WaitClosed();
    }

    [Fact]
    public async Task Disposal_Is_Idempotent_And_Reads_After_Dispose_Fail()
    {
        var (rig, _) = await Setup();
        await using var _r = rig;
        var resp = await ExposeClient.RelayStreamAsync(rig.Conn, ExposeRig.Svc, "POST", "/echo", null, Pieces(new byte[100], 100)).WaitAsync(T);
        await resp.DisposeAsync();
        await resp.DisposeAsync();
        resp.Dispose();
        await Assert.ThrowsAsync<ObjectDisposedException>(async () => await resp.Body.ReadAsync(new byte[4]));
    }
}
