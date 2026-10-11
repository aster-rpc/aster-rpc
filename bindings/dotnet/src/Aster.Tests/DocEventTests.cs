using System.Text;

namespace Aster.Tests;

public class DocEventTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    [Fact]
    public async Task Native_Subscription_Metadata_And_Cancellation_Leave_Receiver_Usable()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var node = await Node.StartAsync(Loopback, cancellationToken: ct);
        using var doc = await node.Docs().CreateAsync(ct);
        string author = await node.Docs().CreateAuthorAsync(ct);
        using var events = await doc.SubscribeAsync(ct);
        for (int i = 0; i < 10; i++)
        {
            using var canceled = new CancellationTokenSource();
            var pending = events.ReceiveAsync(canceled.Token);
            canceled.Cancel();
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => pending.WaitAsync(TimeSpan.FromSeconds(3), ct));
            string key = $"local/{i}";
            await doc.SetBytesAsync(author, key, [(byte)i], ct);
            var ev = await ReadUntil(events, e => e.Kind == DocEventKind.InsertLocal, ct);
            Assert.Equal(author, ev.Entry!.AuthorId);
            Assert.Equal(Encoding.UTF8.GetBytes(key), ev.Entry.Key);
            Assert.Equal(1ul, ev.Entry.ContentLen);
            Assert.True(ev.Entry.Timestamp > 0);
        }
        using var preCanceled = new CancellationTokenSource();
        preCanceled.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => doc.SubscribeAsync(preCanceled.Token));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => events.ReceiveAsync(preCanceled.Token));
        // The receiver retains its own native owner after the Doc handle is freed.
        doc.Dispose();
        var blocked = events.ReceiveAsync(ct);
        events.Dispose();
        Assert.Null(await blocked.WaitAsync(TimeSpan.FromSeconds(3), ct));
        Assert.Null(await events.ReceiveAsync(ct));
        events.Dispose();
    }

    [Fact]
    public async Task Native_Concurrent_Receives_Close_Without_Stranding_Waiters()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var node = await Node.StartAsync(Loopback, cancellationToken: ct);
        using var doc = await node.Docs().CreateAsync(ct);
        using var events = await doc.SubscribeAsync(ct);
        var pending = Enumerable.Range(0, 8).Select(_ => events.ReceiveAsync(ct)).ToArray();
        await events.DisposeAsync();
        foreach (var terminal in await Task.WhenAll(pending).WaitAsync(TimeSpan.FromSeconds(3), ct))
            Assert.Null(terminal);
    }

    [Fact]
    public async Task Native_Join_And_Subscribe_Exposes_Remote_Events_And_Owners()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var provider = await Node.StartAsync(Loopback, cancellationToken: ct);
        await using var consumer = await Node.StartAsync(Loopback, cancellationToken: ct);
        using var source = await provider.Docs().CreateAsync(ct);
        string author = await provider.Docs().CreateAuthorAsync(ct);
        var addr = provider.NodeAddr();
        for (int i = 0; i < 100 && addr.DirectAddresses.Count == 0; i++)
        {
            await Task.Delay(20, ct);
            addr = provider.NodeAddr();
        }
        Assert.NotEmpty(addr.DirectAddresses);
        var joined = await consumer.Docs().JoinAndSubscribeAsync(await source.ShareWithAddrAsync(ct: ct), ct);
        using var doc = joined.Doc;
        using var events = joined.Events;
        Assert.Equal(source.Id(), doc.Id());
        await source.SetBytesAsync(author, "remote/key", "payload"u8.ToArray(), ct);
        var ev = await ReadUntil(events, e => e.Kind == DocEventKind.InsertRemote, ct);
        Assert.Equal(provider.NodeId(), ev.PeerId);
        Assert.Equal(author, ev.Entry!.AuthorId);
        Assert.Equal("remote/key"u8.ToArray(), ev.Entry.Key);
        Assert.Equal(7ul, ev.Entry.ContentLen);
        events.Dispose();
        // Freeing the receiver leaves the document owner intact.
        Assert.Equal(source.Id(), doc.Id());
    }

    [Fact]
    public void Event_Decoder_Uses_Flags_And_Rejects_Invalid_Metadata_And_Tails()
    {
        using var stream = new MemoryStream();
        using var writer = new BinaryWriter(stream);
        foreach (string value in new[] { "author", "key", "hash" })
        {
            byte[] data = Encoding.UTF8.GetBytes(value);
            writer.Write((uint)data.Length);
            writer.Write(data);
        }
        writer.Write(5ul);
        writer.Write(42ul);
        byte[] metadata = stream.ToArray();
        var local = DocEventReceiver.Decode(0, metadata);
        Assert.Equal(DocEventKind.InsertLocal, local.Kind);
        Assert.Equal(42ul, local.Entry!.Timestamp);
        writer.Write(4u);
        writer.Write("peer"u8.ToArray());
        var remote = DocEventReceiver.Decode(1, stream.ToArray());
        Assert.Equal("peer", remote.PeerId);
        Assert.Equal("hash", DocEventReceiver.Decode(2, "hash"u8.ToArray()).ContentHash);
        Assert.Equal(DocEventKind.PendingContentReady, DocEventReceiver.Decode(3, []).Kind);
        foreach (uint subtype in new uint[] { 4, 5, 6 })
            Assert.Equal("peer", DocEventReceiver.Decode(subtype, "peer"u8.ToArray()).PeerId);
        Assert.Throws<IrohException>(() => DocEventReceiver.Decode(99, []));
        Assert.Throws<IrohException>(() => DocEventReceiver.Decode(0, metadata[..^1]));
        Assert.Throws<IrohException>(() => DocEventReceiver.Decode(0, [.. metadata, 1]));
        Assert.Throws<IrohException>(() => DocEventReceiver.Decode(1, metadata));
        Assert.Throws<IrohException>(() => DocEventReceiver.Decode(3, [1]));
    }

    private static async Task<DocEvent> ReadUntil(DocEventReceiver receiver, Func<DocEvent, bool> predicate, CancellationToken ct)
    {
        await foreach (var ev in receiver.ReadAllAsync(ct))
            if (predicate(ev)) return ev;
        throw new Xunit.Sdk.XunitException("unexpected document event EOF");
    }
}
