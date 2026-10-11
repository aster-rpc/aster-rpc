using System.Buffers.Binary;
using System.Text;

namespace Aster.Tests;

public class DocsBatchTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    [Fact]
    public async Task Native_Ordered_Duplicates_Missing_Empty_And_Large()
    {
        await using var node = await Node.StartAsync(Loopback);
        using var doc = await node.Docs().CreateAsync();
        string author = await node.Docs().CreateAuthorAsync();
        byte[] small = [0, 255, 1, 0, 9];
        string hash = await doc.SetBytesDurableAsync(author, "small", small);
        byte[] large = Encoding.UTF8.GetBytes(string.Concat(Enumerable.Repeat("large local value\0", 100_000)));
        string largeHash = await doc.SetBytesDurableAsync(author, "large", large);
        using var scope = node.Blobs().CreateCustodyScope();
        string empty = await scope.AddBytesAsync(ReadOnlyMemory<byte>.Empty);
        string missing = new('a', 64);
        var values = await doc.ReadEntryContentsIfCompleteAsync([missing, hash, empty, largeHash, hash, missing]);
        Assert.Equal(6, values.Count);
        Assert.Null(values[0]);
        Assert.Null(values[5]);
        Assert.NotNull(values[2]);
        Assert.Empty(values[2]!);
        Assert.Equal(small, values[1]);
        Assert.Equal(large, values[3]);
        Assert.Equal(small, values[4]);
        values[1]![0] = 42;
        Assert.Equal(small, values[4]);
        Assert.Equal(small, Assert.Single(await doc.ReadEntryContentsIfCompleteAsync([hash])));
        Assert.Empty(await doc.ReadEntryContentsIfCompleteAsync([]));
    }

    [Fact]
    public async Task Native_Local_Only_And_Custody_Gc()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var provider = await Node.StartAsync(Loopback, cancellationToken: ct);
        await using var node = await Node.StartAsync(Loopback, cancellationToken: ct);
        var (remote, _) = await provider.Blobs().AddBytesAsync("available remotely but not fetched"u8.ToArray(), ct);
        node.AddNodeAddr(provider.NodeAddr());
        using var doc = await node.Docs().CreateAsync(ct);
        Assert.Null(Assert.Single(await doc.ReadEntryContentsIfCompleteAsync([remote], ct)));
        Assert.Equal(BlobStatus.NotFound, node.Blobs().GetStatus(remote).Status);
        using var scope = node.Blobs().CreateCustodyScope();
        byte[] data = Encoding.UTF8.GetBytes(string.Concat(Enumerable.Repeat("guarded local export", 100_000)));
        string hash = await scope.AddBytesAsync(data, ct: ct);
        await node.Blobs().GcRunOnceAsync(ct);
        var values = await doc.ReadEntryContentsIfCompleteAsync([hash], ct);
        Assert.Equal(data, Assert.Single(values));
        scope.Dispose();
        await node.Blobs().GcRunOnceAsync(ct);
        Assert.Null(Assert.Single(await doc.ReadEntryContentsIfCompleteAsync([hash], ct)));
        Assert.Equal(data, values[0]);
    }

    [Fact]
    public async Task Native_Invalid_Cancellation_And_Retry()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var node = await Node.StartAsync(Loopback, cancellationToken: ct);
        using var doc = await node.Docs().CreateAsync(ct);
        using var scope = node.Blobs().CreateCustodyScope();
        byte[] data = Encoding.UTF8.GetBytes(string.Concat(Enumerable.Repeat("cancel local batch", 100_000)));
        string hash = await scope.AddBytesAsync(data, ct: ct);
        foreach (string bad in new[] { "", "bad", new string('g', 64) })
            await Assert.ThrowsAsync<ArgumentException>(() => doc.ReadEntryContentsIfCompleteAsync([hash, bad], ct));
        await Assert.ThrowsAsync<ArgumentNullException>(() => doc.ReadEntryContentsIfCompleteAsync(null!, ct));
        using var canceled = new CancellationTokenSource();
        canceled.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => doc.ReadEntryContentsIfCompleteAsync([hash], canceled.Token));
        string[] hashes = Enumerable.Repeat(hash, 8).ToArray();
        int canceledCount = 0;
        for (int i = 0; i < 8; i++)
        {
            using var racing = CancellationTokenSource.CreateLinkedTokenSource(ct);
            var pending = doc.ReadEntryContentsIfCompleteAsync(hashes, racing.Token);
            racing.Cancel();
            try
            {
                var values = await pending;
                Assert.Equal(hashes.Length, values.Count);
                Assert.Equal(data, values[0]);
            }
            catch (OperationCanceledException) { canceledCount++; }
            Assert.Equal(data, Assert.Single(await doc.ReadEntryContentsIfCompleteAsync([hash], ct)));
        }
        Assert.True(canceledCount > 0, "Must exercise cancellation rather than only already-completed batches.");
        while (node.Runtime.PendingOperationCount != 0) await Task.Delay(1, ct);
        doc.Dispose();
        var closed = await Assert.ThrowsAsync<IrohException>(() => doc.ReadEntryContentsIfCompleteAsync([], ct));
        Assert.Equal(Status.NotFound, closed.Status);
    }

    [Fact]
    public void Decoder_Rejects_Truncation_Count_Presence_Overflow_And_Trailing_Bytes()
    {
        using var stream = new MemoryStream();
        using (var writer = new BinaryWriter(stream, Encoding.UTF8, true))
        {
            writer.Write(4u);
            writer.Write((byte)0);
            foreach (byte[] value in new byte[][] { [], [0, 255, 7], [8] })
            {
                writer.Write((byte)1);
                writer.Write((ulong)value.Length);
                writer.Write(value);
            }
        }
        byte[] payload = stream.ToArray();
        var values = DocContentsBatch.Decode(payload, 4);
        Assert.Null(values[0]);
        Assert.NotNull(values[1]);
        Assert.Empty(values[1]!);
        Assert.Equal(new byte[] { 0, 255, 7 }, values[2]);
        payload[^1] = 99;
        Assert.Equal(new byte[] { 8 }, values[3]);
        for (int n = 0; n < payload.Length; n++) Assert.Throws<IrohException>(() => DocContentsBatch.Decode(payload[..n], 4));
        Assert.Throws<IrohException>(() => DocContentsBatch.Decode([.. payload, 0], 4));
        Assert.Throws<IrohException>(() => DocContentsBatch.Decode(payload, 3));
        Assert.Throws<IrohException>(() => DocContentsBatch.Decode([1, 0, 0, 0, 2], 1));
        byte[] oversized = new byte[13];
        BinaryPrimitives.WriteUInt32LittleEndian(oversized, 1);
        oversized[4] = 1;
        BinaryPrimitives.WriteUInt64LittleEndian(oversized.AsSpan(5), ulong.MaxValue);
        Assert.Throws<IrohException>(() => DocContentsBatch.Decode(oversized, 1));
        byte[] impossibleCount = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(impossibleCount, int.MaxValue);
        Assert.Throws<IrohException>(() => DocContentsBatch.Decode(impossibleCount, int.MaxValue));
        Assert.Empty(DocContentsBatch.Decode([0, 0, 0, 0], 0));
        Assert.Equal(119u, (uint)EventKind.DocContentsBatch);
    }
}
