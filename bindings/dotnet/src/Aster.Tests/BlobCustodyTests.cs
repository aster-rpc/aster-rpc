using System.Text;

namespace Aster.Tests;

public class BlobCustodyTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    private static unsafe string Hash(byte[] data)
    {
        byte* output = stackalloc byte[64];
        UIntPtr len = 64;
        fixed (byte* input = data)
            Native.Check(Native.aster_blake3_hex(input, (UIntPtr)data.Length, output, &len), "aster_blake3_hex");
        return Encoding.UTF8.GetString(new ReadOnlySpan<byte>(output, (int)len));
    }

    private static async Task Gc(Node node, string hash, bool exists)
    {
        await node.Blobs().GcRunOnceAsync();
        Assert.Equal(exists ? BlobStatus.Complete : BlobStatus.NotFound, node.Blobs().GetStatus(hash).Status);
    }

    [Fact]
    public async Task Native_Multiple_Roots_Copy_And_No_Automatic_Tags()
    {
        await using var node = await Node.StartAsync(Loopback);
        using var scope = node.Blobs().CreateCustodyScope();
        byte[] data = [0, 255, 7, 0, 9];
        string hash = Hash(data);
        await scope.ProtectAsync(hash); // Protect before local content exists.
        Assert.Equal(hash, await scope.AddBytesAsync(data));
        string dir = Directory.CreateTempSubdirectory("aster-custody-").FullName;
        try
        {
            string path = Path.Combine(dir, "source ü.bin");
            byte[] fileData = "copied file"u8.ToArray();
            await File.WriteAllBytesAsync(path, fileData);
            string fileHash = await scope.AddPathAsync(path);
            await File.WriteAllBytesAsync(path, [99]);
            File.Delete(path);
            await Gc(node, hash, true);
            await Gc(node, fileHash, true);
            Assert.Equal(fileData, await scope.ReadAsync(fileHash));
            Assert.Equal(0ul, await node.Tags().DeletePrefixAsync(""));
            scope.Dispose();
            scope.Dispose();
            await Assert.ThrowsAsync<ObjectDisposedException>(() => scope.ReadAsync(hash));
            await Gc(node, hash, false);
            await Gc(node, fileHash, false);
        }
        finally { Directory.Delete(dir, true); }
    }

    [Fact]
    public async Task Native_HashSeq_Graph_And_Exact_Named_Retention()
    {
        await using var node = await Node.StartAsync(Loopback);
        using var children = node.Blobs().CreateCustodyScope();
        string child = await children.AddBytesAsync("HashSeq child"u8.ToArray());
        using var rootScope = node.Blobs().CreateCustodyScope();
        byte[] sequence = Convert.FromHexString(child);
        string root = await rootScope.AddBytesAsync(sequence, BlobFormat.HashSeq);
        children.Dispose();
        await Gc(node, child, true);
        Assert.Equal(sequence, await rootScope.ReadAsync(root));
        await rootScope.RetainAsync(root, "verified/graph", BlobFormat.HashSeq);
        rootScope.Dispose();
        await Gc(node, child, true);
        Assert.Equal(1ul, await node.Tags().DeletePrefixAsync(""));
        await Gc(node, root, false);
        await Gc(node, child, false);
    }

    [Fact]
    public async Task Native_Verified_Bytes_And_Path_Mismatch_Preserves_Tag()
    {
        await using var node = await Node.StartAsync(Loopback);
        byte[] old = "old retained content"u8.ToArray();
        string hash = Hash(old);
        const string tag = "verified/ü";
        Assert.Equal(hash, await node.Blobs().AddBytesVerifiedAsync(old, hash, tag));
        byte[] wrong = "mismatch content"u8.ToArray();
        var mismatch = await Assert.ThrowsAsync<IrohException>(() => node.Blobs().AddBytesVerifiedAsync(wrong, hash, tag));
        Assert.Equal(Status.VerificationFailed, mismatch.Status);
        await Gc(node, hash, true);
        await Gc(node, Hash(wrong), false);
        string dir = Directory.CreateTempSubdirectory("aster-verified-").FullName;
        try
        {
            string path = Path.Combine(dir, "source.bin");
            byte[] replacement = "new copied content"u8.ToArray();
            await File.WriteAllBytesAsync(path, replacement);
            string newHash = Hash(replacement);
            Assert.Equal(newHash, await node.Blobs().AddPathVerifiedAsync(path, newHash, tag));
            File.Delete(path);
            await Gc(node, hash, false);
            await Gc(node, newHash, true);
            Assert.Equal(1ul, await node.Tags().DeletePrefixAsync(""));
            await Gc(node, newHash, false);
        }
        finally { Directory.Delete(dir, true); }
    }

    [Fact]
    public async Task Native_Scoped_Fetch_Protection_And_Report()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var provider = await Node.StartAsync(Loopback, cancellationToken: ct);
        await using var fetcher = await Node.StartAsync(Loopback, cancellationToken: ct);
        byte[] data = Encoding.UTF8.GetBytes(string.Concat(Enumerable.Repeat("scoped fetch", 10_000)));
        var (hash, _) = await provider.Blobs().AddBytesAsync(data, ct);
        var addr = provider.NodeAddr();
        for (int i = 0; i < 100 && addr.DirectAddresses.Count == 0; i++) { await Task.Delay(50, ct); addr = provider.NodeAddr(); }
        fetcher.AddNodeAddr(addr);
        using var scope = fetcher.Blobs().CreateCustodyScope();
        await scope.ProtectAsync(hash, ct: ct);
        var result = await scope.DownloadHashFromAsync(hash, [provider.NodeId()], ct: ct);
        Assert.Equal(data, result.Data);
        Assert.Equal(provider.NodeId(), Assert.Single(result.Report.Providers).NodeId);
        Assert.True(result.Report.BytesTransferred > 0);
        await Gc(fetcher, hash, true);
        scope.Dispose();
        await Gc(fetcher, hash, false);
    }

    [Fact]
    public async Task Native_Accepted_Work_Survives_Close_And_Cancel_Collects_Commit()
    {
        await using var node = await Node.StartAsync(Loopback);
        byte[] data = new byte[16 << 20];
        new Random(42).NextBytes(data);
        string hash = Hash(data);
        using var scope = node.Blobs().CreateCustodyScope();
        var accepted = scope.AddBytesAsync(data); // Native lease is acquired before this returns.
        scope.Dispose();
        Assert.Equal(hash, await accepted);
        await Gc(node, hash, false);
        int canceled = 0;
        for (int i = 0; i < 12; i++)
        {
            using var cts = new CancellationTokenSource();
            var pending = node.Blobs().AddBytesVerifiedAsync(data, hash, "race/commit", ct: cts.Token);
            cts.Cancel();
            try { Assert.Equal(hash, await pending); }
            catch (OperationCanceledException)
            {
                canceled++;
                Assert.Equal(0ul, await node.Tags().DeletePrefixAsync("race/"));
            }
            await node.Tags().DeletePrefixAsync("race/");
            await Gc(node, hash, false);
        }
        Assert.True(canceled > 0, "Must exercise cooperative cancellation, not just committed successes.");

        // Submit directly so cancellation is registered only after native tag
        // commit is observable, without consuming the import's terminal event.
        ulong op;
        using (var native = new NativeScope())
            Native.Check(Native.aster_blobs_add_bytes_verified(node.Runtime.Handle, node.Handle.Value,
                native.Bytes(data), native.Utf8(hash), 0, native.Utf8("late/commit"), 0, out op), "aster_blobs_add_bytes_verified");
        using var deadline = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        while (await node.Tags().DeletePrefixAsync("late/", deadline.Token) == 0) await Task.Delay(1, deadline.Token);
        using var late = new CancellationTokenSource();
        late.Cancel();
        byte[] committed = await BlobCustodyScope.Complete(node.Runtime, op, EventKind.BlobAdded, "late commit", late.Token);
        Assert.Equal(hash, Encoding.UTF8.GetString(committed));
    }

    [Fact]
    public async Task Native_Verified_Restart_And_Invalid_Inputs()
    {
        string dir = Directory.CreateTempSubdirectory("aster-custody-restart-").FullName;
        try
        {
            var config = Loopback with { StoragePath = dir };
            byte[] durable = "persisted verified root"u8.ToArray();
            string durableHash = Hash(durable);
            string emptyHash = Hash([]);
            await using (var node = await Node.StartAsync(config))
            {
                Assert.Equal(durableHash, await node.Blobs().AddBytesVerifiedAsync(durable, durableHash, "restart/object"));
                using var temporary = node.Blobs().CreateCustodyScope();
                Assert.Equal(emptyHash, await temporary.AddBytesAsync(ReadOnlyMemory<byte>.Empty));
                using var canceled = new CancellationTokenSource();
                canceled.Cancel();
                await Assert.ThrowsAnyAsync<OperationCanceledException>(() => node.Blobs().AddBytesVerifiedAsync(new byte[] { 1 }, emptyHash, "bad/canceled", ct: canceled.Token));
                await Assert.ThrowsAsync<IrohException>(() => node.Blobs().AddBytesVerifiedAsync(ReadOnlyMemory<byte>.Empty, "bad", "bad/hash"));
                await Assert.ThrowsAsync<IrohException>(() => node.Blobs().AddPathVerifiedAsync("", emptyHash, "bad/path"));
                await Assert.ThrowsAsync<IrohException>(() => node.Blobs().AddBytesVerifiedAsync(ReadOnlyMemory<byte>.Empty, emptyHash, ""));
                await Assert.ThrowsAsync<IrohException>(() => node.Blobs().AddBytesVerifiedAsync(ReadOnlyMemory<byte>.Empty, emptyHash, "bad/format", (BlobFormat)99));
            }
            await using (var node = await Node.StartAsync(config))
            {
                await Gc(node, durableHash, true);
                Assert.Equal(durable, await node.Blobs().ReadRangeAsync(durableHash, 0, (ulong)durable.Length));
                Assert.Equal(1ul, await node.Tags().DeletePrefixAsync(""));
            }
        }
        finally { Directory.Delete(dir, true); }
    }
}
