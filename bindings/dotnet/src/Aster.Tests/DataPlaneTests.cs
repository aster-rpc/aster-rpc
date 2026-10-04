using System;
using System.Linq;
using System.Text;
using System.Threading.Tasks;
using Aster.Crypto;

namespace Aster.Tests;

/// <summary>ABI 3.3 data-plane wrappers on loopback nodes.</summary>
public class DataPlaneTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    [Fact]
    public async Task Blobs_ReadRange_Status_Tags_Gc()
    {
        await using var node = await Node.StartAsync(Loopback);
        var blobs = node.Blobs();
        byte[] data = Enumerable.Range(0, 10_000).Select(i => (byte)(i % 256)).ToArray();
        var (hash, size) = await blobs.AddBytesAsync(data);
        Assert.Equal(10_000ul, size);
        Assert.Equal(data.AsSpan(100, 50).ToArray(), await blobs.ReadRangeAsync(hash, 100, 50));
        Assert.Equal((BlobStatus.Complete, 10_000ul), blobs.GetStatus(hash));

        var tags = node.Tags();
        await tags.SetAsync("keep/a", hash);
        await tags.SetAsync("keep/b", hash);
        await tags.SetAsync("other", hash);
        Assert.Equal(2ul, await tags.DeletePrefixAsync("keep/"));
        await blobs.GcRunOnceAsync();
    }

    [Fact]
    public async Task Blobs_Download_From_Providers_With_Report()
    {
        await using var provider = await Node.StartAsync(Loopback);
        await using var fetcher = await Node.StartAsync(Loopback);
        byte[] data = Encoding.UTF8.GetBytes(string.Concat(Enumerable.Repeat("aster ", 2000)));
        var (hash, _) = await provider.Blobs().AddBytesAsync(data);
        // The fetcher dials the provider by id: teach it the provider's addresses first.
        var addr = provider.NodeAddr();
        for (int i = 0; i < 100 && addr.DirectAddresses.Count == 0; i++) { await Task.Delay(50); addr = provider.NodeAddr(); }
        fetcher.AddNodeAddr(addr);

        var result = await fetcher.Blobs().DownloadHashFromAsync(hash, [provider.NodeId()]).WaitAsync(TimeSpan.FromSeconds(30));
        Assert.Equal(data, result.Data);
        var stats = Assert.Single(result.Report.Providers);
        Assert.Equal(provider.NodeId(), stats.NodeId);
        Assert.True(result.Report.BytesTransferred > 0);
        Assert.Equal(data.AsSpan(0, 10).ToArray(), await fetcher.Blobs().ReadRangeAsync(hash, 0, 10));
    }

    [Fact]
    public async Task Docs_Import_Authority_Durable_Entry_Status()
    {
        await using var node = await Node.StartAsync(Loopback);
        var docs = node.Docs();
        byte[] secret = Ids.GenerateSecretKey();
        string nsId = Ids.Format(IdKind.NamespaceId, Ids.NamespaceId(secret));

        Assert.Null(await docs.OpenAsync(nsId));
        using var doc = await docs.ImportWriteNamespaceAsync(secret);
        Assert.Equal(nsId, doc.Id());

        var report = await docs.AuthorityReportAsync(nsId);
        Assert.Equal(LocalAccess.Write, report.Access);
        Assert.Equal(Ids.NamespaceId(secret), report.NamespaceId);
        Assert.Equal(Docs.AuthorityFingerprint(nsId, LocalAccess.Write), report.Fingerprint);

        string author = await docs.CreateAuthorAsync();
        Assert.False(await doc.EntryExistsAsync(author, "k"));
        string contentHash = await doc.SetBytesDurableAsync(author, "k", "v"u8.ToArray());
        Assert.True(await doc.EntryExistsAsync(author, "k"));
        Assert.Equal((BlobStatus.Complete, 1ul), await doc.ContentStatusAsync(contentHash));

        using var reopened = await docs.OpenAsync(nsId);
        Assert.NotNull(reopened);

        var retire = await docs.RetireLocalWriteAuthorityAsync(nsId);
        Assert.Equal(RetireOutcome.Busy, retire.Outcome);
    }
}
