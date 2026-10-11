using System.Buffers.Binary;
using System.Text;

namespace Aster.Tests;

public class DocsParityTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    [Fact]
    public async Task Native_Query_Preserves_Metadata_And_Deletes_Prefixes()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var node = await Node.StartAsync(Loopback, cancellationToken: ct);
        var docs = node.Docs();
        using var doc = await docs.CreateAsync(ct);
        string author = await docs.CreateAuthorAsync(ct);
        byte[] value = [0, 1, 255, 4];
        const string key = "policy/ü\0record";
        await doc.SetBytesAsync(author, key, value, ct);
        Assert.Equal(value, await doc.GetExactAsync(author, key, ct));
        var entry = Assert.Single(await doc.QueryAsync(DocQueryMode.KeyPrefix, "policy/", ct: ct));
        Assert.Equal(author, entry.AuthorId);
        Assert.Equal(Encoding.UTF8.GetBytes(key), entry.Key);
        Assert.Equal(64, entry.ContentHash.Length);
        Assert.Equal((ulong)value.Length, entry.ContentLen);
        Assert.True(entry.Timestamp > 0);
        Assert.Equal(value, await doc.ReadEntryContentAsync(entry.ContentHash, ct));

        string second = await docs.CreateAuthorAsync(ct);
        await Task.Delay(2, ct);
        await doc.SetBytesAsync(second, key, "latest"u8.ToArray(), ct);
        Assert.Equal(2, (await doc.QueryAsync(DocQueryMode.KeyExact, key, ct: ct)).Count);
        Assert.Single(await doc.QueryAsync(DocQueryMode.KeyExact, key, limit: 1, ct: ct));
        foreach (var mode in new[] { DocQueryMode.LatestExact, DocQueryMode.LatestPrefix })
            Assert.Equal(second, Assert.Single(await doc.QueryAsync(mode, key, ct: ct)).AuthorId);
        Assert.Equal(1ul, await doc.DeleteAsync(author, "policy/", ct));
        Assert.DoesNotContain(await doc.QueryAsync(DocQueryMode.KeyExact, key, ct: ct), e => e.AuthorId == author);
        Assert.Equal(1ul, await doc.DeleteAsync(second, Array.Empty<byte>(), ct));
        Assert.Empty(await doc.QueryAsync(DocQueryMode.KeyPrefix, ct: ct));

        await Assert.ThrowsAsync<ArgumentOutOfRangeException>(() => doc.QueryAsync((DocQueryMode)99, ct: ct));
        await Assert.ThrowsAsync<IrohException>(() => doc.DeleteAsync("invalid", "", ct));
        await Assert.ThrowsAsync<IrohException>(() => doc.StartSyncAsync(["invalid-peer"], ct));
        await Assert.ThrowsAsync<IrohException>(() => doc.ShareWithAddrAsync(99, ct));
    }

    [Fact]
    public async Task Native_Addressed_Ticket_And_Explicit_Peers_Transfer_Updates()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var provider = await Node.StartAsync(Loopback, cancellationToken: ct);
        await using var consumer = await Node.StartAsync(Loopback, cancellationToken: ct);
        using var source = await provider.Docs().CreateAsync(ct);
        string author = await provider.Docs().CreateAuthorAsync(ct);
        await source.SetBytesAsync(author, "manifest/root", "first"u8.ToArray(), ct);
        var addr = provider.NodeAddr();
        for (int i = 0; i < 100 && addr.DirectAddresses.Count == 0; i++)
        {
            await Task.Delay(20, ct);
            addr = provider.NodeAddr();
        }
        Assert.NotEmpty(addr.DirectAddresses);
        using var joined = await consumer.Docs().JoinAsync(await source.ShareWithAddrAsync(ct: ct), ct);
        Assert.Equal(source.Id(), joined.Id());
        await joined.LeaveAsync(ct);
        consumer.AddNodeAddr(addr);
        await joined.StartSyncAsync([addr.EndpointId], ct);
        await WaitValue(joined, author, "first"u8.ToArray(), ct);
        await source.SetBytesAsync(author, "manifest/root", "second"u8.ToArray(), ct);
        await WaitValue(joined, author, "second"u8.ToArray(), ct);
        await joined.LeaveAsync(ct);
        await joined.StartSyncAsync(Array.Empty<string>(), ct);
    }

    [Fact]
    public async Task PreCanceled_Docs_Operations_Do_Not_Mutate()
    {
        await using var node = await Node.StartAsync(Loopback);
        using var doc = await node.Docs().CreateAsync();
        string author = await node.Docs().CreateAuthorAsync();
        await doc.SetBytesAsync(author, "keep", [1]);
        using var canceled = new CancellationTokenSource();
        canceled.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => doc.DeleteAsync(author, "", canceled.Token));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => doc.QueryAsync(DocQueryMode.KeyPrefix, ct: canceled.Token));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => doc.StartSyncAsync([], canceled.Token));
        Assert.Single(await doc.QueryAsync(DocQueryMode.KeyExact, "keep"));
    }

    [Fact]
    public void Metadata_Decoder_Rejects_Truncation_Oversized_Fields_And_Partial_Tails()
    {
        byte[] first = Fixture("first", [0, 255, 1], "hash", ulong.MaxValue, 1234567890123456);
        byte[] second = Fixture("second", [], "other", 0, 42);
        var entries = DocMetadata.Decode([.. first, .. second]);
        Assert.Equal(2, entries.Count);
        Assert.Equal(new byte[] { 0, 255, 1 }, entries[0].Key);
        Assert.Equal(ulong.MaxValue, entries[0].ContentLen);
        Assert.Equal(1234567890123456ul, entries[0].Timestamp);
        Assert.Equal(42ul, entries[1].Timestamp);
        Assert.Empty(DocMetadata.Decode([]));
        for (int n = 1; n < first.Length; n++)
            Assert.Throws<IrohException>(() => DocMetadata.Decode(first[..n]));
        foreach (int offset in new[] { 0, 9, 16 })
        {
            byte[] bad = (byte[])first.Clone();
            BinaryPrimitives.WriteUInt32LittleEndian(bad.AsSpan(offset), uint.MaxValue);
            Assert.Throws<IrohException>(() => DocMetadata.Decode(bad));
        }
        Assert.Throws<IrohException>(() => DocMetadata.Decode([.. first, 1]));
        Assert.Throws<IrohException>(() => DocMetadata.Decode([.. first, .. second[..^1]]));
    }

    private static byte[] Fixture(string author, byte[] key, string hash, ulong length, ulong timestamp)
    {
        using var stream = new MemoryStream();
        using var writer = new BinaryWriter(stream);
        foreach (byte[] field in new[] { Encoding.UTF8.GetBytes(author), key, Encoding.UTF8.GetBytes(hash) })
        {
            writer.Write((uint)field.Length);
            writer.Write(field);
        }
        writer.Write(length);
        writer.Write(timestamp);
        return stream.ToArray();
    }

    private static async Task WaitValue(Doc doc, string author, byte[] expected, CancellationToken ct)
    {
        while (true)
        {
            var entry = (await doc.QueryAsync(DocQueryMode.KeyExact, "manifest/root", ct: ct)).FirstOrDefault();
            if (entry != null)
            {
                try
                {
                    if ((await doc.ReadEntryContentAsync(entry.ContentHash, ct)).SequenceEqual(expected)) return;
                }
                catch (IrohException) { /* metadata can arrive before its blob */ }
            }
            await Task.Delay(20, ct);
        }
    }
}
