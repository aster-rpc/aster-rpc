using System;
using System.Collections.Generic;
using System.Linq;

namespace Aster.Coordination;

// Typed forms of the UCv1 D0-DE messages (design docs/_internal/bindings/coordination-abi-design.md §2).
// Node ids are raw 32-byte arrays, client / operation ids raw 16-byte arrays. `*Code` fields are the raw
// code-space values of the Rust enums (the C ABI carries them as u32); a committed typed rejection is a
// resolved result (MutationReply.OutcomeCode == Rejected), not an error.

/// <summary>A resource key: a namespace and opaque key bytes.</summary>
public sealed record ResourceKey(string Namespace, byte[] Key)
{
    public ResourceKey(string ns, string key) : this(ns, System.Text.Encoding.UTF8.GetBytes(key)) { }
    public bool Equals(ResourceKey? o) => o != null && Namespace == o.Namespace && Key.AsSpan().SequenceEqual(o.Key);
    public override int GetHashCode() => HashCode.Combine(Namespace, Key.Length);
}

/// <summary>A Raft log id: term, the leader's node id and the index.</summary>
public sealed record LogId(ulong Term, byte[] LeaderNodeId, ulong Index)
{
    public bool Equals(LogId? o) => o != null && Term == o.Term && Index == o.Index && LeaderNodeId.AsSpan().SequenceEqual(o.LeaderNodeId);
    public override int GetHashCode() => HashCode.Combine(Term, Index);
}

/// <summary>The fencing token of a coordination lease (group, resource, epoch, holder node id).</summary>
public sealed record CoordFence(byte[] GroupId, ResourceKey Resource, ulong Epoch, byte[] Holder)
{
    public bool Equals(CoordFence? o) => o != null && Epoch == o.Epoch && Resource.Equals(o.Resource)
        && GroupId.AsSpan().SequenceEqual(o.GroupId) && Holder.AsSpan().SequenceEqual(o.Holder);
    public override int GetHashCode() => HashCode.Combine(Epoch, Resource);
}

/// <summary>A resource row as read linearizably. <c>Holder</c> / <c>Value</c> are null when absent (a redacted value reads as null with <c>ValueRedacted</c>).</summary>
public sealed record ResourceSnapshot(
    byte[] GroupId, ResourceKey Resource, ulong Epoch, byte[]? Holder, bool Held, ulong TtlMs,
    ulong LeaseRevision, ulong ValueRevision, ulong AclRevision, byte[]? Value, bool ValueRedacted);

/// <summary>How a joining member is reached (a ticket and an optional signed join permit).</summary>
public sealed record MemberSeed(byte[] NodeId, string Ticket, byte[]? SignedJoinPermit);

// ── mutations (D0) ──────────────────────────────────────────────────────────

/// <summary>One of the ten sequenced, journaled mutations.</summary>
public abstract record MutationRequest
{
    private MutationRequest() { }
    public sealed record Acquire(ResourceKey Resource, ulong TtlMs) : MutationRequest;
    public sealed record Renew(CoordFence Fence) : MutationRequest;
    public sealed record CompareAndSet(CoordFence Fence, ulong ExpectedValueRevision, byte[] NewValue) : MutationRequest;
    public sealed record Release(CoordFence Fence) : MutationRequest;
    public sealed record ReleaseWithValue(CoordFence Fence, ulong ExpectedValueRevision, byte[] FinalValue) : MutationRequest;
    /// <summary><c>ExpectedEpoch</c> null = revoke whatever epoch is current.</summary>
    public sealed record Revoke(ResourceKey Resource, ulong? ExpectedEpoch) : MutationRequest;
    public sealed record SetGrant(ResourceKey Resource, byte[] TargetNodeId, ulong Rights, ulong ExpectedAclRevision) : MutationRequest;
    public sealed record SetAdmin(byte[] TargetNodeId, bool Enabled, ulong ExpectedAdminRevision) : MutationRequest;
    public sealed record ChangeMembership(byte[] OperationId, LogId? ExpectedMembershipLogId,
        IReadOnlyList<byte[]> DesiredVoters, IReadOnlyList<byte[]> RetainedLearners, IReadOnlyList<MemberSeed> MemberSeeds) : MutationRequest;
    public sealed record CancelMembership(byte[] OperationId, uint ExpectedPhaseCode) : MutationRequest;
}

/// <summary>One of the eight reads.</summary>
public abstract record ReadRequest
{
    private ReadRequest() { }
    public sealed record ReadLinearizable(ResourceKey Resource) : ReadRequest;
    public sealed record InspectAcl(ResourceKey Resource) : ReadRequest;
    public sealed record InspectAdmins : ReadRequest;
    public sealed record MembershipStatus(byte[]? OperationId) : ReadRequest;
    public sealed record GroupStatus : ReadRequest;
    public sealed record ProbeClient(byte[] TargetNodeId, byte[] TargetClientId) : ReadRequest;
    /// <summary><c>Limit</c> is 1..=10000.</summary>
    public sealed record AuditStream(LogId? After, uint Limit) : ReadRequest;
    public sealed record RecoveryExport : ReadRequest;
}

// ── results (D2-DE) ─────────────────────────────────────────────────────────

/// <summary>The resolved reply of a mutation (D2). A committed typed rejection has <c>OutcomeCode</c> = <see cref="CoordinationCodes.OutcomeRejected"/>.</summary>
public sealed record MutationReply(
    byte[] ClientId, ulong Sequence, byte[] RequestHash, uint DispositionCode, uint OutcomeCode, uint ReasonCode,
    ResourceSnapshot? Snapshot, ulong AclRevision, ulong AdminRevision, byte[]? OperationId, LogId? MembershipLogId)
{
    public bool IsRejected => OutcomeCode == CoordinationCodes.OutcomeRejected;
}

public sealed record AclGrant(byte[] SubjectNodeId, ulong Rights);
public sealed record AclStatus(ResourceKey Resource, ulong AclRevision, IReadOnlyList<AclGrant> Grants);
public sealed record AdminStatus(ulong AdminRevision, IReadOnlyList<byte[]> Administrators);

public sealed record MembershipStatus(byte[]? OperationId, uint StatusCode, LogId? ExpectedMembershipLogId, LogId? ObservedMembershipLogId,
    IReadOnlyList<byte[]> DesiredVoters, IReadOnlyList<byte[]> RetainedLearners, uint ErrorCode);

public sealed record RecoveredFrom(byte[] SourceGroupId, byte[] SourceGenesisHash, byte[] SourceAuthorityPublicKey,
    string SourceAuthorityDomain, byte[] ExportHash, LogId? SourceLastApplied);

public sealed record GroupStatus(
    uint LifecycleCode, byte[] LocalNodeId, uint LocalRoleCode, byte[]? LeaderNodeId, ulong Term, LogId? LastLogId, LogId? CommittedLogId,
    LogId? AppliedLogId, uint DeploymentProfileCode, LogId? MembershipLogId, bool MembershipJoint, IReadOnlyList<byte[]> Voters,
    IReadOnlyList<byte[]> Learners, byte[]? PendingOperationId, bool? QuorumAvailable, uint FatalCode, LogId? AuditFloor,
    byte[] GenesisHash, byte[] AuthorityPublicKey, string AuthorityDomain, RecoveredFrom? RecoveredFrom);

public sealed record ClientProbe(byte[] TargetNodeId, byte[] TargetClientId, bool Exists, ulong LastSequence, byte[] RequestHash,
    uint DispositionCode, bool Retired);

public sealed record AuditRecord(LogId LogId, byte[]? ActorNodeId, uint OperationCode, ResourceKey? Resource, uint ResultCode, ulong Epoch, ulong ValueRevision);

/// <summary>A recovery export with its body (DA).</summary>
public sealed record RecoveryExport(ulong TotalLen, byte[] Blake3Checksum, LogId? AppliedLogId, byte[] Body);

/// <summary>A recovery export written to a file (DB): everything but the body.</summary>
public sealed record RecoveryExportManifest(ulong TotalLen, byte[] Blake3Checksum, LogId? AppliedLogId);

public sealed record RetireReply(uint OutcomeCode, uint ReasonCode);

public sealed record PlacementSeparation(byte SeparatedPairs, byte ConnectedPairs, byte UnknownPairs, byte TotalPairs, byte RepresentedClusters, bool DiversityProved);

/// <summary>What one placement reconcile pass decided (DE).</summary>
public sealed record PlacementReport(
    ulong PolicyRevision, byte[] PolicyDigest, LogId? MembershipLogId, IReadOnlyList<byte[]> CurrentVoters, IReadOnlyList<byte[]> CurrentLearners,
    IReadOnlyList<byte[]> SelectedVoters, IReadOnlyList<byte[]> SelectedLearners, IReadOnlyList<byte[]> GraceCrossedMembers,
    PlacementSeparation? Separation, byte[]? PendingOperationId, string Reason);

// ── placement providers (UCv1 BC-BF, upcall kinds 32 / 33) ──────────────────

/// <summary>A candidate the placement policy may select; <c>MemberSeed</c> says how to reach a node that is not yet a member.</summary>
public sealed record PlacementCandidate(byte[] NodeId, MemberSeed? MemberSeed);

/// <summary>The placement policy (BD). <c>Eligible</c>, <c>PinnedVoters</c> and <c>Excluded</c> are sets: ascending by node id.</summary>
public sealed record PlacementPolicy(
    ulong Revision, byte[] Digest, byte[] AuthorityPublicKey, string AuthorityDomain, byte TargetVoters, byte TargetRetainedLearners,
    IReadOnlyList<PlacementCandidate> Eligible, IReadOnlyList<byte[]> PinnedVoters, IReadOnlyList<byte[]> Excluded,
    byte MinimumSeparatedVoterPairs, ulong ReplacementGraceMs, bool AllowHealthyRebalance);

/// <summary>Asks the provisioner to bring <c>NodeId</c> up as a member (BE); it answers with a <see cref="MemberSeed"/> for that node (BF).</summary>
public sealed record ProvisionRequest(byte[] GroupId, byte[] PolicyDigest, LogId? MembershipLogId, byte[] NodeId);

/// <summary>Raw code-space values worth naming. Others are carried as <c>uint</c>; see the Rust docs (<c>aster::coordination</c>).</summary>
public static class CoordinationCodes
{
    /// <summary><c>MutationReply.OutcomeCode</c> of a committed typed rejection (the reason is in <c>ReasonCode</c>).</summary>
    public const uint OutcomeRejected = 100;
}

/// <summary>Kinds of coordination failure (<c>aster_coordination_error_t</c>, one to one with <c>CoordinationErrorKind</c>).</summary>
public enum CoordinationErrorKind
{
    None = 0,
    /// <summary>A resolved result awaits acknowledgement: take it with <c>UnacknowledgedAsync</c> and acknowledge it first.</summary>
    AwaitingAck = 1,
    /// <summary>Another request is unresolved on this journal: <c>ResumeAsync</c> first.</summary>
    Unresolved = 2,
    /// <summary>Another process (or open client) holds this actor/group journal.</summary>
    JournalLocked = 3,
    /// <summary>The journal is fatal: no further mutation is possible.</summary>
    JournalFatal = 4,
    /// <summary>The group answered with a non-OK RPC status (<see cref="CoordinationException.RpcCode"/>; 7 = refused by the replicated ACL).</summary>
    Rpc = 5,
    /// <summary>No member answered; a journaled sequence stays open and resumes with the same bytes.</summary>
    Unavailable = 6,
    /// <summary>The client (or this handle's client) is closed.</summary>
    Closed = 7,
    /// <summary>Reserved: a committed typed rejection is a result, not an error.</summary>
    Rejected = 8,
    /// <summary>An argument or request outside the contract.</summary>
    Invalid = 9,
    Other = 10,
}

/// <summary>
/// A coordination operation failed. Switch on <see cref="Kind"/>, never on the message.
/// <see cref="IrohException.Status"/> is the coarse mapping (<see cref="Aster.Status.PermissionDenied"/> for an RPC 7 refusal by the
/// replicated ACL, <see cref="Aster.Status.Unauthenticated"/> for RPC 16 from coordination admission, ...).
/// </summary>
public sealed class CoordinationException : IrohException
{
    public CoordinationErrorKind Kind { get; }

    /// <summary>The RPC status code when <see cref="Kind"/> is <see cref="CoordinationErrorKind.Rpc"/>, else 0.</summary>
    public uint RpcCode { get; }

    public CoordinationException(CoordinationErrorKind kind, uint rpcCode, Status status, string message) : base(status, message)
    {
        Kind = kind;
        RpcCode = rpcCode;
    }
}
