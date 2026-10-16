using System.Text.Json;
using System.Text.Json.Serialization;
using Aster.Upcall;

namespace Aster;

public enum TopologyLadderLevel : uint { SameHost = 0, SameLan = 1, SameSite = 2, SameRegion = 3, Far = 4 }
/// <summary>Local evidence, not policy proof. Missing measurements remain Unknown.</summary>
public enum TopologySeparation : uint { Unknown = 0, Connected = 1, Separated = 2 }

/// <summary>Copied passive peer measurements. RemoteAddr is local-only; baseline RPC omits it.</summary>
public sealed record TopologyPeerView(string NodeId, TopologyLadderLevel Level, string LevelReason,
    [property: JsonPropertyName("rtt_us")] ulong? RttMicroseconds,
    [property: JsonPropertyName("jitter_us")] ulong? JitterMicroseconds,
    uint LossPpm, ulong ThroughputBps, ulong CwndCeilingBps, bool Direct, string? RelayUrl, string? RemoteAddr,
    ulong CongestionEvents, ulong Samples, uint ConfidencePpm,
    [property: JsonPropertyName("last_measured_unix_ms")] ulong LastMeasuredUnixMilliseconds,
    [property: JsonPropertyName("cluster_held_since_unix_ms")] ulong ClusterHeldSinceUnixMilliseconds, bool IsConnected,
    TopologyQuicTraffic? QuicTraffic = null, TopologyPeerBlobStats? BlobTransfer = null);

/// <summary>QUIC-level bytes on a peer's connections (UDP datagram payload: headers, ACKs and retransmits included).</summary>
public sealed record TopologyQuicBytes(
    [property: JsonPropertyName("quic_bytes_sent")] ulong Sent,
    [property: JsonPropertyName("quic_bytes_received")] ulong Received);

/// <summary>A peer's QUIC bytes by protocol family, cumulative since the node started and across reconnects (C ABI 7.4).</summary>
public sealed record TopologyQuicTraffic(
    TopologyQuicBytes? Blobs, [property: JsonPropertyName("blobs-zstd")] TopologyQuicBytes? BlobsZstd,
    TopologyQuicBytes? Docs, TopologyQuicBytes? Gossip, TopologyQuicBytes? Rpc, TopologyQuicBytes? Other)
{
    /// <summary>Every protocol family summed.</summary>
    public TopologyQuicBytes Total()
    {
        ulong sent = 0, received = 0;
        foreach (var b in new[] { Blobs, BlobsZstd, Docs, Gossip, Rpc, Other })
        {
            if (b is null) continue;
            sent += b.Sent; received += b.Received;
        }
        return new(sent, received);
    }
}

/// <summary>The exact blob payload (and zstd wire) bytes exchanged with one peer, cumulative like <see cref="TopologyQuicTraffic"/> (C ABI 7.4).</summary>
public sealed record TopologyPeerBlobStats(
    ulong FetchedPayloadBytes, ulong FetchedZstdPayloadBytes, ulong FetchedWireBytes, ulong FetchedPlainBytes,
    ulong ServedPayloadBytes, ulong ServedZstdPayloadBytes, ulong ServedWireBytes, ulong ServedPlainBytes,
    ulong PlainFallbacks);

/// <summary>Null fields retain native defaults. Explicit zero is preserved except position refresh (60s).</summary>
public sealed record TopologySwarmOptions
{
    [JsonPropertyName("position_refresh_ms")] public uint? PositionRefreshMilliseconds { get; init; }
    [JsonPropertyName("min_hold_ms")] public uint? MinHoldMilliseconds { get; init; }
    [JsonPropertyName("liveness_ttl_ms")] public uint? LivenessTtlMilliseconds { get; init; }
    [JsonPropertyName("edge_ttl_ms")] public uint? EdgeTtlMilliseconds { get; init; }
    [JsonPropertyName("max_skew_ms")] public uint? MaxSkewMilliseconds { get; init; }
    public uint? SeparationCoverage { get; init; }
    public uint? WitnessR { get; init; }
}

/// <summary>Borrowed local node view. Reading it does not open a listener. Monitoring is needed
/// to populate observations and join a shared swarm.</summary>
public sealed class TopologyView
{
    private readonly Node _node;
    internal TopologyView(Node node) { _node = node; }
    internal static readonly JsonSerializerOptions JsonOptions = new()
    { PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower, DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull };
    internal delegate int Read(out Bytes output);
    internal static T ReadJson<T>(Read call)
    {
        Bytes output = default;
        try
        {
            Native.Check(call(out output), "topology");
            return JsonSerializer.Deserialize<T>(NativeScope.Read(output), JsonOptions)!;
        }
        finally { if (output.ptr != IntPtr.Zero) Native.aster_string_release(output.ptr, output.len); }
    }
    public bool HasMonitoring
    {
        get
        {
            _node.Runtime.Retain();
            try { Native.Check(Native.aster_topology_has_monitoring(_node.Runtime.Handle, _node.Handle.Value, out uint value), "aster_topology_has_monitoring"); return value != 0; }
            finally { _node.Runtime.Release(); }
        }
    }
    public IReadOnlyList<TopologyPeerView> Peers()
    {
        _node.Runtime.Retain();
        try { return ReadJson<TopologyPeerView[]>((out Bytes output) => Native.aster_topology_peers(_node.Runtime.Handle, _node.Handle.Value, out output)); }
        finally { _node.Runtime.Release(); }
    }
    public TopologyPeerView? Peer(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId); _node.Runtime.Retain();
        try
        {
            using var scope = new NativeScope(); var id = scope.Utf8(nodeId);
            return ReadJson<TopologyPeerView?>((out Bytes output) => Native.aster_topology_peer(_node.Runtime.Handle, _node.Handle.Value, id, out output));
        }
        finally { _node.Runtime.Release(); }
    }
    /// <summary>Import the shared namespace and publish measurements. A null filter admits
    /// every namespace-secret holder. Timing values are native milliseconds. Canceling
    /// the wait does not roll back joining; the node owns an installed publisher until
    /// shutdown or replacement by a later join.</summary>
    public async Task EnableSharedAsync(byte[] secret, AdmittedFilterHandle? filter = null,
        IReadOnlyList<string>? bootstrap = null, TopologySwarmOptions? options = null, CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested(); ArgumentNullException.ThrowIfNull(secret);
        if (secret.Length != 32) throw new ArgumentException("secret must be 32 bytes", nameof(secret));
        _node.Runtime.Retain();
        try
        {
            ulong op;
            unsafe
            {
                using var scope = new NativeScope(); var key = scope.Bytes(secret);
                Native.Check(Native.aster_topology_enable_shared_with_options(_node.Runtime.Handle, _node.Handle.Value,
                    (byte*)key.ptr, filter?.NativeHandle ?? 0, scope.Utf8List(bootstrap ?? Array.Empty<string>()),
                    scope.Bytes(JsonSerializer.SerializeToUtf8Bytes(options ?? new TopologySwarmOptions(), JsonOptions)), 0, out op), "aster_topology_enable_shared_with_options");
            }
            await Operations.AwaitAsync(_node.Runtime, op, EventKind.UnitResult, "aster_topology_enable_shared_with_options", ct).ConfigureAwait(false);
        }
        finally { _node.Runtime.Release(); }
    }
    /// <summary>Read one transient observation. Separate view calls may see different eventual
    /// states; use SnapshotAsync for a placement decision.</summary>
    public async Task<IReadOnlyList<ClusterView>> ClustersAsync(CancellationToken ct = default)
    {
        using var snapshot = await SnapshotAsync(ct).ConfigureAwait(false); return snapshot.Clusters();
    }
    public async Task<ClusterView?> MyClusterAsync(CancellationToken ct = default)
    {
        string id = _node.NodeId();
        using var snapshot = await SnapshotAsync(ct).ConfigureAwait(false); return snapshot.ClusterOf(id);
    }
    /// <summary>Read native separation evidence from one transient observation. Unknown remains
    /// a real verdict; use SnapshotAsync to keep multiple questions on the same view.</summary>
    public async Task<TopologySeparation> SeparatedAsync(string a, string b, CancellationToken ct = default)
    {
        using var snapshot = await SnapshotAsync(ct).ConfigureAwait(false); return snapshot.Separated(a, b);
    }
    /// <summary>Capture one immutable shared observation. Requires a joined swarm.
    /// A committed snapshot wins racing cancellation and must be disposed by its caller.</summary>
    public async Task<TopologyObservation> SnapshotAsync(CancellationToken ct = default)
    {
        ct.ThrowIfCancellationRequested(); var runtime = _node.Runtime.Retain(); bool owned = false;
        try
        {
            Native.Check(Native.aster_topology_snapshot(runtime.Handle, _node.Handle.Value, 0, out ulong op), "aster_topology_snapshot");
            var ev = await DocEventReceiver.AwaitTerminal(runtime, op, ct).ConfigureAwait(false);
            if (ev.kind != (uint)EventKind.TopologySnapshot || ev.status != 0) throw EventError.From(runtime, ev, "aster_topology_snapshot");
            EventError.TakePayload(runtime, ev);
            var result = new TopologyObservation(runtime, ev.handle); owned = true; return result;
        }
        finally { if (!owned) runtime.Release(); }
    }
}

/// <summary>Owns one immutable native shared view. Every query uses the same observation.
/// This is local evidence, never serialized or accepted as authorization proof.</summary>
public sealed class TopologyObservation : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private readonly object _gate = new();
    private bool _disposed;
    internal TopologyObservation(Runtime runtime, ulong handle) { _runtime = runtime; _handle = handle; }
    public IReadOnlyList<ClusterView> Clusters()
    {
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            return TopologyView.ReadJson<ClusterView[]>((out Bytes output) => Native.aster_topology_snapshot_clusters(_runtime.Handle, _handle, out output));
        }
    }
    public ClusterView? ClusterOf(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_disposed, this); using var scope = new NativeScope(); var id = scope.Utf8(nodeId);
            return TopologyView.ReadJson<ClusterView?>((out Bytes output) => Native.aster_topology_snapshot_cluster_of(_runtime.Handle, _handle, id, out output));
        }
    }
    public TopologySeparation Separated(string a, string b)
    {
        ArgumentNullException.ThrowIfNull(a); ArgumentNullException.ThrowIfNull(b);
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_disposed, this); using var scope = new NativeScope();
            Native.Check(Native.aster_topology_snapshot_separated(_runtime.Handle, _handle, scope.Utf8(a), scope.Utf8(b), out uint value), "aster_topology_snapshot_separated");
            if (value > 2) throw new IrohException(Status.Internal, "invalid topology separation");
            return (TopologySeparation)value;
        }
    }
    /// <summary>Release native ownership. A native failure preserves the owner for retry.</summary>
    public void Dispose()
    {
        lock (_gate)
        {
            if (_disposed) return;
            int status = Native.aster_topology_snapshot_free(_runtime.Handle, _handle);
            if (status != (int)Status.NotFound && status != (int)Status.AlreadyClosed) Native.Check(status, "aster_topology_snapshot_free");
            _disposed = true; _runtime.Release();
        }
    }
}
