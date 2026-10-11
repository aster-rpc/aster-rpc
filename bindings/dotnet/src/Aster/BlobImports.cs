using System.Text;

namespace Aster;

public sealed partial class Blobs
{
    /// <summary>Imports a UTF-8 path by copying its content into the store. An automatic persistent tag retains it across GC.</summary>
    /// <remarks>Cancellation abandons the wait; a native import already started may still finish.</remarks>
    public async Task<string> AddPathAsync(string path, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(path);
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_blobs_add_path(_runtime.Handle, _nodeHandle, scope.Utf8(path), 0, out op), "aster_blobs_add_path");
        var ev = await Ops.Await(_runtime, op, EventKind.BlobAdded, "aster_blobs_add_path", ct).ConfigureAwait(false);
        return Encoding.UTF8.GetString(EventError.TakePayload(_runtime, ev));
    }

    /// <summary>Copies a file into the store and retains it under the exact UTF-8 tag name, replacing that named root.</summary>
    /// <remarks>No additional automatic tag is created. Deleting the tag permits GC when no other retention protects the blob.
    /// Cancellation abandons the wait; the native import and tag update may still finish.</remarks>
    public async Task<string> AddPathWithNamedTagAsync(string path, string tagName, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        ArgumentNullException.ThrowIfNull(path);
        ArgumentNullException.ThrowIfNull(tagName);
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_blobs_add_path_with_named_tag(_runtime.Handle, _nodeHandle, scope.Utf8(path), scope.Utf8(tagName), 0, out op), "aster_blobs_add_path_with_named_tag");
        var ev = await Ops.Await(_runtime, op, EventKind.BlobAdded, "aster_blobs_add_path_with_named_tag", ct).ConfigureAwait(false);
        return Encoding.UTF8.GetString(EventError.TakePayload(_runtime, ev));
    }
}
