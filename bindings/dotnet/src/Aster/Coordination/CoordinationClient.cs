using System;
using System.Collections.Generic;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Aster.Lease;
using Aster.Upcall;

namespace Aster.Coordination;

/// <summary>How to open a <see cref="CoordinationClient"/>.</summary>
public sealed record CoordinationClientOptions
{
    /// <summary>The coordination group id: exactly 32 bytes.</summary>
    public required byte[] GroupId { get; init; }

    /// <summary>A read-only client supports reads and the ids only; everything journaled is <c>UNSUPPORTED</c>. It needs no journal.</summary>
    public bool ReadOnly { get; init; }

    /// <summary>Open-credential tickets (base58): routing hints only, they grant nothing.</summary>
    public IReadOnlyList<string> SeedTickets { get; init; } = Array.Empty<string>();

    /// <summary>Journal directory: required for a durable client, ignored when <see cref="ReadOnly"/>. One open client per actor/group journal.</summary>
    public string? JournalDir { get; init; }
}

/// <summary>Completion plumbing for the coordination operations: failure events become <see cref="CoordinationException"/>.</summary>
internal static class CoordOps
{
    internal static CoordinationException ToException(Runtime rt, in Event ev, string what)
    {
        string detail = string.Empty;
        if (ev.data_ptr != IntPtr.Zero && ev.data_len != UIntPtr.Zero)
            detail = new Bytes { ptr = ev.data_ptr, len = ev.data_len }.ToUtf8String();
        if (ev.buffer != 0) Native.aster_buffer_release(rt.Handle, ev.buffer);
        var status = ev.status != 0 ? (Status)ev.status : Status.Internal;
        int code = ev.error_code;
        var kind = code is >= 1 and <= 10 ? (CoordinationErrorKind)code : CoordinationErrorKind.Other;
        uint rpc = kind == CoordinationErrorKind.Rpc ? ev.flags : 0;
        string msg = $"{what}: {kind}" + (kind == CoordinationErrorKind.Rpc ? $" (rpc {rpc})" : "") + (detail.Length == 0 ? "" : $": {detail}");
        return new CoordinationException(kind, rpc, status, msg);
    }

    /// <summary>Checks a synchronous status: a closed handle is <see cref="CoordinationErrorKind.Closed"/>.</summary>
    internal static void Check(int rc, string what)
    {
        if (rc == 0) return;
        var e = IrohException.FromStatus(rc, what);
        if (rc == (int)Status.AlreadyClosed) throw new CoordinationException(CoordinationErrorKind.Closed, 0, Status.AlreadyClosed, e.Message);
        throw e;
    }

    /// <summary>
    /// Awaits operation <paramref name="op"/>. Cancelling <paramref name="ct"/> abandons the wait and suppresses
    /// the completion event (<c>aster_operation_cancel</c>); it never aborts an operation Rust already started
    /// (a half-sent mutation is the journal's to resolve), so the outcome stays recoverable through
    /// <c>UnacknowledgedAsync</c> / <c>ResumeAsync</c>.
    /// </summary>
    internal static async Task<(Event Ev, byte[] Data)> AwaitAsync(Runtime rt, ulong op, EventKind expected, string what, CancellationToken ct)
    {
        Event ev;
        try
        {
            ev = await rt.WaitForAsync(op, ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            try { rt.Cancel(op); } catch (IrohException) { /* already completed */ }
            throw;
        }
        if (ev.kind == (uint)EventKind.Error) throw ToException(rt, in ev, what);
        if (ev.kind != (uint)expected)
        {
            byte[] stray = EventError.TakePayload(rt, in ev);
            throw new IrohException($"{what}: unexpected event {(EventKind)ev.kind} ({stray.Length} bytes)");
        }
        return (ev, EventError.TakePayload(rt, in ev));
    }
}

/// <summary>
/// A resolved mutation: the group's committed answer, journaled by the client until you
/// <see cref="AcknowledgeAsync"/> it. Take no further mutation before acknowledging (the client refuses with
/// <see cref="CoordinationErrorKind.AwaitingAck"/>). <see cref="Dispose"/> drops the handle <b>without</b>
/// acknowledging: the result stays journaled and is redelivered by <see cref="CoordinationClient.UnacknowledgedAsync"/>.
/// A typed rejection is a result (<see cref="MutationReply.IsRejected"/>), not an exception.
/// </summary>
public sealed class ResolvedMutation : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _consumed;

    internal ResolvedMutation(Runtime runtime, ulong handle, MutationReply reply)
    {
        _runtime = runtime;
        _handle = handle;
        Reply = reply;
    }

    public MutationReply Reply { get; }

    /// <summary>
    /// Acknowledges the result (consumes the handle, also when the client was closed meanwhile: the result then
    /// stays journaled). If the acknowledgement fails it stays journaled and is redelivered.
    /// </summary>
    public async Task AcknowledgeAsync(CancellationToken cancellationToken = default)
    {
        if (Interlocked.Exchange(ref _consumed, 1) != 0) throw new InvalidOperationException("this result was already acknowledged or disposed");
        CoordOps.Check(Native.aster_coordination_resolved_acknowledge(_runtime.Handle, _handle, 0, out ulong op), "aster_coordination_resolved_acknowledge");
        await CoordOps.AwaitAsync(_runtime, op, EventKind.UnitResult, "aster_coordination_resolved_acknowledge", cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Drops the handle without acknowledging (the result stays journaled).</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _consumed, 1) == 0)
            Native.aster_coordination_resolved_free(_runtime.Handle, _handle);
    }
}

/// <summary>The answer of <see cref="CoordinationClient.AcquireLeaseAsync"/>.</summary>
public abstract record CoordinationLeaseAcquire
{
    private CoordinationLeaseAcquire() { }

    /// <summary>Granted: a held lease that renews in the background until released or disposed.</summary>
    public sealed record Granted(CoordinationLease Lease) : CoordinationLeaseAcquire;

    /// <summary>Denied (typically held by another): <c>Snapshot</c> carries the holder and a back-off hint.</summary>
    public sealed record Denied(DenyReason Reason, LeaseSnapshot Snapshot) : CoordinationLeaseAcquire;
}

/// <summary>
/// A coordination client on a node (<c>aster_coordination_client_t</c>, C ABI 4.4): the Rust client wrapped, never
/// ported. Journaling, redirect routing, the inbound validation boundary and the delivery contract stay in Rust.
///
/// <para><b>Delivery contract.</b> Mutations are sequenced and journaled; each returns a <see cref="ResolvedMutation"/> you
/// must acknowledge before the next. After a crash (or a close racing a resolve) the unacknowledged result is redelivered
/// by <see cref="UnacknowledgedAsync"/> on reopen; an unresolved request resumes with <see cref="ResumeAsync"/>.</para>
///
/// <para><b>Close.</b> <see cref="DisposeAsync"/> closes and frees the handle at once and returns: new calls are refused with
/// <see cref="CoordinationErrorKind.Closed"/>, outstanding resolved / lease / manager handles go inert, and operations already in
/// flight complete normally. The journal lifetime lock is released only when the last of them ends (a lease CAS or a mutation may
/// be driven for its full retry budget), so a reopen right after can still see <see cref="CoordinationErrorKind.JournalLocked"/>.</para>
/// </summary>
public sealed class CoordinationClient : IAsyncDisposable, IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private readonly bool _readOnly;
    private int _disposed;

    // A disposed client is closed: refuse with Closed rather than a NOT_FOUND from the freed handle.
    private ulong Live => Volatile.Read(ref _disposed) != 0
        ? throw new CoordinationException(CoordinationErrorKind.Closed, 0, Status.AlreadyClosed, "the coordination client is closed")
        : _handle;

    private CoordinationClient(Runtime runtime, ulong handle, bool readOnly, byte[] groupId, byte[] localNodeId)
    {
        _runtime = runtime;
        _handle = handle;
        _readOnly = readOnly;
        GroupId = groupId;
        LocalNodeId = localNodeId;
        runtime.Retain();
    }

    /// <summary>The coordination group (32 bytes).</summary>
    public byte[] GroupId { get; }

    /// <summary>This client's node id (32 bytes).</summary>
    public byte[] LocalNodeId { get; }

    public bool ReadOnly => _readOnly;

    internal Runtime Runtime => _runtime;
    internal ulong NativeHandle => Live;

    // `&`/`fixed` on locals is not allowed in async methods (CS9123), so the native calls live in these
    // synchronous helpers.
    private static unsafe ulong StartOpen(Runtime rt, Node node, CoordinationClientOptions options)
    {
        using var scope = new NativeScope();
        CoordinationClientOptionsNative o = default;
        Native.Check(Native.aster_coordination_client_options_init(&o), "aster_coordination_client_options_init");
        o.flags = options.ReadOnly ? Native.CoordinationClientReadOnly : 0;
        o.group_id = (IntPtr)scope.Ptr(options.GroupId);
        o.seed_tickets = scope.Utf8List(options.SeedTickets);
        o.journal_dir = scope.Utf8(options.ReadOnly ? null : options.JournalDir);
        Native.Check(Native.aster_coordination_client_open(rt.Handle, node.Handle.Value, &o, 0, out ulong op), "aster_coordination_client_open");
        return op;
    }

    private static unsafe (byte[] Group, byte[] Local) ReadIds(Runtime rt, ulong handle)
    {
        byte[] group = new byte[32], local = new byte[32];
        fixed (byte* g = group)
        fixed (byte* l = local)
            Native.Check(Native.aster_coordination_client_ids(rt.Handle, handle, g, l), "aster_coordination_client_ids");
        return (group, local);
    }

    /// <summary>
    /// Opens a client on <paramref name="node"/>. A durable client takes the journal lifetime lock: a second open over the
    /// same journal fails with <see cref="CoordinationErrorKind.JournalLocked"/>.
    /// </summary>
    public static async Task<CoordinationClient> OpenAsync(Node node, CoordinationClientOptions options, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(node);
        ArgumentNullException.ThrowIfNull(options);
        if (options.GroupId.Length != 32) throw new ArgumentException("GroupId must be 32 bytes", nameof(options));
        if (!options.ReadOnly && string.IsNullOrEmpty(options.JournalDir)) throw new ArgumentException("JournalDir is required for a durable client", nameof(options));
        var rt = node.Runtime;
        ulong op = StartOpen(rt, node, options);
        var (ev, _) = await CoordOps.AwaitAsync(rt, op, EventKind.CoordinationClientOpened, "aster_coordination_client_open", cancellationToken).ConfigureAwait(false);
        (byte[] group, byte[] local) = ReadIds(rt, ev.handle);
        return new CoordinationClient(rt, ev.handle, options.ReadOnly, group, local);
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// <summary>The 16 raw bytes identifying this client's journal (the id other clients can retire).</summary>
    public async Task<byte[]> ClientIdAsync(CancellationToken cancellationToken = default)
    {
        CoordOps.Check(Native.aster_coordination_client_id(_runtime.Handle, Live, 0, out ulong op), "aster_coordination_client_id");
        var (_, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.BytesResult, "aster_coordination_client_id", cancellationToken).ConfigureAwait(false);
        return data;
    }

    /// <summary>
    /// Submits one journaled mutation and returns its resolved result, which you must acknowledge. A refusal by the
    /// replicated ACL is <see cref="CoordinationException"/> (<c>Rpc</c>, <see cref="Status.PermissionDenied"/>) and stays
    /// journaled as unresolved; a committed typed rejection is a result.
    /// </summary>
    public async Task<ResolvedMutation> MutateAsync(MutationRequest request, CancellationToken cancellationToken = default)
    {
        byte[] d0 = CoordinationCodec.EncodeMutationRequest(request);
        ulong op;
        using (var scope = new NativeScope())
            CoordOps.Check(Native.aster_coordination_mutate(_runtime.Handle, Live, scope.Bytes(d0), 0, out op), "aster_coordination_mutate");
        return (await ResolvedAsync(op, "aster_coordination_mutate", cancellationToken).ConfigureAwait(false))
            ?? throw new IrohException("aster_coordination_mutate: no resolved result");
    }

    private async Task<ResolvedMutation?> ResolvedAsync(ulong op, string what, CancellationToken ct)
    {
        var (ev, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.CoordinationResolved, what, ct).ConfigureAwait(false);
        if (ev.handle == 0) return null;
        try
        {
            return new ResolvedMutation(_runtime, ev.handle, CoordinationCodec.DecodeMutationReply(data));
        }
        catch
        {
            Native.aster_coordination_resolved_free(_runtime.Handle, ev.handle);
            throw;
        }
    }

    /// <summary>The result resolved but not yet acknowledged (null = none): redelivery after a reopen or a close that raced a resolve.</summary>
    public async Task<ResolvedMutation?> UnacknowledgedAsync(CancellationToken cancellationToken = default)
    {
        CoordOps.Check(Native.aster_coordination_unacknowledged(_runtime.Handle, Live, 0, out ulong op), "aster_coordination_unacknowledged");
        return await ResolvedAsync(op, "aster_coordination_unacknowledged", cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Resumes the unresolved request of the journal with the same bytes (null = nothing to resume).</summary>
    public async Task<ResolvedMutation?> ResumeAsync(CancellationToken cancellationToken = default)
    {
        CoordOps.Check(Native.aster_coordination_resume(_runtime.Handle, Live, 0, out ulong op), "aster_coordination_resume");
        return await ResolvedAsync(op, "aster_coordination_resume", cancellationToken).ConfigureAwait(false);
    }

    public Task<ResolvedMutation> AcquireAsync(ResourceKey resource, TimeSpan ttl, CancellationToken ct = default)
        => MutateAsync(new MutationRequest.Acquire(resource, (ulong)ttl.TotalMilliseconds), ct);

    public Task<ResolvedMutation> RenewAsync(CoordFence fence, CancellationToken ct = default) => MutateAsync(new MutationRequest.Renew(fence), ct);

    public Task<ResolvedMutation> CompareAndSetAsync(CoordFence fence, ulong expectedValueRevision, byte[] newValue, CancellationToken ct = default)
        => MutateAsync(new MutationRequest.CompareAndSet(fence, expectedValueRevision, newValue), ct);

    public Task<ResolvedMutation> ReleaseAsync(CoordFence fence, CancellationToken ct = default) => MutateAsync(new MutationRequest.Release(fence), ct);

    public Task<ResolvedMutation> ReleaseWithValueAsync(CoordFence fence, ulong expectedValueRevision, byte[] finalValue, CancellationToken ct = default)
        => MutateAsync(new MutationRequest.ReleaseWithValue(fence, expectedValueRevision, finalValue), ct);

    public Task<ResolvedMutation> RevokeAsync(ResourceKey resource, ulong? expectedEpoch = null, CancellationToken ct = default)
        => MutateAsync(new MutationRequest.Revoke(resource, expectedEpoch), ct);

    public Task<ResolvedMutation> SetGrantAsync(ResourceKey resource, byte[] targetNodeId, ulong rights, ulong expectedAclRevision, CancellationToken ct = default)
        => MutateAsync(new MutationRequest.SetGrant(resource, targetNodeId, rights, expectedAclRevision), ct);

    public Task<ResolvedMutation> SetAdminAsync(byte[] targetNodeId, bool enabled, ulong expectedAdminRevision, CancellationToken ct = default)
        => MutateAsync(new MutationRequest.SetAdmin(targetNodeId, enabled, expectedAdminRevision), ct);

    public Task<ResolvedMutation> ChangeMembershipAsync(byte[] operationId, LogId? expectedMembershipLogId, IReadOnlyList<byte[]> desiredVoters,
        IReadOnlyList<byte[]> retainedLearners, IReadOnlyList<MemberSeed>? memberSeeds = null, CancellationToken ct = default)
        => MutateAsync(new MutationRequest.ChangeMembership(operationId, expectedMembershipLogId, desiredVoters, retainedLearners, memberSeeds ?? Array.Empty<MemberSeed>()), ct);

    public Task<ResolvedMutation> CancelMembershipAsync(byte[] operationId, uint expectedPhaseCode, CancellationToken ct = default)
        => MutateAsync(new MutationRequest.CancelMembership(operationId, expectedPhaseCode), ct);

    /// <summary>Retires another client of this actor (its journal id, 16 bytes) so its sequence stops blocking.</summary>
    public async Task<RetireReply> RetireOtherClientAsync(byte[] targetClientId, CancellationToken cancellationToken = default)
    {
        if (targetClientId.Length != 16) throw new ArgumentException("client id must be 16 bytes", nameof(targetClientId));
        ulong op;
        unsafe
        {
            fixed (byte* p = targetClientId)
                CoordOps.Check(Native.aster_coordination_retire_other_client(_runtime.Handle, Live, p, 0, out op), "aster_coordination_retire_other_client");
        }
        var (_, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.CoordinationResult, "aster_coordination_retire_other_client", cancellationToken).ConfigureAwait(false);
        return CoordinationCodec.DecodeRetireReply(data);
    }

    // ── reads (either client) ────────────────────────────────────────────────

    private async Task<byte[]> ReadAsync(ReadRequest request, CancellationToken ct)
    {
        byte[] d1 = CoordinationCodec.EncodeReadRequest(request);
        ulong op;
        using (var scope = new NativeScope())
            CoordOps.Check(Native.aster_coordination_read(_runtime.Handle, Live, scope.Bytes(d1), 0, out op), "aster_coordination_read");
        var (_, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.CoordinationResult, "aster_coordination_read", ct).ConfigureAwait(false);
        return data;
    }

    public async Task<ResourceSnapshot> ReadLinearizableAsync(ResourceKey resource, CancellationToken ct = default)
        => CoordinationCodec.DecodeReadReply(await ReadAsync(new ReadRequest.ReadLinearizable(resource), ct).ConfigureAwait(false));

    public async Task<AclStatus> InspectAclAsync(ResourceKey resource, CancellationToken ct = default)
        => CoordinationCodec.DecodeAclStatus(await ReadAsync(new ReadRequest.InspectAcl(resource), ct).ConfigureAwait(false));

    public async Task<AdminStatus> InspectAdminsAsync(CancellationToken ct = default)
        => CoordinationCodec.DecodeAdminStatus(await ReadAsync(new ReadRequest.InspectAdmins(), ct).ConfigureAwait(false));

    /// <summary>The status of a membership change (<c>operationId</c> null = the current one).</summary>
    public async Task<MembershipStatus> MembershipStatusAsync(byte[]? operationId = null, CancellationToken ct = default)
        => CoordinationCodec.DecodeMembershipStatus(await ReadAsync(new ReadRequest.MembershipStatus(operationId), ct).ConfigureAwait(false));

    public async Task<GroupStatus> GroupStatusAsync(CancellationToken ct = default)
        => CoordinationCodec.DecodeGroupStatus(await ReadAsync(new ReadRequest.GroupStatus(), ct).ConfigureAwait(false));

    public async Task<ClientProbe> ProbeClientAsync(byte[] targetNodeId, byte[] targetClientId, CancellationToken ct = default)
        => CoordinationCodec.DecodeClientProbe(await ReadAsync(new ReadRequest.ProbeClient(targetNodeId, targetClientId), ct).ConfigureAwait(false));

    /// <summary>Audit records after <paramref name="after"/> (null = from the floor), at most <paramref name="limit"/> (1..=10000).</summary>
    public async Task<IReadOnlyList<AuditRecord>> AuditStreamAsync(LogId? after, uint limit, CancellationToken ct = default)
        => CoordinationCodec.DecodeAuditRecords(await ReadAsync(new ReadRequest.AuditStream(after, limit), ct).ConfigureAwait(false));

    public async Task<RecoveryExport> RecoveryExportAsync(CancellationToken ct = default)
        => CoordinationCodec.DecodeRecoveryExport(await ReadAsync(new ReadRequest.RecoveryExport(), ct).ConfigureAwait(false));

    /// <summary>Writes a recovery export to <paramref name="path"/> and returns its manifest.</summary>
    public async Task<RecoveryExportManifest> RecoveryExportToFileAsync(string path, CancellationToken cancellationToken = default)
    {
        ulong op;
        using (var scope = new NativeScope())
            CoordOps.Check(Native.aster_coordination_recovery_export_to_file(_runtime.Handle, Live, scope.Utf8(path), 0, out op), "aster_coordination_recovery_export_to_file");
        var (_, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.CoordinationResult, "aster_coordination_recovery_export_to_file", cancellationToken).ConfigureAwait(false);
        return CoordinationCodec.DecodeRecoveryExportManifest(data);
    }

    // ── leases ───────────────────────────────────────────────────────────────

    /// <summary>
    /// Tries to acquire <paramref name="resource"/> as this node: granted = a lease that renews in the background, denied =
    /// the holder and a back-off hint. <paramref name="timing"/> null = the defaults (see <see cref="LeaseTiming"/>).
    /// </summary>
    public async Task<CoordinationLeaseAcquire> AcquireLeaseAsync(ResourceKey resource, LeaseTiming? timing = null, CancellationToken cancellationToken = default)
    {
        var t = timing ?? default;
        ulong op;
        using (var scope = new NativeScope())
            CoordOps.Check(Native.aster_coordination_lease_acquire(_runtime.Handle, Live, scope.Utf8(resource.Namespace), scope.Bytes(resource.Key),
                LeaseTiming.Ms(t.Ttl), LeaseTiming.Ms(t.RenewInterval), LeaseTiming.Ms(t.HolderMargin), 0, out op), "aster_coordination_lease_acquire");
        var (ev, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.CoordinationLeaseAcquired, "aster_coordination_lease_acquire", cancellationToken).ConfigureAwait(false);
        if (ev.handle != 0)
        {
            try
            {
                return new CoordinationLeaseAcquire.Granted(new CoordinationLease(_runtime, ev.handle, CoordinationCodec.DecodeCoordinationFence(data)));
            }
            catch
            {
                Native.aster_coordination_lease_free(_runtime.Handle, ev.handle);
                throw;
            }
        }
        var denied = Ucv1.DecodeTransitionOutcome(data) as TransitionOutcome.Denied
            ?? throw new IrohException("a coordination lease acquire without a handle must be denied");
        return new CoordinationLeaseAcquire.Denied(denied.Reason, denied.Snapshot);
    }

    // ── disposal ─────────────────────────────────────────────────────────────

    /// <summary>Closes and frees the handle (see the class remarks: it returns at once; in-flight operations complete).</summary>
    public ValueTask DisposeAsync()
    {
        Dispose();
        return ValueTask.CompletedTask;
    }

    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) == 0)
        {
            Native.aster_coordination_client_free(_runtime.Handle, _handle);
            _runtime.Release();
        }
    }
}

/// <summary>
/// A held coordination lease (<c>aster_coordination_lease_t</c>): background renewals, a fence read at send time, and
/// CAS / release writes. <see cref="Dispose"/> stops renewing without releasing (the lease lapses by TTL).
///
/// <para><b>Release is not prompt.</b> An in-flight <see cref="CompareAndSetAsync"/> is one journaled mutation the driver may retry for its
/// full budget; <see cref="ReleaseAsync"/>, <see cref="ReleaseWithValueAsync"/> and a client close wait for it, and the journal lock is held
/// until it ends. Give release a generous <see cref="CancellationToken"/> (cancelling abandons the wait, not the release).</para>
/// </summary>
public sealed class CoordinationLease : IDisposable
{
    private readonly Runtime _runtime;
    private readonly ulong _handle;
    private int _consumed;

    internal CoordinationLease(Runtime runtime, ulong handle, CoordFence granted)
    {
        _runtime = runtime;
        _handle = handle;
        GrantedFence = granted;
    }

    /// <summary>The fence issued at acquire.</summary>
    public CoordFence GrantedFence { get; }

    /// <summary>The current fence, read at send time (never cache it); null once the holder must stop acting (fenced, lost, released, closed).</summary>
    public unsafe CoordFence? CurrentFence()
    {
        UIntPtr need = UIntPtr.Zero;
        int rc = Native.aster_coordination_lease_fence(_runtime.Handle, _handle, null, UIntPtr.Zero, &need);
        for (int attempt = 0; attempt < 4 && rc == (int)Status.BufferTooSmall; attempt++)
        {
            byte[] buf = new byte[(int)(nuint)need];
            fixed (byte* p = buf)
                rc = Native.aster_coordination_lease_fence(_runtime.Handle, _handle, p, (UIntPtr)buf.Length, &need);
            if (rc == 0) return CoordinationCodec.DecodeCoordinationFence(buf.AsSpan(0, (int)(nuint)need).ToArray());
        }
        if (rc == (int)Status.AlreadyClosed || rc == (int)Status.NotFound) return null;
        CoordOps.Check(rc, "aster_coordination_lease_fence");
        return null;
    }

    /// <summary>The holder-side status (polled: the Rust lease has no watch). Reports <c>Released</c> once released or closed.</summary>
    public LeaseStatus CurrentStatus()
    {
        int rc = Native.aster_coordination_lease_status(_runtime.Handle, _handle, out uint s);
        if (rc == (int)Status.AlreadyClosed || rc == (int)Status.NotFound) return LeaseStatus.Released;
        CoordOps.Check(rc, "aster_coordination_lease_status");
        return (LeaseStatus)s;
    }

    /// <summary>One journaled compare-and-set of the resource value under the lease's fence (value revision check).</summary>
    public async Task<MutationReply> CompareAndSetAsync(ulong expectedValueRevision, byte[] newValue, CancellationToken cancellationToken = default)
    {
        ulong op;
        using (var scope = new NativeScope())
            CoordOps.Check(Native.aster_coordination_lease_compare_and_set(_runtime.Handle, _handle, expectedValueRevision, scope.Bytes(newValue), 0, out op), "aster_coordination_lease_compare_and_set");
        var (_, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.CoordinationResult, "aster_coordination_lease_compare_and_set", cancellationToken).ConfigureAwait(false);
        return CoordinationCodec.DecodeMutationReply(data);
    }

    /// <summary>Stops renewing and releases at the group (consumes the lease; waits for an in-flight CAS).</summary>
    public async Task ReleaseAsync(CancellationToken cancellationToken = default)
    {
        Consume();
        CoordOps.Check(Native.aster_coordination_lease_release(_runtime.Handle, _handle, 0, out ulong op), "aster_coordination_lease_release");
        await CoordOps.AwaitAsync(_runtime, op, EventKind.UnitResult, "aster_coordination_lease_release", cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Releases with a final value (consumes the lease; waits for an in-flight CAS).</summary>
    public async Task<MutationReply> ReleaseWithValueAsync(ulong expectedValueRevision, byte[] finalValue, CancellationToken cancellationToken = default)
    {
        Consume();
        ulong op;
        using (var scope = new NativeScope())
            CoordOps.Check(Native.aster_coordination_lease_release_with_value(_runtime.Handle, _handle, expectedValueRevision, scope.Bytes(finalValue), 0, out op), "aster_coordination_lease_release_with_value");
        var (_, data) = await CoordOps.AwaitAsync(_runtime, op, EventKind.CoordinationResult, "aster_coordination_lease_release_with_value", cancellationToken).ConfigureAwait(false);
        return CoordinationCodec.DecodeMutationReply(data);
    }

    private void Consume()
    {
        if (Interlocked.Exchange(ref _consumed, 1) != 0) throw new InvalidOperationException("the lease was already released or disposed");
    }

    /// <summary>Stops renewing without releasing (the lease lapses at the group by TTL).</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _consumed, 1) == 0)
            Native.aster_coordination_lease_free(_runtime.Handle, _handle);
    }
}
