using System.Text;

namespace Aster;

public sealed partial class Blobs
{
    /// <summary>Creates temporary protection for multiple typed roots, including roots not present locally. Dispose after verification or named retention.</summary>
    public BlobCustodyScope CreateCustodyScope()
    {
        _runtime.Retain();
        try
        {
            Native.Check(Native.aster_blobs_custody_new(_runtime.Handle, _nodeHandle, out ulong scope), "aster_blobs_custody_new");
            return new BlobCustodyScope(_runtime, scope);
        }
        catch { _runtime.Release(); throw; }
    }

    /// <summary>Copies bytes, verifies the store-computed hash and commits exactly the named tag on a match. No automatic persistent root is created.</summary>
    /// <remarks>Mismatch preserves the previous tag. Cancellation waits for cleanup; once commit begins, actual success or error wins cancellation.</remarks>
    public async Task<string> AddBytesVerifiedAsync(ReadOnlyMemory<byte> data, string expectedHash, string tagName,
        BlobFormat format = BlobFormat.Raw, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(expectedHash);
        ArgumentNullException.ThrowIfNull(tagName);
        _runtime.Retain();
        try
        {
            ulong op;
            using (var s = new NativeScope())
                Native.Check(Native.aster_blobs_add_bytes_verified(_runtime.Handle, _nodeHandle, s.Bytes(data.Span), s.Utf8(expectedHash), (uint)format, s.Utf8(tagName), 0, out op), "aster_blobs_add_bytes_verified");
            return Encoding.UTF8.GetString(await BlobCustodyScope.Complete(_runtime, op, EventKind.BlobAdded, "aster_blobs_add_bytes_verified", ct).ConfigureAwait(false));
        }
        finally { _runtime.Release(); }
    }

    /// <summary>Copies a file, verifies its store-computed hash and commits exactly the named tag on a match. Ownership and cancellation follow AddBytesVerifiedAsync.</summary>
    public async Task<string> AddPathVerifiedAsync(string path, string expectedHash, string tagName,
        BlobFormat format = BlobFormat.Raw, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(path);
        ArgumentNullException.ThrowIfNull(expectedHash);
        ArgumentNullException.ThrowIfNull(tagName);
        _runtime.Retain();
        try
        {
            ulong op;
            using (var s = new NativeScope())
                Native.Check(Native.aster_blobs_add_path_verified(_runtime.Handle, _nodeHandle, s.Utf8(path), s.Utf8(expectedHash), (uint)format, s.Utf8(tagName), 0, out op), "aster_blobs_add_path_verified");
            return Encoding.UTF8.GetString(await BlobCustodyScope.Complete(_runtime, op, EventKind.BlobAdded, "aster_blobs_add_path_verified", ct).ConfigureAwait(false));
        }
        finally { _runtime.Release(); }
    }
}

/// <summary>Owns temporary GC protection for multiple Raw or HashSeq roots. No persistent tag is created until RetainAsync.</summary>
/// <remarks>Disposal seals new submissions; accepted scoped operations retain all guards until native cleanup and terminal delivery finish.</remarks>
public sealed class BlobCustodyScope : IDisposable, IAsyncDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private readonly object _gate = new();
    private bool _disposed;

    internal BlobCustodyScope(Runtime runtime, ulong handle) { _runtime = runtime; _handle = handle; }

    internal static async Task<byte[]> Complete(Runtime runtime, ulong op, EventKind expected, string what, CancellationToken ct)
    {
        // The native operation chooses a single terminal outcome. Keep registration
        // after cancellation so committed success and cleanup cannot be abandoned.
        var terminal = runtime.WaitForAsync(op);
        using var registration = ct.Register(() => Native.aster_operation_cancel(runtime.Handle, op));
        var ev = await terminal.ConfigureAwait(false);
        if (ev.kind == (uint)EventKind.OperationCancelled)
        {
            EventError.TakePayload(runtime, ev);
            throw new OperationCanceledException(ct);
        }
        if (ev.kind != (uint)expected || ev.status != 0) throw EventError.From(runtime, ev, what);
        return EventError.TakePayload(runtime, ev);
    }

    private async Task<byte[]> Run(string what, EventKind expected, Func<ulong> submit, CancellationToken ct)
    {
        ct.ThrowIfCancellationRequested();
        ulong op;
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            _runtime.Retain();
            try { op = submit(); }
            catch { _runtime.Release(); throw; }
        }
        try { return await Complete(_runtime, op, expected, what, ct).ConfigureAwait(false); }
        finally { _runtime.Release(); }
    }

    /// <summary>Protects a typed root even before fetching it. HashSeq includes its graph as children become known; no Collection schema is imposed.</summary>
    public async Task ProtectAsync(string hash, BlobFormat format = BlobFormat.Raw, CancellationToken ct = default)
        => _ = await Run("aster_blob_custody_protect", EventKind.BlobCustodyDone, () =>
        {
            using var s = new NativeScope();
            Native.Check(Native.aster_blob_custody_protect(_runtime.Handle, _handle, s.Utf8(hash), (uint)format, 0, out ulong op), "aster_blob_custody_protect");
            return op;
        }, ct).ConfigureAwait(false);

    /// <summary>Copies bytes into temporary custody without an automatic tag.</summary>
    public async Task<string> AddBytesAsync(ReadOnlyMemory<byte> data, BlobFormat format = BlobFormat.Raw, CancellationToken ct = default)
        => Encoding.UTF8.GetString(await Run("aster_blob_custody_add_bytes", EventKind.BlobAdded, () =>
        {
            using var s = new NativeScope();
            Native.Check(Native.aster_blob_custody_add_bytes(_runtime.Handle, _handle, s.Bytes(data.Span), (uint)format, 0, out ulong op), "aster_blob_custody_add_bytes");
            return op;
        }, ct).ConfigureAwait(false));

    /// <summary>Copies a file into temporary custody; later source edits cannot alter imported bytes. No automatic tag is created.</summary>
    public async Task<string> AddPathAsync(string path, BlobFormat format = BlobFormat.Raw, CancellationToken ct = default)
        => Encoding.UTF8.GetString(await Run("aster_blob_custody_add_path", EventKind.BlobAdded, () =>
        {
            using var s = new NativeScope();
            Native.Check(Native.aster_blob_custody_add_path(_runtime.Handle, _handle, s.Utf8(path), (uint)format, 0, out ulong op), "aster_blob_custody_add_path");
            return op;
        }, ct).ConfigureAwait(false));

    /// <summary>Commits the exact named tag and typed root, replacing its prior root. Temporary guards remain until disposal.</summary>
    public async Task RetainAsync(string hash, string tagName, BlobFormat format = BlobFormat.Raw, CancellationToken ct = default)
        => _ = await Run("aster_blob_custody_retain", EventKind.BlobCustodyDone, () =>
        {
            using var s = new NativeScope();
            Native.Check(Native.aster_blob_custody_retain(_runtime.Handle, _handle, s.Utf8(hash), (uint)format, s.Utf8(tagName), 0, out ulong op), "aster_blob_custody_retain");
            return op;
        }, ct).ConfigureAwait(false);

    /// <summary>Reads local root bytes while retaining all guards. A HashSeq returns raw sequence bytes, without Collection interpretation.</summary>
    public Task<byte[]> ReadAsync(string hash, CancellationToken ct = default)
        => Run("aster_blob_custody_read", EventKind.BlobRead, () =>
        {
            using var s = new NativeScope();
            Native.Check(Native.aster_blob_custody_read(_runtime.Handle, _handle, s.Utf8(hash), 0, out ulong op), "aster_blob_custody_read");
            return op;
        }, ct);

    /// <summary>Protects a typed root before fetching from ordered providers. Accepted work retains guards across disposal.</summary>
    /// <remarks>Cancellation waits for downloader cleanup and may include network drain; it is not a prompt deadline for a stalled peer.</remarks>
    public async Task<FetchResult> DownloadHashFromAsync(string hash, IReadOnlyList<string> providers,
        BlobFormat format = BlobFormat.Raw, FetchStrategy strategy = FetchStrategy.Whole, bool toStore = false, CancellationToken ct = default)
    {
        byte[] payload = await Run("aster_blob_custody_download_hash_from", EventKind.BlobFetched, () =>
        {
            using var s = new NativeScope();
            Native.Check(Native.aster_blob_custody_download_hash_from(_runtime.Handle, _handle, s.Utf8(hash), s.Utf8List(providers), (uint)format, (uint)strategy, toStore ? 1u : 0u, 0, out ulong op), "aster_blob_custody_download_hash_from");
            return op;
        }, ct).ConfigureAwait(false);
        var report = Ops.ParseFetchReport(payload, out int consumed);
        return new FetchResult(report, payload.AsSpan(consumed).ToArray());
    }

    /// <summary>Seals new submissions and releases this owner's guards. Accepted work retains them; a native failure preserves ownership for retry.</summary>
    public void Dispose()
    {
        lock (_gate)
        {
            if (_disposed) return;
            int status = Native.aster_blob_custody_free(_runtime.Handle, _handle);
            if (status != (int)Status.NotFound && status != (int)Status.AlreadyClosed) Native.Check(status, "aster_blob_custody_free");
            _disposed = true;
            _runtime.Release();
        }
    }

    public ValueTask DisposeAsync() { Dispose(); return ValueTask.CompletedTask; }
}
