using System.Runtime.CompilerServices;
using System.Text;

namespace Aster;

/// <summary>Native live document event subtype.</summary>
public enum DocEventKind : uint
{
    InsertLocal = 0,
    InsertRemote = 1,
    ContentReady = 2,
    PendingContentReady = 3,
    NeighborUp = 4,
    NeighborDown = 5,
    SyncFinished = 6,
}

/// <summary>Which side started a document sync round.</summary>
public enum DocSyncOrigin : byte
{
    /// <summary>The peer dialled this node and it accepted.</summary>
    Accept = 0,
    /// <summary>This node dialled the peer.</summary>
    Connect = 1,
}

/// <summary>How a sync round ended. A failed round has <c>Succeeded</c> false and the document layer's message in <c>Error</c>.</summary>
public sealed record DocSyncResult(DocSyncOrigin Origin, bool Succeeded, string? Error = null);

/// <summary>A live event; insertion events carry metadata, remote insertions and neighbor/sync events carry a peer ID, and sync-finished events carry <c>Sync</c>.</summary>
public sealed record DocEvent(DocEventKind Kind, DocEntry? Entry = null, string? PeerId = null, string? ContentHash = null, DocSyncResult? Sync = null);

public sealed partial class Doc
{
    /// <summary>Subscribes to future events. Dispose the receiver independently from this document.</summary>
    public async Task<DocEventReceiver> SubscribeAsync(CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        _runtime.Retain();
        try
        {
            Native.Check(Native.aster_doc_subscribe(_runtime.Handle, _handle, 0, out ulong op), "aster_doc_subscribe");
            var ev = await DocEventReceiver.AwaitTerminal(_runtime, op, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.DocSubscribed || ev.status != 0)
                throw EventError.From(_runtime, ev, "aster_doc_subscribe");
            EventError.TakePayload(_runtime, ev);
            return new DocEventReceiver(_runtime, ev.handle);
        }
        finally { _runtime.Release(); }
    }
}

public sealed partial class Docs
{
    /// <summary>Atomically joins and subscribes. The returned document and event receiver each require disposal.</summary>
    public async Task<(Doc Doc, DocEventReceiver Events)> JoinAndSubscribeAsync(string ticket, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        _runtime.Retain();
        try
        {
            ulong op;
            using (var scope = new NativeScope())
                Native.Check(Native.aster_docs_join_and_subscribe(_runtime.Handle, _nodeHandle, scope.Utf8(ticket), 0, out op), "aster_docs_join_and_subscribe");
            var ev = await DocEventReceiver.AwaitTerminal(_runtime, op, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.DocJoinedAndSubscribed || ev.status != 0)
                throw EventError.From(_runtime, ev, "aster_docs_join_and_subscribe");
            EventError.TakePayload(_runtime, ev);
            return (new Doc(_runtime, ev.handle), new DocEventReceiver(_runtime, ev.related));
        }
        finally { _runtime.Release(); }
    }
}

/// <summary>Owns a native Docs subscription. Receive cancellation leaves the receiver usable; disposal wakes pending receives with EOF.</summary>
public sealed class DocEventReceiver : IDisposable, IAsyncDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private readonly object _gate = new();
    private bool _disposed;

    internal DocEventReceiver(Runtime runtime, ulong handle)
    {
        _runtime = runtime.Retain();
        _handle = handle;
    }

    // Do not cancel the managed completion task: cancellation can lose to a
    // committed native event. Consume the single native terminal in either case.
    internal static async Task<Event> AwaitTerminal(Runtime runtime, ulong op, CancellationToken ct)
    {
        var terminal = runtime.WaitForAsync(op);
        using var registration = ct.Register(() => Native.aster_operation_cancel(runtime.Handle, op));
        var ev = await terminal.ConfigureAwait(false);
        if (ev.kind == (uint)EventKind.OperationCancelled)
        {
            EventError.TakePayload(runtime, ev);
            throw new OperationCanceledException(ct);
        }
        return ev;
    }

    /// <summary>Receives one event, or null at EOF. A committed event wins a racing cancellation and is returned.</summary>
    public async Task<DocEvent?> ReceiveAsync(CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested();
        lock (_gate)
        {
            if (_disposed) return null;
            _runtime.Retain();
        }
        try
        {
            int status = Native.aster_doc_event_recv(_runtime.Handle, _handle, 0, out ulong op);
            if (status == (int)Status.NotFound) { Dispose(); return null; }
            Native.Check(status, "aster_doc_event_recv");
            var ev = await AwaitTerminal(_runtime, op, ct).ConfigureAwait(false);
            if (ev.kind == (uint)EventKind.DocEvent && ev.status == (uint)Status.NotFound)
            {
                EventError.TakePayload(_runtime, ev);
                Dispose();
                return null;
            }
            if (ev.kind != (uint)EventKind.DocEvent || ev.status != 0)
                throw EventError.From(_runtime, ev, "aster_doc_event_recv");
            return Decode(ev.flags, EventError.TakePayload(_runtime, ev));
        }
        finally { _runtime.Release(); }
    }

    /// <summary>Enumerates events until EOF or cancellation. The caller still owns and must dispose this receiver.</summary>
    public async IAsyncEnumerable<DocEvent> ReadAllAsync([EnumeratorCancellation] CancellationToken ct = default)
    {
        while (await ReceiveAsync(ct).ConfigureAwait(false) is { } ev) yield return ev;
    }

    internal static DocEvent Decode(uint subtype, byte[] payload)
    {
        ReadOnlySpan<byte> rest = payload;
        var kind = (DocEventKind)subtype;
        DocEvent ev;
        switch (kind)
        {
            case DocEventKind.InsertLocal:
                ev = new(kind, DocMetadata.ReadEntry(ref rest));
                break;
            case DocEventKind.InsertRemote:
                var entry = DocMetadata.ReadEntry(ref rest);
                ev = new(kind, entry, Encoding.UTF8.GetString(DocMetadata.ReadField(ref rest, "peer")));
                break;
            case DocEventKind.ContentReady:
                return new(kind, ContentHash: Encoding.UTF8.GetString(payload));
            case DocEventKind.NeighborUp:
            case DocEventKind.NeighborDown:
                return new(kind, PeerId: Encoding.UTF8.GetString(payload));
            case DocEventKind.SyncFinished:
            {
                string peer = Encoding.UTF8.GetString(DocMetadata.ReadField(ref rest, "peer"));
                if (rest.Length < 2 || rest[0] > 1 || rest[1] > 1)
                    throw new IrohException(Status.Internal, "document event: malformed sync result");
                var origin = (DocSyncOrigin)rest[0];
                bool succeeded = rest[1] == 1;
                rest = rest[2..];
                var error = DocMetadata.ReadField(ref rest, "sync error");
                if (succeeded && !error.IsEmpty)
                    throw new IrohException(Status.Internal, "document event: completed sync carries an error");
                ev = new(kind, PeerId: peer, Sync: new(origin, succeeded, succeeded ? null : Encoding.UTF8.GetString(error)));
                break;
            }
            case DocEventKind.PendingContentReady:
                ev = new(kind);
                break;
            default:
                throw new IrohException(Status.Internal, $"document event: unknown subtype {subtype}");
        }
        if (!rest.IsEmpty) throw new IrohException(Status.Internal, "document event: trailing payload bytes");
        return ev;
    }

    public void Dispose()
    {
        lock (_gate)
        {
            if (_disposed) return;
            _disposed = true;
        }
        try { Native.aster_doc_event_receiver_free(_runtime.Handle, _handle); }
        finally { _runtime.Release(); }
    }

    public ValueTask DisposeAsync()
    {
        Dispose();
        return ValueTask.CompletedTask;
    }
}
