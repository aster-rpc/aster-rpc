using System;
using System.Buffers.Binary;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Aster.Crypto;

namespace Aster;

// ABI 3.3 data-plane parity (ffi/RENAME_MAP.md "New in 3.3"): blobs fetch strategy / providers /
// report / to-store, read_range, tag prefix delete, GC; docs authority / retire / open-import /
// durable set / entry status; connection paths and stream priority.

/// <summary>Blob format (<c>aster_blob_format_t</c>).</summary>
public enum BlobFormat : uint
{
    Raw = 0,
    /// <summary>A hash sequence; it need not use Collection metadata.</summary>
    HashSeq = 1,
}

/// <summary>Multi-provider fetch strategy (<c>aster_fetch_strategy_t</c>).</summary>
public enum FetchStrategy : uint
{
    Whole = 0,
    /// <summary>Fetch each child of a <see cref="BlobFormat.HashSeq"/> separately (provider order per child).</summary>
    SplitChildren = 1,
}

/// <summary>Local blob status (<c>aster_blob_status_t</c>).</summary>
public enum BlobStatus : uint
{
    NotFound = 0,
    Partial = 1,
    Complete = 2,
}

/// <summary>Outcome of <see cref="Docs.RetireLocalWriteAuthorityAsync"/> (<c>aster_retire_outcome_t</c>).</summary>
public enum RetireOutcome : uint
{
    Retired = 0,
    /// <summary>The namespace is still open elsewhere; nothing changed.</summary>
    Busy = 1,
}

/// <summary>
/// Per-provider statistics of a fetch (Rust <c>aster::ProviderOutcome</c>; fetch report v2, ABI 7.4).
/// </summary>
/// <param name="NodeId">The provider's 64-hex node id.</param>
/// <param name="Attempts">Requests sent to this provider.</param>
/// <param name="Failures">Requests that failed.</param>
/// <param name="CompletedRequests">Requests carried to completion.</param>
/// <param name="InFlight">Requests mid-attempt when the report was taken (0 in a finished fetch).</param>
/// <param name="PlainFallbacks">Attempts that went over plain transport because the provider does not serve compressed blobs.</param>
/// <param name="BytesTransferred">Payload bytes received from this provider.</param>
/// <param name="WireBytes">Compressed bytes received from this provider over zstd; null when every attempt was plain.</param>
public sealed record FetchProviderStats(string NodeId, uint Attempts, uint Failures, uint CompletedRequests,
    uint InFlight = 0, uint PlainFallbacks = 0, ulong BytesTransferred = 0, ulong? WireBytes = null);

/// <summary>A fetch report: bytes transferred (the sum of the providers') and per-provider stats in first-attempt order.</summary>
public sealed record FetchReport(ulong BytesTransferred, IReadOnlyList<FetchProviderStats> Providers)
{
    /// <summary>Providers with a request mid-attempt when the report was taken.</summary>
    public IEnumerable<string> InFlight() => Providers.Where(p => p.InFlight > 0).Select(p => p.NodeId);

    /// <summary>Providers that do not serve compressed blobs and were fetched from (or tried) over plain transport; cache these as plain-only.</summary>
    public IEnumerable<string> PlainOnly() => Providers.Where(p => p.PlainFallbacks > 0).Select(p => p.NodeId);

    /// <summary>Providers that carried at least one request to completion.</summary>
    public IEnumerable<string> Served() => Providers.Where(p => p.CompletedRequests > 0).Select(p => p.NodeId);
}

/// <summary>
/// Node-wide blob transfer counters (Rust <c>aster::BlobTransferStats</c>; <see cref="Blobs.TransferStats"/>).
/// Payload bytes are the blob bytes; wire bytes are what crossed the network (compressed for zstd).
/// </summary>
public sealed record BlobTransferStats(
    ulong FetchedPayloadBytes, ulong FetchedZstdPayloadBytes, ulong FetchedWireBytes, ulong FetchedPlainBytes,
    ulong ServedPayloadBytes, ulong ServedZstdPayloadBytes, ulong ServedWireBytes, ulong ServedPlainBytes,
    ulong PlainFallbacks, ulong PlainServes,
    ulong FetchPlainOk, ulong FetchPlainFailed, ulong FetchZstdOk, ulong FetchZstdFailed,
    ulong ServePlainOk, ulong ServePlainFailed, ulong ServeZstdOk, ulong ServeZstdFailed,
    ulong FetchDecodeErrors, ulong ServeDecodeErrors);

/// <summary>A completed fetch: the report and the blob bytes (empty when fetched to the store).</summary>
public sealed record FetchResult(FetchReport Report, byte[] Data);

/// <summary>Local authority over a namespace: access and its authority fingerprint.</summary>
public sealed record AuthorityReport(LocalAccess Access, byte[] NamespaceId, byte[] Fingerprint);

/// <summary>Result of <see cref="Docs.RetireLocalWriteAuthorityAsync"/>.</summary>
public sealed record RetireResult(RetireOutcome Outcome, LocalAccess Access, ulong OpenReferences);

/// <summary>Transport path kind (<c>aster_path_kind_t</c>).</summary>
public enum PathKind : uint
{
    None = 0,
    Direct = 1,
    Relay = 2,
    Other = 3,
}

/// <summary>One transport path of a connection.</summary>
public sealed record PathInfo(PathKind Kind, bool IsSelected, string? PeerHost, ushort PeerPort, string? RelayUrl, string? Other, TimeSpan? Rtt);

internal static class Ops
{
    /// <summary>Awaits an async op; ASTER_EVENT_ERROR or another kind than <paramref name="expected"/> throws.</summary>
    public static async Task<Event> Await(Runtime runtime, ulong opId, EventKind expected, string what, CancellationToken ct)
    {
        Event ev = await runtime.WaitForAsync(opId, ct).ConfigureAwait(false);
        if (ev.kind != (uint)expected || ev.status != 0)
            throw EventError.From(runtime, ev, what);
        return ev;
    }

    // ffi/src/data.rs: ASTER_FETCH_REPORT_HEADER_SIZE, _PROVIDER_RECORD_SIZE (v1), _PROVIDER_RECORD_V2_SIZE.
    private const int ReportHeader = 16, RecordV1 = 44, RecordV2 = 72;
    private const uint ProviderHasWireBytes = 1; // ASTER_FETCH_PROVIDER_HAS_WIRE_BYTES

    /// <summary>
    /// Parses fetch report v1 or v2 (the version byte decides) at the start of <paramref name="payload"/>;
    /// <paramref name="consumed"/> is where the blob bytes start. A truncated or unknown report throws
    /// <see cref="IrohException"/>.
    /// </summary>
    public static FetchReport ParseFetchReport(ReadOnlySpan<byte> payload, out int consumed)
    {
        if (payload.Length < ReportHeader)
            throw new IrohException(Status.Internal, "fetch report: truncated header");
        int record = payload[0] switch
        {
            1 => RecordV1,
            2 => RecordV2,
            _ => throw new IrohException(Status.Internal, $"fetch report: unsupported version {payload[0]}"),
        };
        uint n = BinaryPrimitives.ReadUInt32LittleEndian(payload[4..]);
        ulong bytes = BinaryPrimitives.ReadUInt64LittleEndian(payload[8..]);
        if (n > (uint)((payload.Length - ReportHeader) / record))
            throw new IrohException(Status.Internal, $"fetch report: {n} provider records do not fit in {payload.Length} bytes");
        var providers = new List<FetchProviderStats>((int)n);
        int off = ReportHeader;
        for (uint i = 0; i < n; i++, off += record)
        {
            var r = payload.Slice(off, record);
            string node = Convert.ToHexString(r[..32]).ToLowerInvariant();
            uint attempts = BinaryPrimitives.ReadUInt32LittleEndian(r[32..]);
            uint failures = BinaryPrimitives.ReadUInt32LittleEndian(r[36..]);
            uint completed = BinaryPrimitives.ReadUInt32LittleEndian(r[40..]);
            if (record == RecordV1)
            {
                providers.Add(new FetchProviderStats(node, attempts, failures, completed));
                continue;
            }
            uint flags = BinaryPrimitives.ReadUInt32LittleEndian(r[52..]);
            ulong wire = BinaryPrimitives.ReadUInt64LittleEndian(r[64..]);
            providers.Add(new FetchProviderStats(node, attempts, failures, completed,
                InFlight: BinaryPrimitives.ReadUInt32LittleEndian(r[44..]),
                PlainFallbacks: BinaryPrimitives.ReadUInt32LittleEndian(r[48..]),
                BytesTransferred: BinaryPrimitives.ReadUInt64LittleEndian(r[56..]),
                WireBytes: (flags & ProviderHasWireBytes) != 0 ? wire : null));
        }
        consumed = off;
        return new FetchReport(bytes, providers);
    }
}

public sealed partial class Blobs
{
    private const uint FetchToStore = 1;

    /// <summary>Reads <paramref name="length"/> bytes at <paramref name="offset"/> of a local blob.</summary>
    public async Task<byte[]> ReadRangeAsync(string hashHex, ulong offset, ulong length, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_blobs_read_range(_runtime.Handle, _nodeHandle, s.Utf8(hashHex), offset, length, 0, out op), "aster_blobs_read_range");
        var ev = await Ops.Await(_runtime, op, EventKind.BlobRead, "aster_blobs_read_range", ct).ConfigureAwait(false);
        return EventError.TakePayload(_runtime, ev);
    }

    /// <summary>
    /// Downloads <paramref name="hashHex"/> from one provider (64-hex node id). With
    /// <paramref name="toStore"/> the blob stays in the store and no bytes are returned.
    /// </summary>
    public async Task<byte[]> DownloadHashAsync(string hashHex, string provider, BlobFormat format = BlobFormat.Raw,
        bool toStore = false, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_blobs_download_hash(_runtime.Handle, _nodeHandle, s.Utf8(hashHex), s.Utf8(provider),
                (uint)format, toStore ? FetchToStore : 0, 0, out op), "aster_blobs_download_hash");
        var ev = await Ops.Await(_runtime, op, EventKind.BlobDownloaded, "aster_blobs_download_hash", ct).ConfigureAwait(false);
        return EventError.TakePayload(_runtime, ev);
    }

    /// <summary>
    /// Downloads from an ordered provider list with failover (each attempt asks only for the
    /// missing ranges) and returns the fetch report.
    /// </summary>
    public async Task<FetchResult> DownloadHashFromAsync(string hashHex, IReadOnlyList<string> providers,
        BlobFormat format = BlobFormat.Raw, FetchStrategy strategy = FetchStrategy.Whole, bool toStore = false,
        CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_blobs_download_hash_from(_runtime.Handle, _nodeHandle, s.Utf8(hashHex), s.Utf8List(providers),
                (uint)format, (uint)strategy, (toStore ? FetchToStore : 0) | Native.FetchReportV2, 0, out op), "aster_blobs_download_hash_from");
        var ev = await Ops.Await(_runtime, op, EventKind.BlobFetched, "aster_blobs_download_hash_from", ct).ConfigureAwait(false);
        byte[] payload = EventError.TakePayload(_runtime, ev);
        var report = Ops.ParseFetchReport(payload, out int consumed);
        return new FetchResult(report, payload.AsSpan(consumed).ToArray());
    }

    /// <summary>
    /// The node's blob transfer counters since it started (Rust <c>aster::Blobs::transfer_stats</c>,
    /// <c>aster_blobs_transfer_stats</c>, ABI 7.4): every fetch and serve, plain and zstd.
    /// </summary>
    public unsafe BlobTransferStats TransferStats()
    {
        BlobTransferStatsNative n = default;
        n.struct_size = (uint)sizeof(BlobTransferStatsNative);
        Native.Check(Native.aster_blobs_transfer_stats(_runtime.Handle, _nodeHandle, &n), "aster_blobs_transfer_stats");
        return new BlobTransferStats(
            n.fetched_payload_bytes, n.fetched_zstd_payload_bytes, n.fetched_wire_bytes, n.fetched_plain_bytes,
            n.served_payload_bytes, n.served_zstd_payload_bytes, n.served_wire_bytes, n.served_plain_bytes,
            n.plain_fallbacks, n.plain_serves,
            n.fetch_plain_ok, n.fetch_plain_failed, n.fetch_zstd_ok, n.fetch_zstd_failed,
            n.serve_plain_ok, n.serve_plain_failed, n.serve_zstd_ok, n.serve_zstd_failed,
            n.fetch_decode_errors, n.serve_decode_errors);
    }

    /// <summary>Runs one node-wide blob GC pass.</summary>
    public async Task GcRunOnceAsync(CancellationToken ct = default)
    {
        Native.Check(Native.aster_blobs_gc_run_once(_runtime.Handle, _nodeHandle, 0, out ulong op), "aster_blobs_gc_run_once");
        await Ops.Await(_runtime, op, EventKind.UnitResult, "aster_blobs_gc_run_once", ct).ConfigureAwait(false);
    }

    /// <summary>Local status and size of a blob.</summary>
    public (BlobStatus Status, ulong Size) GetStatus(string hashHex)
    {
        using var s = new NativeScope();
        var h = s.Utf8(hashHex);
        Native.Check(Native.aster_blobs_status(_runtime.Handle, _nodeHandle, h.ptr, h.len, out uint status, out ulong size), "aster_blobs_status");
        return ((BlobStatus)status, size);
    }
}

public sealed partial class Tags
{
    /// <summary>Deletes every tag starting with <paramref name="prefix"/> (empty = all); returns the count.</summary>
    public async Task<ulong> DeletePrefixAsync(string prefix, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_tags_delete_prefix(_runtime.Handle, _nodeHandle, s.Utf8(prefix), 0, out op), "aster_tags_delete_prefix");
        var ev = await Ops.Await(_runtime, op, EventKind.TagDeleted, "aster_tags_delete_prefix", ct).ConfigureAwait(false);
        byte[] payload = EventError.TakePayload(_runtime, ev);
        return payload.Length >= 8 ? BinaryPrimitives.ReadUInt64LittleEndian(payload) : ev.flags;
    }
}

public sealed partial class Docs
{
    /// <summary>Reopens a namespace already in the local replica; <c>null</c> if absent.</summary>
    public async Task<Doc?> OpenAsync(string namespaceId, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_docs_open(_runtime.Handle, _nodeHandle, s.Utf8(namespaceId), 0, out op), "aster_docs_open");
        Event ev = await _runtime.WaitForAsync(op, ct).ConfigureAwait(false);
        if (ev.kind == (uint)EventKind.DocOpened && ev.status == (uint)Aster.Status.NotFound)
            return null;
        if (ev.kind != (uint)EventKind.DocOpened || ev.status != 0)
            throw EventError.From(_runtime, ev, "aster_docs_open");
        return new Doc(_runtime, ev.handle);
    }

    /// <summary>Opens or imports a <b>writable</b> namespace from its 32-byte secret (idempotent).</summary>
    public async Task<Doc> ImportWriteNamespaceAsync(byte[] namespaceSecret, CancellationToken ct = default)
    {
        Ids.Require32(namespaceSecret, nameof(namespaceSecret));
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_docs_import_write_namespace(_runtime.Handle, _nodeHandle, s.Bytes(namespaceSecret), 0, out op),
                "aster_docs_import_write_namespace");
        var ev = await Ops.Await(_runtime, op, EventKind.DocOpened, "aster_docs_import_write_namespace", ct).ConfigureAwait(false);
        return new Doc(_runtime, ev.handle);
    }

    /// <summary>Opens or imports a read-only namespace by id.</summary>
    public async Task<Doc> ImportReadNamespaceAsync(string namespaceId, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_docs_import_read_namespace(_runtime.Handle, _nodeHandle, s.Utf8(namespaceId), 0, out op),
                "aster_docs_import_read_namespace");
        var ev = await Ops.Await(_runtime, op, EventKind.DocOpened, "aster_docs_import_read_namespace", ct).ConfigureAwait(false);
        return new Doc(_runtime, ev.handle);
    }

    /// <summary>This node's authority over <paramref name="namespaceId"/>.</summary>
    public async Task<AuthorityReport> AuthorityReportAsync(string namespaceId, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_docs_authority_report(_runtime.Handle, _nodeHandle, s.Utf8(namespaceId), 0, out op), "aster_docs_authority_report");
        var ev = await Ops.Await(_runtime, op, EventKind.DocAuthorityReport, "aster_docs_authority_report", ct).ConfigureAwait(false);
        return ParseAuthority(_runtime, ev);
    }

    /// <summary>Drops local write authority (keeps read access) unless the namespace is busy.</summary>
    public async Task<RetireResult> RetireLocalWriteAuthorityAsync(string namespaceId, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_docs_retire_local_write_authority(_runtime.Handle, _nodeHandle, s.Utf8(namespaceId), 0, out op),
                "aster_docs_retire_local_write_authority");
        var ev = await Ops.Await(_runtime, op, EventKind.DocRetireOutcome, "aster_docs_retire_local_write_authority", ct).ConfigureAwait(false);
        byte[] p = EventError.TakePayload(_runtime, ev);
        return new RetireResult((RetireOutcome)ev.flags,
            (LocalAccess)BinaryPrimitives.ReadUInt32LittleEndian(p),
            BinaryPrimitives.ReadUInt64LittleEndian(p.AsSpan(8)));
    }

    /// <summary>The authority fingerprint of (<paramref name="namespaceId"/>, <paramref name="access"/>). Pure.</summary>
    public static unsafe byte[] AuthorityFingerprint(string namespaceId, LocalAccess access)
    {
        var output = new byte[32];
        using var s = new NativeScope();
        fixed (byte* o = output)
            Native.Check(Native.aster_doc_authority_fingerprint(s.Utf8(namespaceId), (uint)access, o), "aster_doc_authority_fingerprint");
        return output;
    }

    internal static AuthorityReport ParseAuthority(Runtime runtime, Event ev)
    {
        byte[] p = EventError.TakePayload(runtime, ev);
        return new AuthorityReport((LocalAccess)ev.flags, p.AsSpan(0, 32).ToArray(), p.AsSpan(32, 32).ToArray());
    }
}

public sealed partial class Doc
{
    /// <summary>The namespace id (64 hex).</summary>
    public unsafe string Id()
    {
        byte* buf = stackalloc byte[128];
        UIntPtr len = 128;
        Native.Check(Native.aster_doc_id(_runtime.Handle, _handle, buf, &len), "aster_doc_id");
        return System.Text.Encoding.ASCII.GetString(buf, (int)len);
    }

    /// <summary>Sets an entry and waits until its content is durable; returns the content hash.</summary>
    public async Task<string> SetBytesDurableAsync(string authorId, string key, byte[] value, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_doc_set_bytes_durable(_runtime.Handle, _handle, s.Utf8(authorId), s.Utf8(key), s.Bytes(value), 0, out op),
                "aster_doc_set_bytes_durable");
        var ev = await Ops.Await(_runtime, op, EventKind.DocSet, "aster_doc_set_bytes_durable", ct).ConfigureAwait(false);
        return System.Text.Encoding.UTF8.GetString(EventError.TakePayload(_runtime, ev));
    }

    /// <summary>Whether an entry (or its tombstone) exists.</summary>
    public async Task<bool> EntryExistsAsync(string authorId, string key, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_doc_entry_exists(_runtime.Handle, _handle, s.Utf8(authorId), s.Utf8(key), 0, out op), "aster_doc_entry_exists");
        var ev = await Ops.Await(_runtime, op, EventKind.DocEntryExists, "aster_doc_entry_exists", ct).ConfigureAwait(false);
        EventError.TakePayload(_runtime, ev);
        return ev.flags != 0;
    }

    /// <summary>Local status and size of an entry's content blob.</summary>
    public async Task<(BlobStatus Status, ulong Size)> ContentStatusAsync(string contentHashHex, CancellationToken ct = default)
    {
        ulong op;
        using (var s = new NativeScope())
            Native.Check(Native.aster_doc_entry_content_status(_runtime.Handle, _handle, s.Utf8(contentHashHex), 0, out op),
                "aster_doc_entry_content_status");
        var ev = await Ops.Await(_runtime, op, EventKind.DocContentStatus, "aster_doc_entry_content_status", ct).ConfigureAwait(false);
        byte[] p = EventError.TakePayload(_runtime, ev);
        return ((BlobStatus)ev.flags, p.Length >= 8 ? BinaryPrimitives.ReadUInt64LittleEndian(p) : 0);
    }

    /// <summary>
    /// Frees this handle now, then leaves and closes the doc under one deadline (role swap).
    /// The <see cref="Doc"/> must not be used afterwards.
    /// </summary>
    public async Task ShutdownForRoleSwapAsync(TimeSpan budget, CancellationToken ct = default)
    {
        Native.Check(Native.aster_doc_shutdown_for_role_swap(_runtime.Handle, _handle, (ulong)budget.TotalMilliseconds, 0, out ulong op),
            "aster_doc_shutdown_for_role_swap");
        _disposed = true;
        await Ops.Await(_runtime, op, EventKind.UnitResult, "aster_doc_shutdown_for_role_swap", ct).ConfigureAwait(false);
    }
}

public sealed partial class Connection
{
    /// <summary>Every transport path of this connection.</summary>
    public IReadOnlyList<PathInfo> Paths() => ReadPaths(selectedOnly: false);

    /// <summary>The selected path, if any.</summary>
    public PathInfo? SelectedPath() => ReadPaths(selectedOnly: true).FirstOrDefault();

    private unsafe IReadOnlyList<PathInfo> ReadPaths(bool selectedOnly)
    {
        var list = new PathListNative { struct_size = (uint)sizeof(PathListNative) };
        int r = selectedOnly
            ? Native.aster_connection_selected_path(_runtime.Handle, _handle.Value, &list)
            : Native.aster_connection_paths(_runtime.Handle, _handle.Value, &list);
        Native.Check(r, selectedOnly ? "aster_connection_selected_path" : "aster_connection_paths");
        try
        {
            var result = new List<PathInfo>((int)list.len);
            var items = (PathInfoNative*)list.items;
            for (int i = 0; i < (int)list.len; i++)
            {
                ref var p = ref items[i];
                string? Str(Bytes b) => b.IsEmpty ? null : b.ToUtf8String();
                result.Add(new PathInfo((PathKind)p.path_kind, p.is_selected != 0, Str(p.peer_host), p.peer_port,
                    Str(p.relay_url), Str(p.other), p.rtt_micros == ulong.MaxValue ? null : TimeSpan.FromMicroseconds(p.rtt_micros)));
            }
            return result;
        }
        finally { Native.aster_path_list_release(&list); }
    }
}

public sealed partial class SendStream
{
    /// <summary>Sets the stream's send priority (higher is sent first).</summary>
    public async Task SetPriorityAsync(int priority, CancellationToken ct = default)
    {
        Native.Check(Native.aster_send_stream_set_priority(_runtime.Handle, _handle.Value, priority, 0, out ulong op), "aster_send_stream_set_priority");
        await Ops.Await(_runtime, op, EventKind.UnitResult, "aster_send_stream_set_priority", ct).ConfigureAwait(false);
    }

    /// <summary>The stream's send priority.</summary>
    public async Task<int> GetPriorityAsync(CancellationToken ct = default)
    {
        Native.Check(Native.aster_send_stream_priority(_runtime.Handle, _handle.Value, 0, out ulong op), "aster_send_stream_priority");
        var ev = await Ops.Await(_runtime, op, EventKind.StreamPriority, "aster_send_stream_priority", ct).ConfigureAwait(false);
        byte[] p = EventError.TakePayload(_runtime, ev);
        return p.Length >= 4 ? BinaryPrimitives.ReadInt32LittleEndian(p) : unchecked((int)ev.flags);
    }
}
