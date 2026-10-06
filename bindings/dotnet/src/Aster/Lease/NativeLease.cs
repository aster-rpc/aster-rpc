using System;
using System.Threading;
using System.Threading.Tasks;
using Aster.Upcall;

namespace Aster.Lease;

/// <summary>
/// Timing of a lease. All zero = the defaults (30 s ttl, 10 s renew, 1 s margin); with a ttl, a
/// zero <see cref="RenewInterval"/> = ttl/3 and a zero <see cref="HolderMargin"/> = ttl/10 (at most 1 s).
/// </summary>
public readonly record struct LeaseTiming(TimeSpan Ttl, TimeSpan RenewInterval = default, TimeSpan HolderMargin = default)
{
    internal static ulong Ms(TimeSpan t) => t <= TimeSpan.Zero ? 0 : (ulong)Math.Max(1, (long)t.TotalMilliseconds);
}

/// <summary>
/// A Rust-side lease serializer (<c>aster_lease_serializer_t</c>, C ABI 4.1) that leases are
/// acquired from: either <see cref="Foreign"/> (the serialization point is a .NET
/// <see cref="ILeaseSerializer"/> served on an <see cref="Upcalls"/> table) or <see cref="Memory"/>
/// (Rust's reference in-memory serializer, optionally guarded by an <see cref="ILeaseAuthorizer"/>).
/// </summary>
public sealed class NativeLeaseSerializer : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _disposed;

    private NativeLeaseSerializer(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>
    /// A serializer whose transitions and snapshots are upcalls on <paramref name="upcalls"/>
    /// (register the implementation with <see cref="Upcalls.ServeLeaseSerializer"/>). A failed
    /// upcall is a backend error, never a denial.
    /// </summary>
    public static NativeLeaseSerializer Foreign(Upcalls upcalls)
    {
        Native.Check(Native.aster_lease_serializer_foreign_new(upcalls.Runtime.Handle, upcalls.NativeHandle, out ulong h), "aster_lease_serializer_foreign_new");
        return new NativeLeaseSerializer(upcalls.Runtime, h);
    }

    /// <summary>
    /// Rust's in-memory reference serializer. With <paramref name="authorizer"/> non-null every
    /// transition is first put to it as a <c>LEASE_AUTHORIZE</c> decision (register the
    /// authorizer with <see cref="Upcalls.ServeLeaseAuthorizer"/>); a refusal (or no answer under
    /// fail-closed) is <c>Denied(Unauthorized)</c>.
    /// </summary>
    public static NativeLeaseSerializer Memory(Runtime runtime, Upcalls? authorizer = null)
    {
        Native.Check(Native.aster_lease_serializer_memory_new(runtime.Handle, authorizer?.NativeHandle ?? 0, out ulong h), "aster_lease_serializer_memory_new");
        return new NativeLeaseSerializer(runtime, h);
    }

    /// <summary>One ACQUIRE at the serializer; <paramref name="timing"/> null = the defaults.</summary>
    public async Task<AcquireResult> AcquireAsync(string resource, string holder, LeaseTiming? timing = null, CancellationToken cancellationToken = default)
    {
        ThrowIfDisposed();
        var t = timing ?? default;
        ulong op;
        using (var scope = new NativeScope())
        {
            Native.Check(Native.aster_lease_acquire(_runtime.Handle, _handle, scope.Utf8(resource), scope.Utf8(holder),
                LeaseTiming.Ms(t.Ttl), LeaseTiming.Ms(t.RenewInterval), LeaseTiming.Ms(t.HolderMargin), 0, out op), "aster_lease_acquire");
        }
        var (ev, data) = await Operations.AwaitAsync(_runtime, op, EventKind.LeaseAcquired, "aster_lease_acquire", cancellationToken).ConfigureAwait(false);
        TransitionOutcome outcome;
        try
        {
            outcome = Ucv1.DecodeTransitionOutcome(data);
        }
        catch
        {
            if (ev.handle != 0) Native.aster_lease_free(_runtime.Handle, ev.handle); // never leak a granted handle
            throw;
        }
        switch (outcome)
        {
            case TransitionOutcome.Granted g:
                return new AcquireResult.Granted(new HeldLease(_runtime, ev.handle, g.Fence));
            case TransitionOutcome.Denied d:
                if (ev.handle != 0) Native.aster_lease_free(_runtime.Handle, ev.handle);
                return new AcquireResult.Denied(d.Reason, d.Snapshot);
            default:
                if (ev.handle != 0) Native.aster_lease_free(_runtime.Handle, ev.handle);
                throw new IrohException($"serializer answered {outcome} to an acquire");
        }
    }

    /// <summary>Advisory snapshot of <paramref name="resource"/>.</summary>
    public async Task<LeaseSnapshot> SnapshotAsync(string resource, CancellationToken cancellationToken = default)
    {
        ThrowIfDisposed();
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_lease_snapshot(_runtime.Handle, _handle, scope.Utf8(resource), 0, out op), "aster_lease_snapshot");
        var (_, data) = await Operations.AwaitAsync(_runtime, op, EventKind.BytesResult, "aster_lease_snapshot", cancellationToken).ConfigureAwait(false);
        return Ucv1.DecodeLeaseSnapshot(data);
    }

    /// <summary>REVOKE <paramref name="resource"/> on behalf of <paramref name="authority"/> (advances the fence).</summary>
    public async Task<TransitionOutcome> RevokeAsync(string resource, string authority, CancellationToken cancellationToken = default)
    {
        ThrowIfDisposed();
        ulong op;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_lease_revoke(_runtime.Handle, _handle, scope.Utf8(resource), scope.Utf8(authority), 0, out op), "aster_lease_revoke");
        var (_, data) = await Operations.AwaitAsync(_runtime, op, EventKind.BytesResult, "aster_lease_revoke", cancellationToken).ConfigureAwait(false);
        return Ucv1.DecodeTransitionOutcome(data);
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);

    /// <summary>Frees the handle; leases acquired from it keep renewing.</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) == 0)
            Native.aster_lease_serializer_free(_runtime.Handle, _handle);
    }
}

/// <summary>
/// A held lease (<c>aster_lease_t</c>, C ABI 4.1): fence access gated by the holder's self-limit,
/// background renewals at the serializer, and a status watch.
///
/// <para><see cref="Dispose"/> stops renewing without releasing (the lease lapses by TTL);
/// <see cref="ReleaseAsync"/> and <see cref="DisposeAsync"/> hand it off cleanly (advancing the fence).</para>
/// </summary>
public sealed class HeldLease : IDisposable, IAsyncDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _done;

    internal HeldLease(Runtime runtime, ulong handle, Fence granted)
    {
        _runtime = runtime;
        _handle = handle;
        GrantedFence = granted;
    }

    /// <summary>The fence issued at acquire (resource, epoch, holder).</summary>
    public Fence GrantedFence { get; }

    /// <summary>
    /// The fence for a protected action, read at send time (do not cache it); null once the holder
    /// must stop acting (fenced, lost, released, or past its self-limit).
    /// </summary>
    public Fence? CurrentFence()
    {
        if (Volatile.Read(ref _done) != 0) return null;
        int rc = Native.aster_lease_fence(_runtime.Handle, _handle, out ulong epoch);
        if (rc == (int)Status.AlreadyClosed || rc == (int)Status.NotFound) return null;
        Native.Check(rc, "aster_lease_fence");
        return new Fence(GrantedFence.Resource, epoch, GrantedFence.Holder);
    }

    public LeaseStatus CurrentStatus()
    {
        if (Volatile.Read(ref _done) != 0) return LeaseStatus.Released;
        Native.Check(Native.aster_lease_status(_runtime.Handle, _handle, out uint s), "aster_lease_status");
        return (LeaseStatus)s;
    }

    /// <summary>Completes when the lease leaves <c>Held</c> (fenced, lost or released).</summary>
    public async Task<LeaseStatus> WatchAsync(CancellationToken cancellationToken = default)
    {
        Native.Check(Native.aster_lease_watch(_runtime.Handle, _handle, 0, out ulong op), "aster_lease_watch");
        var (ev, _) = await Operations.AwaitAsync(_runtime, op, EventKind.LeaseStatus, "aster_lease_watch", cancellationToken).ConfigureAwait(false);
        // Freed before the watch fired: the event carries a non-zero status.
        return ev.status != 0 ? LeaseStatus.Released : (LeaseStatus)ev.flags;
    }

    /// <summary>
    /// Stops renewing and RELEASEs at the serializer (advances the fence; this holder's in-flight
    /// writes are rejected from here on). Idempotent. A serializer failure throws; the lease then lapses by TTL.
    /// </summary>
    public async Task ReleaseAsync(CancellationToken cancellationToken = default)
    {
        if (Interlocked.Exchange(ref _done, 1) != 0) return;
        Native.Check(Native.aster_lease_release(_runtime.Handle, _handle, 0, out ulong op), "aster_lease_release");
        await Operations.AwaitAsync(_runtime, op, EventKind.UnitResult, "aster_lease_release", cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Best-effort graceful release when still held.</summary>
    public async ValueTask DisposeAsync()
    {
        try { await ReleaseAsync().ConfigureAwait(false); }
        catch (IrohException) { /* lapses by TTL */ }
    }

    /// <summary>Stops renewing without releasing (the lease lapses at the serializer by TTL).</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _done, 1) == 0)
            Native.aster_lease_free(_runtime.Handle, _handle);
    }
}

/// <summary>
/// A Rust <c>FencedResource</c> implemented by an <see cref="IFencedResource"/> served on an
/// <see cref="Upcalls"/> table (<c>aster_fenced_resource_t</c>, C ABI 4.1).
/// </summary>
public sealed class NativeFencedResource : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _disposed;

    private NativeFencedResource(Runtime runtime, ulong handle)
    {
        _runtime = runtime;
        _handle = handle;
    }

    /// <summary>Register the implementation with <see cref="Upcalls.ServeFencedResource"/>.</summary>
    public static NativeFencedResource Foreign(Upcalls upcalls)
    {
        Native.Check(Native.aster_fenced_resource_foreign_new(upcalls.Runtime.Handle, upcalls.NativeHandle, out ulong h), "aster_fenced_resource_foreign_new");
        return new NativeFencedResource(upcalls.Runtime, h);
    }

    /// <summary>
    /// Applies <paramref name="op"/> under <paramref name="fence"/> as <paramref name="writer"/>.
    /// Completes with the applied output or the fence rejection; throws <see cref="IrohException"/>
    /// when the outcome is <b>unknown</b> (timeout, handler error, closed table): a write cannot be failed open.
    /// </summary>
    public async Task<MutateResult> MutateAsync(Fence fence, string writer, byte[] op, CancellationToken cancellationToken = default)
    {
        ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);
        ulong opId;
        using (var scope = new NativeScope())
            Native.Check(Native.aster_fenced_mutate(_runtime.Handle, _handle, scope.Utf8(fence.Resource), fence.Epoch,
                scope.Utf8(fence.Holder), scope.Utf8(writer), scope.Bytes(op), 0, out opId), "aster_fenced_mutate");
        var (_, data) = await Operations.AwaitAsync(_runtime, opId, EventKind.BytesResult, "aster_fenced_mutate", cancellationToken).ConfigureAwait(false);
        return Ucv1.DecodeFencedMutateResponse(data);
    }

    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) == 0)
            Native.aster_fenced_resource_free(_runtime.Handle, _handle);
    }
}
