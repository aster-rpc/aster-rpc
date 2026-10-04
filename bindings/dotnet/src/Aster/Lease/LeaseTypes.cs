using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Aster.Lease;

/// <summary>Why a lease transition was refused (UCv1 codes 1..6).</summary>
public enum DenyReason : byte
{
    HeldByOther = 1,
    EpochMismatch = 2,
    NotHolder = 3,
    Expired = 4,
    NotHeld = 5,
    Unauthorized = 6,
}

/// <summary>
/// The fencing token a lease holder stamps on every protected action: <c>Epoch</c> is monotonic per
/// resource and never reused; <c>Holder</c> is the identity the epoch was issued to.
/// </summary>
public sealed record Fence(string Resource, ulong Epoch, string Holder);

/// <summary>
/// Observer view of a serializer row. Advisory: stale the moment it is returned; never gate a
/// write on it. <c>Holder</c> and <c>RemainingMs</c> are null when absent.
/// </summary>
public sealed record LeaseSnapshot(string Resource, ulong Epoch, string? Holder, bool Held, ulong? RemainingMs);

/// <summary>A requested lease transition (the four verbs of the lease state machine).</summary>
public abstract record LeaseOp
{
    private LeaseOp() { }

    /// <summary>The identity performing the transition (for authorization).</summary>
    public abstract string Actor { get; }

    /// <summary>ACQUIRE (legal on a free or expired row).</summary>
    public sealed record Acquire(string Candidate, ulong TtlMs) : LeaseOp
    {
        public override string Actor => Candidate;
    }

    /// <summary>RENEW by the current holder at its epoch.</summary>
    public sealed record Renew(string Holder, ulong Epoch) : LeaseOp
    {
        public override string Actor => Holder;
    }

    /// <summary>RELEASE by the current holder (advances the fence).</summary>
    public sealed record Release(string Holder, ulong Epoch) : LeaseOp
    {
        public override string Actor => Holder;
    }

    /// <summary>REVOKE by an authority (advances the fence).</summary>
    public sealed record Revoke(string Authority) : LeaseOp
    {
        public override string Actor => Authority;
    }
}

/// <summary>Result of a transition at a lease serializer. A refusal is information, not a fault.</summary>
public abstract record TransitionOutcome
{
    private TransitionOutcome() { }

    /// <summary>ACQUIRE succeeded: the issued fence.</summary>
    public sealed record Granted(Fence Fence) : TransitionOutcome;
    public sealed record Renewed : TransitionOutcome;
    public sealed record Released : TransitionOutcome;
    public sealed record Revoked : TransitionOutcome;

    /// <summary>The predicate refused; the snapshot (post-decision) is a back-off hint.</summary>
    public sealed record Denied(DenyReason Reason, LeaseSnapshot Snapshot) : TransitionOutcome;
}

/// <summary>
/// Why a protected action was rejected at the resource. <c>Carried</c> / <c>Current</c> are the
/// epochs for <see cref="FenceRejectionReason.StaleEpoch"/> and <see cref="FenceRejectionReason.FutureEpoch"/>, 0 otherwise.
/// </summary>
public enum FenceRejectionReason : byte
{
    StaleEpoch = 1,
    NotHolder = 2,
    NotHeld = 3,
    FutureEpoch = 4,
}

/// <summary>A fenced mutation the fence rule refused.</summary>
public sealed record FenceRejection(FenceRejectionReason Reason, ulong Carried, ulong Current)
{
    public static FenceRejection StaleEpoch(ulong carried, ulong current) => new(FenceRejectionReason.StaleEpoch, carried, current);
    public static FenceRejection FutureEpoch(ulong carried, ulong current) => new(FenceRejectionReason.FutureEpoch, carried, current);
    public static FenceRejection NotHolder() => new(FenceRejectionReason.NotHolder, 0, 0);
    public static FenceRejection NotHeld() => new(FenceRejectionReason.NotHeld, 0, 0);

    /// <summary>
    /// The fence rule (<c>check_fence</c>): accept iff the carried epoch equals the row's, the row is
    /// held, and <paramref name="writer"/> is its holder. Evaluate it inside the resource's own
    /// serialization point, atomically with the write. Returns null when the fence passes.
    /// </summary>
    public static FenceRejection? Check(ulong rowEpoch, bool rowHeld, string? rowHolder, Fence fence, string writer)
    {
        if (fence.Epoch < rowEpoch) return StaleEpoch(fence.Epoch, rowEpoch);
        if (fence.Epoch > rowEpoch) return FutureEpoch(fence.Epoch, rowEpoch);
        if (!rowHeld) return NotHeld();
        if (rowHolder == null || rowHolder != writer) return NotHolder();
        return null;
    }
}

/// <summary>Result of a fenced mutation: applied (with output bytes) or rejected by the fence rule.</summary>
public abstract record MutateResult
{
    private MutateResult() { }

    /// <summary>The mutation ran; <c>Output</c> is application-defined.</summary>
    public sealed record Applied(byte[] Output) : MutateResult
    {
        public bool Equals(Applied? other) => other != null && Output.AsSpan().SequenceEqual(other.Output);
        public override int GetHashCode() => Output.Length;
    }

    /// <summary>The fence rule refused the mutation. Surface it loudly; never retry silently.</summary>
    public sealed record Rejected(FenceRejection Rejection) : MutateResult;
}

/// <summary>Holder-side lease status (<c>aster_lease_status_t</c>).</summary>
public enum LeaseStatus : uint
{
    Held = 0,
    /// <summary>Released by this holder.</summary>
    Released = 1,
    /// <summary>A renewal was refused: someone holds a newer epoch, or the lease was revoked.</summary>
    Fenced = 2,
    /// <summary>The self-limit passed without a successful renewal.</summary>
    Lost = 3,
}

/// <summary>
/// A lease serialization point implemented in .NET and served to Rust by upcall (kinds
/// <c>LEASE_TRANSITION</c> / <c>LEASE_SNAPSHOT</c>). Apply each transition atomically under your
/// own lock or CAS, evaluating expiry on your own clock. A thrown exception is a backend error
/// (the acquire / renew / release fails), never a denial. Cancelled via the token when Rust stops
/// waiting (timeout, abandoned caller, shutdown).
/// </summary>
public interface ILeaseSerializer
{
    /// <summary>Apply one transition. Predicate refusals are <see cref="TransitionOutcome.Denied"/>.</summary>
    ValueTask<TransitionOutcome> TransitionAsync(string resource, LeaseOp op, CancellationToken cancellationToken);

    /// <summary>Advisory snapshot (hints only).</summary>
    ValueTask<LeaseSnapshot> SnapshotAsync(string resource, CancellationToken cancellationToken);
}

/// <summary>
/// May <c>op</c> be attempted at all? Served by upcall (kind <c>LEASE_AUTHORIZE</c>, a decision):
/// <c>false</c> yields <c>Denied(Unauthorized)</c>; an exception or no answer applies the kind's
/// failure mode (fail-closed by default).
/// </summary>
public interface ILeaseAuthorizer
{
    ValueTask<bool> AuthorizeAsync(LeaseOp op, CancellationToken cancellationToken);
}

/// <summary>
/// The resource-side half of a lease, implemented in .NET and served by upcall (kind
/// <c>FENCED_MUTATE</c>). Perform <c>op</c> iff the fence rule (<see cref="FenceRejection.Check"/>)
/// passes, atomically with the check. An exception means the outcome is unknown to the Rust caller.
/// </summary>
public interface IFencedResource
{
    ValueTask<MutateResult> MutateAsync(Fence fence, string writer, byte[] op, CancellationToken cancellationToken);
}

/// <summary>The answer of <see cref="NativeLeaseSerializer.AcquireAsync(string, string, ulong, ulong, ulong, CancellationToken)"/>.</summary>
public abstract record AcquireResult
{
    private AcquireResult() { }

    /// <summary>Granted: a held lease that renews in the background until released or disposed.</summary>
    public sealed record Granted(HeldLease Lease) : AcquireResult;

    /// <summary>Refused (typically <c>HeldByOther</c>); back off using <c>Snapshot.RemainingMs</c> plus jitter.</summary>
    public sealed record Denied(DenyReason Reason, LeaseSnapshot Snapshot) : AcquireResult;
}
