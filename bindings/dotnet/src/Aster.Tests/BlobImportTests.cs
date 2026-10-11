namespace Aster.Tests;

public class BlobImportTests
{
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    [Fact]
    public async Task Native_Path_Import_Owns_Copy_And_Automatic_Retention()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var node = await Node.StartAsync(Loopback, cancellationToken: ct);
        string dir = Directory.CreateTempSubdirectory("aster-import-").FullName;
        try
        {
            string path = Path.Combine(dir, "source ü.bin");
            byte[] value = [0, 255, 5, 0, 8];
            await File.WriteAllBytesAsync(path, value, ct);
            string hash = await node.Blobs().AddPathAsync(path, ct);
            await File.WriteAllBytesAsync(path, [9], ct);
            File.Delete(path);
            await node.Blobs().GcRunOnceAsync(ct);
            Assert.Equal(value, await node.Blobs().ReadRangeAsync(hash, 0, (ulong)value.Length, ct));
        }
        finally { Directory.Delete(dir, true); }
    }

    [Fact]
    public async Task Native_Named_Path_Import_Replaces_Root_And_Allows_Gc()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var ct = timeout.Token;
        await using var node = await Node.StartAsync(Loopback, cancellationToken: ct);
        string dir = Directory.CreateTempSubdirectory("aster-named-").FullName;
        try
        {
            string path = Path.Combine(dir, "source.bin");
            const string tag = "portal/ü/verified";
            byte[] value = [0, 255, 5, 0, 8];
            await File.WriteAllBytesAsync(path, value, ct);
            string hash = await node.Blobs().AddPathWithNamedTagAsync(path, tag, ct);
            File.Delete(path);
            await node.Blobs().GcRunOnceAsync(ct);
            Assert.Equal(value, await node.Blobs().ReadRangeAsync(hash, 0, (ulong)value.Length, ct));
            await File.WriteAllBytesAsync(path, "replacement"u8.ToArray(), ct);
            string replacement = await node.Blobs().AddPathWithNamedTagAsync(path, tag, ct);
            Assert.NotEqual(hash, replacement);
            await node.Blobs().GcRunOnceAsync(ct);
            Assert.Equal(BlobStatus.NotFound, node.Blobs().GetStatus(hash).Status);
            Assert.Equal(BlobStatus.Complete, node.Blobs().GetStatus(replacement).Status);
            await node.Tags().DeleteAsync(tag, ct);
            await node.Blobs().GcRunOnceAsync(ct);
            Assert.Equal(BlobStatus.NotFound, node.Blobs().GetStatus(replacement).Status);
        }
        finally { Directory.Delete(dir, true); }
    }

    [Fact]
    public async Task Native_Invalid_And_PreCanceled_Imports_Leave_No_Named_Root()
    {
        await using var node = await Node.StartAsync(Loopback);
        string dir = Directory.CreateTempSubdirectory("aster-invalid-").FullName;
        try
        {
            string path = Path.Combine(dir, "empty.bin");
            await File.WriteAllBytesAsync(path, []);
            await Assert.ThrowsAsync<IrohException>(() => node.Blobs().AddPathAsync(""));
            await Assert.ThrowsAsync<IrohException>(() => node.Blobs().AddPathWithNamedTagAsync(path + ".missing", "bad/missing"));
            using var canceled = new CancellationTokenSource();
            canceled.Cancel();
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => node.Blobs().AddPathAsync(path, canceled.Token));
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => node.Blobs().AddPathWithNamedTagAsync(path, "bad/canceled", canceled.Token));
            Assert.Equal(0ul, await node.Tags().DeletePrefixAsync("bad/"));
            string hash = await node.Blobs().AddPathWithNamedTagAsync(path, "valid/empty");
            Assert.Equal((BlobStatus.Complete, 0ul), node.Blobs().GetStatus(hash));
        }
        finally { Directory.Delete(dir, true); }
    }
}
