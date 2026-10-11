using System.Buffers.Binary;
using System.Text;

namespace Aster;

/// <summary>Document metadata query selection (<c>aster_doc_query_mode_t</c>).</summary>
public enum DocQueryMode : uint
{
    KeyExact = 0,
    KeyPrefix = 1,
    LatestExact = 2,
    LatestPrefix = 3,
}

/// <summary>Metadata from a document query. Content bytes are read separately by hash.</summary>
/// <param name="Timestamp">Write timestamp in microseconds since the Unix epoch.</param>
public sealed record DocEntry(string AuthorId, byte[] Key, string ContentHash, ulong ContentLen, ulong Timestamp);

public sealed partial class Doc
{
    /// <summary>Queries UTF-8 keys without reading their content blobs. Zero limit means unbounded; LatestExact returns at most one entry.</summary>
    public Task<IReadOnlyList<DocEntry>> QueryAsync(DocQueryMode mode, string key = "", ulong limit = 0, CancellationToken ct = default)
        => QueryAsync(mode, Encoding.UTF8.GetBytes(key), limit, ct);

    /// <summary>Queries binary keys without reading their content blobs.</summary>
    public async Task<IReadOnlyList<DocEntry>> QueryAsync(DocQueryMode mode, byte[] key, ulong limit = 0, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(key);
        if (!Enum.IsDefined(mode)) throw new ArgumentOutOfRangeException(nameof(mode));
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_doc_query(_runtime.Handle, _handle, (uint)mode, scope.Bytes(key), limit, 0, out op), "aster_doc_query");
        var ev = await Ops.Await(_runtime, op, EventKind.DocQuery, "aster_doc_query", ct).ConfigureAwait(false);
        var entries = DocMetadata.Decode(EventError.TakePayload(_runtime, ev));
        if ((ulong)entries.Count != ev.flags)
            throw new IrohException(Status.Internal, "document query: entry count does not match payload");
        return entries;
    }

    /// <summary>Reads a content blob identified by query metadata.</summary>
    public async Task<byte[]> ReadEntryContentAsync(string contentHash, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_doc_read_entry_content(_runtime.Handle, _handle, scope.Utf8(contentHash), 0, out op), "aster_doc_read_entry_content");
        var ev = await Ops.Await(_runtime, op, EventKind.BlobRead, "aster_doc_read_entry_content", ct).ConfigureAwait(false);
        return EventError.TakePayload(_runtime, ev);
    }

    /// <summary>Deletes this author's entries with a UTF-8 key prefix; empty prefix deletes every entry by that author.</summary>
    public Task<ulong> DeleteAsync(string authorId, string prefix, CancellationToken ct = default)
        => DeleteAsync(authorId, Encoding.UTF8.GetBytes(prefix), ct);

    /// <summary>Deletes this author's entries with a binary key prefix and returns the number removed.</summary>
    public async Task<ulong> DeleteAsync(string authorId, byte[] prefix, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(prefix);
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_doc_del(_runtime.Handle, _handle, scope.Utf8(authorId), scope.Bytes(prefix), 0, out op), "aster_doc_del");
        var ev = await Ops.Await(_runtime, op, EventKind.DocDel, "aster_doc_del", ct).ConfigureAwait(false);
        byte[] payload = EventError.TakePayload(_runtime, ev);
        if (payload.Length != sizeof(ulong))
            throw new IrohException(Status.Internal, "document delete: invalid removed-count payload");
        return BinaryPrimitives.ReadUInt64LittleEndian(payload);
    }

    /// <summary>Starts sync with explicit endpoint-ID hex strings. Supply routing addresses through Node.AddNodeAddr first.</summary>
    public async Task StartSyncAsync(IReadOnlyList<string> peers, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(peers);
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_doc_start_sync(_runtime.Handle, _handle, scope.Utf8List(peers), 0, out op), "aster_doc_start_sync");
        var ev = await Ops.Await(_runtime, op, EventKind.UnitResult, "aster_doc_start_sync", ct).ConfigureAwait(false);
        EventError.TakePayload(_runtime, ev);
    }

    /// <summary>Stops syncing this document.</summary>
    public async Task LeaveAsync(CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        Native.Check(Native.aster_doc_leave(_runtime.Handle, _handle, 0, out ulong op), "aster_doc_leave");
        var ev = await Ops.Await(_runtime, op, EventKind.UnitResult, "aster_doc_leave", ct).ConfigureAwait(false);
        EventError.TakePayload(_runtime, ev);
    }

    /// <summary>Shares a ticket with relay/direct address hints. Mode is 0 for read or 1 for write.</summary>
    public async Task<string> ShareWithAddrAsync(uint mode = 0, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        Native.Check(Native.aster_doc_share_with_addr(_runtime.Handle, _handle, mode, 0, out ulong op), "aster_doc_share_with_addr");
        var ev = await Ops.Await(_runtime, op, EventKind.DocShared, "aster_doc_share_with_addr", ct).ConfigureAwait(false);
        return Encoding.UTF8.GetString(EventError.TakePayload(_runtime, ev));
    }
}

internal static class DocMetadata
{
    internal static IReadOnlyList<DocEntry> Decode(byte[] payload)
    {
        var entries = new List<DocEntry>();
        ReadOnlySpan<byte> rest = payload;
        while (!rest.IsEmpty)
            entries.Add(ReadEntry(ref rest));
        return entries;
    }

    internal static DocEntry ReadEntry(ref ReadOnlySpan<byte> rest)
    {
        string author = Encoding.UTF8.GetString(ReadField(ref rest, "author"));
        byte[] key = ReadField(ref rest, "key").ToArray();
        string hash = Encoding.UTF8.GetString(ReadField(ref rest, "hash"));
        if (rest.Length < 16) throw Invalid("truncated length or timestamp");
        ulong length = BinaryPrimitives.ReadUInt64LittleEndian(rest);
        ulong timestamp = BinaryPrimitives.ReadUInt64LittleEndian(rest[8..]);
        rest = rest[16..];
        return new DocEntry(author, key, hash, length, timestamp);
    }

    internal static ReadOnlySpan<byte> ReadField(ref ReadOnlySpan<byte> rest, string field)
    {
        if (rest.Length < 4) throw Invalid($"truncated {field} length");
        uint length = BinaryPrimitives.ReadUInt32LittleEndian(rest);
        rest = rest[4..];
        if (length > (uint)rest.Length) throw Invalid($"truncated {field}");
        var value = rest[..(int)length];
        rest = rest[(int)length..];
        return value;
    }

    private static IrohException Invalid(string detail) => new(Status.Internal, $"document query: {detail}");
}
