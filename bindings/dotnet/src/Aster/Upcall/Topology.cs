using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Upcall;

/// <summary>Counters of an <see cref="AdmittedFilterHandle"/>.</summary>
public readonly record struct AdmittedFilterStats(ulong Hits, ulong Stale, ulong Misses, ulong Upcalls);

/// <summary>
/// The Rust side of an <see cref="IAdmittedFilter"/> (<c>aster_topology_admitted_t</c>, C ABI 4.1): a
/// cache answered by batched <c>TOPOLOGY_ADMITTED</c> upcalls. Lookups never block: fresh answers
/// are served, stale ones served and refreshed, misses get the kind's failure-mode answer (default:
/// not admitted) and are resolved in the background. Install it with <see cref="Topology.EnableSharedAsync"/>.
/// </summary>
public sealed class AdmittedFilterHandle : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _disposed;

    private AdmittedFilterHandle(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>
    /// Zero values take the defaults: answers served for 30 s, failed batches retried after 1 s,
    /// 256 ids per upcall. Register the filter with <see cref="Upcalls.ServeAdmittedFilter"/>.
    /// </summary>
    public static AdmittedFilterHandle Create(Upcalls upcalls, TimeSpan ttl = default, TimeSpan failureRetry = default, int maxBatch = 0)
    {
        Native.Check(Native.aster_topology_admitted_new(upcalls.Runtime.Handle, upcalls.NativeHandle,
            (uint)Math.Max(0, ttl.TotalMilliseconds), (uint)Math.Max(0, failureRetry.TotalMilliseconds), (uint)Math.Max(0, maxBatch), out ulong h),
            "aster_topology_admitted_new");
        return new AdmittedFilterHandle(upcalls.Runtime, h);
    }

    internal ulong NativeHandle => _handle;

    /// <summary>Looks <paramref name="nodeId"/> (64 hex) up exactly as the swarm does (a miss queues a resolution).</summary>
    public bool IsAdmitted(string nodeId)
    {
        ThrowIfDisposed();
        using var scope = new NativeScope();
        Native.Check(Native.aster_topology_admitted_check(_runtime.Handle, _handle, scope.Utf8(nodeId), out uint v), "aster_topology_admitted_check");
        return v != 0;
    }

    /// <summary>Pushes an answer (e.g. on revocation); it takes effect at the next derived view.</summary>
    public void Set(string nodeId, bool admitted)
    {
        ThrowIfDisposed();
        using var scope = new NativeScope();
        Native.Check(Native.aster_topology_admitted_set(_runtime.Handle, _handle, scope.Utf8(nodeId), admitted ? 1u : 0u), "aster_topology_admitted_set");
    }

    /// <summary>Forgets one id's answer (the next lookup re-resolves).</summary>
    public void Invalidate(string nodeId)
    {
        ThrowIfDisposed();
        using var scope = new NativeScope();
        Native.Check(Native.aster_topology_admitted_invalidate(_runtime.Handle, _handle, scope.Utf8(nodeId)), "aster_topology_admitted_invalidate");
    }

    /// <summary>Forgets every answer.</summary>
    public void Clear() => Invalidate(string.Empty);

    /// <summary>Resolves <paramref name="nodeIds"/> now with one upcall (warm the cache before joining a swarm).</summary>
    public async Task PrefetchAsync(IReadOnlyList<string> nodeIds, CancellationToken cancellationToken = default)
    {
        ThrowIfDisposed();
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_topology_admitted_prefetch(_runtime.Handle, _handle, scope.Utf8List(nodeIds), 0, out op), "aster_topology_admitted_prefetch");
        await Operations.AwaitAsync(_runtime, op, EventKind.UnitResult, "aster_topology_admitted_prefetch", cancellationToken).ConfigureAwait(false);
    }

    public unsafe AdmittedFilterStats Stats()
    {
        ThrowIfDisposed();
        TopologyAdmittedStatsNative s = default;
        Native.Check(Native.aster_topology_admitted_stats(_runtime.Handle, _handle, &s), "aster_topology_admitted_stats");
        return new AdmittedFilterStats(s.hits, s.stale, s.misses, s.upcalls);
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);

    /// <summary>Frees the handle; a swarm it was installed in keeps using it.</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) == 0)
            Native.aster_topology_admitted_free(_runtime.Handle, _handle);
    }
}

/// <summary>Topology v2 swarm membership with an optional .NET admission filter (C ABI 4.1).</summary>
public static class Topology
{
    /// <summary>
    /// Joins the topology swarm keyed by the 32-byte namespace <paramref name="secret"/>: publishes
    /// this node's measurements and derives the shared cluster view, counting only authors
    /// <paramref name="filter"/> admits (null = every holder of the secret). The node must run with
    /// monitoring enabled and <paramref name="filter"/> must belong to the node's runtime.
    /// <paramref name="positionRefresh"/> zero = 60 s.
    /// </summary>
    public static async Task EnableSharedAsync(Node node, byte[] secret, AdmittedFilterHandle? filter = null,
        IReadOnlyList<string>? bootstrap = null, TimeSpan positionRefresh = default, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(node);
        ArgumentNullException.ThrowIfNull(secret);
        if (secret.Length != 32) throw new ArgumentException("secret must be 32 bytes", nameof(secret));
        ulong op;
        unsafe
        {
            using var scope = new NativeScope();
            fixed (byte* key = secret)
                Native.Check(Native.aster_topology_enable_shared(node.Runtime.Handle, node.Handle.Value, key, filter?.NativeHandle ?? 0,
                    scope.Utf8List(bootstrap ?? Array.Empty<string>()), (uint)Math.Max(0, positionRefresh.TotalMilliseconds), 0, out op),
                    "aster_topology_enable_shared");
        }
        await Operations.AwaitAsync(node.Runtime, op, EventKind.UnitResult, "aster_topology_enable_shared", cancellationToken).ConfigureAwait(false);
    }
}
