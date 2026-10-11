using System.Buffers.Binary;
using System.Runtime.InteropServices;

namespace Aster;

internal static partial class Native
{
    [LibraryImport(NativeLib)]
    internal static partial int aster_doc_read_entry_contents_if_complete(ulong runtime, ulong doc, BytesList hashes, ulong userData, out ulong operation);
}

public sealed partial class Doc
{
    /// <summary>Reads ordered content hashes in one local native batch. Null slots are missing/partial; non-null empty arrays are complete empty content. Duplicates are preserved.</summary>
    /// <remarks>No fetch, tags or retention. This is not a global snapshot: GC/deletion before validated large-blob export may fail the entire batch. Use custody for stable host verification.</remarks>
    public async Task<IReadOnlyList<byte[]?>> ReadEntryContentsIfCompleteAsync(IReadOnlyList<string> hashes, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(hashes);
        var input = new string[hashes.Count];
        for (int i = 0; i < input.Length; i++)
        {
            string hash = hashes[i];
            if (hash is null || hash.Length != 64 || !hash.All(char.IsAsciiHexDigit))
                throw new ArgumentException($"Hash {i} must contain exactly 64 hexadecimal characters.", nameof(hashes));
            input[i] = hash;
        }
        _runtime.Retain();
        try
        {
            ulong op;
            using (var s = new NativeScope())
                Native.Check(Native.aster_doc_read_entry_contents_if_complete(_runtime.Handle, _handle, s.Utf8List(input), 0, out op), "aster_doc_read_entry_contents_if_complete");
            var ev = await Ops.Await(_runtime, op, EventKind.DocContentsBatch, "aster_doc_read_entry_contents_if_complete", ct).ConfigureAwait(false);
            return DocContentsBatch.Decode(EventError.TakePayload(_runtime, ev), input.Length);
        }
        finally { _runtime.Release(); }
    }
}

internal static class DocContentsBatch
{
    internal static IReadOnlyList<byte[]?> Decode(byte[] payload, int expectedCount)
    {
        if (payload.Length < 4) throw Invalid("truncated count");
        uint count = BinaryPrimitives.ReadUInt32LittleEndian(payload);
        ReadOnlySpan<byte> rest = payload.AsSpan(4);
        if (expectedCount < 0 || count != (uint)expectedCount) throw Invalid("count does not match submitted hashes");
        // Each slot needs at least its presence byte. Check before allocating.
        if (count > (uint)rest.Length) throw Invalid("truncated slots");
        var result = new byte[]?[expectedCount];
        for (int i = 0; i < result.Length; i++)
        {
            if (rest.IsEmpty) throw Invalid("truncated presence");
            byte presence = rest[0];
            rest = rest[1..];
            if (presence == 0) continue;
            if (presence != 1) throw Invalid("invalid presence marker");
            if (rest.Length < 8) throw Invalid("truncated content length");
            ulong length = BinaryPrimitives.ReadUInt64LittleEndian(rest);
            rest = rest[8..];
            if (length > (ulong)rest.Length) throw Invalid("truncated or oversized content");
            result[i] = rest[..(int)length].ToArray();
            rest = rest[(int)length..];
        }
        if (!rest.IsEmpty) throw Invalid("trailing bytes");
        return result;
    }

    private static IrohException Invalid(string reason) => new(Status.Internal, $"document batch: {reason}");
}
