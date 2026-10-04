package site.aster.coordination;

import java.nio.charset.StandardCharsets;
import java.util.List;

/**
 * The value types of the C ABI 4.4 coordination client (UCv1 tags {@code D0}-{@code DE}, see {@code
 * docs/_internal/bindings/coordination-abi-design.md} section 2). Node ids are 32 raw bytes, client
 * and operation ids 16; {@code *Code} fields carry the Rust code-space values (the Store's
 * dispositions, outcomes, reasons, lifecycle and role codes) as {@code int}.
 *
 * <p>Arrays are not copied: treat them as immutable.
 */
public final class Coordination {

  private Coordination() {}

  /** A resource: a namespace and an opaque key. */
  public record ResourceKey(String namespace, byte[] key) {
    public static ResourceKey of(String namespace, String key) {
      return new ResourceKey(namespace, key.getBytes(StandardCharsets.UTF_8));
    }
  }

  /** A position in the replicated log. */
  public record LogId(long term, byte[] leaderNodeId, long index) {}

  /** How to reach a member being added. */
  public record MemberSeed(byte[] nodeId, String ticket, byte[] signedJoinPermit) {}

  /**
   * The fence of a coordination lease: present it to whatever the lease protects. Never cache one;
   * ask the lease for a fresh one before each protected action.
   */
  public record Fence(byte[] groupId, ResourceKey resource, long epoch, byte[] holder) {}

  /** The register + lease state of one resource. {@code holder} / {@code value} may be null. */
  public record ResourceSnapshot(
      byte[] groupId,
      ResourceKey resource,
      long epoch,
      byte[] holder,
      boolean held,
      long ttlMs,
      long leaseRevision,
      long valueRevision,
      long aclRevision,
      byte[] value,
      boolean valueRedacted) {}

  public record Grant(byte[] subjectNodeId, long rights) {}

  public record AclStatus(ResourceKey resource, long aclRevision, List<Grant> grants) {}

  public record AdminStatus(long adminRevision, List<byte[]> administrators) {}

  public record MembershipStatus(
      byte[] operationId,
      int statusCode,
      LogId expectedMembershipLogId,
      LogId observedMembershipLogId,
      List<byte[]> desiredVoters,
      List<byte[]> retainedLearners,
      int errorCode) {}

  public record RecoveredFrom(
      byte[] sourceGroupId,
      byte[] sourceGenesisHash,
      byte[] sourceAuthorityPublicKey,
      String sourceAuthorityDomain,
      byte[] exportHash,
      LogId sourceLastApplied) {}

  public record GroupStatus(
      int lifecycleCode,
      byte[] localNodeId,
      int localRoleCode,
      byte[] leaderNodeId,
      long term,
      LogId lastLogId,
      LogId committedLogId,
      LogId appliedLogId,
      int deploymentProfileCode,
      LogId membershipLogId,
      boolean membershipJoint,
      List<byte[]> voters,
      List<byte[]> learners,
      byte[] pendingOperationId,
      Boolean quorumAvailable,
      int fatalCode,
      LogId auditFloor,
      byte[] genesisHash,
      byte[] authorityPublicKey,
      String authorityDomain,
      RecoveredFrom recoveredFrom) {}

  public record ClientProbe(
      byte[] targetNodeId,
      byte[] targetClientId,
      boolean exists,
      long lastSequence,
      byte[] requestHash,
      int dispositionCode,
      boolean retired) {}

  public record AuditRecord(
      LogId logId,
      byte[] actorNodeId,
      int operationCode,
      ResourceKey resource,
      int resultCode,
      long epoch,
      long valueRevision) {}

  public record RecoveryExport(
      long totalLen, byte[] blake3Checksum, LogId appliedLogId, byte[] body) {}

  public record RecoveryManifest(long totalLen, byte[] blake3Checksum, LogId appliedLogId) {}

  public record RetireReply(int outcomeCode, int reasonCode) {}

  /**
   * The resolved result of one sequenced mutation. A committed typed rejection is a result ({@code
   * outcomeCode}, {@code reasonCode}), not an exception.
   */
  public record MutationReply(
      byte[] clientId,
      long sequence,
      byte[] requestHash,
      int dispositionCode,
      int outcomeCode,
      int reasonCode,
      ResourceSnapshot snapshot,
      long aclRevision,
      long adminRevision,
      byte[] operationId,
      LogId membershipLogId) {}

  public record Separation(
      int separatedPairs,
      int connectedPairs,
      int unknownPairs,
      int totalPairs,
      int representedClusters,
      boolean diversityProved) {}

  public record PlacementReport(
      long policyRevision,
      byte[] policyDigest,
      LogId membershipLogId,
      List<byte[]> currentVoters,
      List<byte[]> currentLearners,
      List<byte[]> selectedVoters,
      List<byte[]> selectedLearners,
      List<byte[]> graceCrossedMembers,
      Separation separation,
      byte[] pendingOperationId,
      String reason) {}

  // -- requests ------------------------------------------------------------------------------

  /** One sequenced mutation (UCv1 {@code D0}). */
  public sealed interface MutationRequest {
    record Acquire(ResourceKey resource, long ttlMs) implements MutationRequest {}

    record Renew(Fence fence) implements MutationRequest {}

    record CompareAndSet(Fence fence, long expectedValueRevision, byte[] newValue)
        implements MutationRequest {}

    record Release(Fence fence) implements MutationRequest {}

    record ReleaseWithValue(Fence fence, long expectedValueRevision, byte[] finalValue)
        implements MutationRequest {}

    /** {@code expectedEpoch} null = any epoch. */
    record Revoke(ResourceKey resource, Long expectedEpoch) implements MutationRequest {}

    record SetGrant(
        ResourceKey resource, byte[] targetNodeId, long rights, long expectedAclRevision)
        implements MutationRequest {}

    record SetAdmin(byte[] targetNodeId, boolean enabled, long expectedAdminRevision)
        implements MutationRequest {}

    record ChangeMembership(
        byte[] operationId,
        LogId expectedMembershipLogId,
        List<byte[]> desiredVoters,
        List<byte[]> retainedLearners,
        List<MemberSeed> memberSeeds)
        implements MutationRequest {}

    record CancelMembership(byte[] operationId, int expectedPhaseCode) implements MutationRequest {}
  }

  /** One read or status request (UCv1 {@code D1}). */
  public sealed interface ReadRequest {
    record ReadLinearizable(ResourceKey resource) implements ReadRequest {}

    record InspectAcl(ResourceKey resource) implements ReadRequest {}

    record InspectAdmins() implements ReadRequest {}

    /** {@code operationId} null = the current operation. */
    record MembershipStatusOf(byte[] operationId) implements ReadRequest {}

    record GroupStatusOf() implements ReadRequest {}

    record ProbeClient(byte[] targetNodeId, byte[] targetClientId) implements ReadRequest {}

    /** {@code after} null = from the audit floor; {@code limit} 1..=10000. */
    record AuditStream(LogId after, long limit) implements ReadRequest {}

    record RecoveryExportRequest() implements ReadRequest {}
  }
}
