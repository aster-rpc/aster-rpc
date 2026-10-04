package site.aster.coordination;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.List;
import site.aster.coordination.Coordination.LogId;
import site.aster.coordination.Coordination.MemberSeed;
import site.aster.upcall.Ucv1.DecodeException;
import site.aster.upcall.Ucv1.R;
import site.aster.upcall.Ucv1.W;

/**
 * The placement manager's providers and their UCv1 messages (upcall kinds 32 and 33, tags {@code
 * BC}-{@code BF}). Node ids are 32 bytes; sets travel in strictly ascending byte order, which the
 * encoder produces and the decoder insists on.
 */
public final class Placement {

  private Placement() {}

  public static final int TAG_POLICY_REQUEST = 0xBC;
  public static final int TAG_POLICY = 0xBD;
  public static final int TAG_PROVISION_REQUEST = 0xBE;

  /** One node the policy allows; {@code memberSeed} null = ask the provisioner. */
  public record Candidate(byte[] nodeId, MemberSeed memberSeed) {}

  /** The desired-roles policy of one reconciliation pass. */
  public record PlacementPolicy(
      long revision,
      byte[] digest,
      byte[] authorityPublicKey,
      String authorityDomain,
      int targetVoters,
      int targetRetainedLearners,
      List<Candidate> eligible,
      List<byte[]> pinnedVoters,
      List<byte[]> excluded,
      int minimumSeparatedVoterPairs,
      long replacementGraceMs,
      boolean allowHealthyRebalance) {}

  /** What the provisioner is asked for: idempotent per {@code (groupId, policyDigest, nodeId)}. */
  public record ProvisionRequest(
      byte[] groupId, byte[] policyDigest, LogId membershipLogId, byte[] nodeId) {}

  /** The policy source (kind 32): asked once per pass, on a virtual thread; may block. */
  @FunctionalInterface
  public interface PolicySource {
    PlacementPolicy policy() throws Exception;
  }

  /**
   * The provisioner (kind 33): produce seed material (how to reach and join) for a candidate that
   * has none. The answer must be for the node asked about, else the pass fails. May block.
   */
  @FunctionalInterface
  public interface Provisioner {
    MemberSeed provision(ProvisionRequest request) throws Exception;
  }

  // -- codec ---------------------------------------------------------------------------------

  private static final Comparator<byte[]> ASCENDING = Arrays::compareUnsigned;

  private static void exact(byte[] b, int n, String what) {
    if (b == null || b.length != n) {
      throw new DecodeException(what + " must be " + n + " bytes");
    }
  }

  public static byte[] encodePolicyRequest() {
    return new W(TAG_POLICY_REQUEST).done();
  }

  public static void decodePolicyRequest(byte[] buf) {
    new R(buf, TAG_POLICY_REQUEST).end();
  }

  private static void putIdSet(W w, List<byte[]> ids, String what) {
    List<byte[]> sorted = new ArrayList<>(ids);
    sorted.sort(ASCENDING);
    w.list(sorted.size());
    byte[] prev = null;
    for (byte[] id : sorted) {
      exact(id, 32, what);
      if (prev != null && Arrays.equals(prev, id)) {
        throw new IllegalArgumentException("duplicate " + what);
      }
      w.bytes(id);
      prev = id;
    }
  }

  private static List<byte[]> getIdSet(R r, String what) {
    int n = r.list();
    List<byte[]> out = new ArrayList<>(n);
    byte[] prev = null;
    for (int i = 0; i < n; i++) {
      byte[] id = r.bytes();
      exact(id, 32, what);
      if (prev != null && Arrays.compareUnsigned(prev, id) >= 0) {
        throw new DecodeException(what + "s not in strictly ascending order");
      }
      out.add(id);
      prev = id;
    }
    return List.copyOf(out);
  }

  public static byte[] encodePolicy(PlacementPolicy p) {
    exact(p.digest(), 32, "digest");
    exact(p.authorityPublicKey(), 32, "authority public key");
    W w = new W(TAG_POLICY).uint(p.revision()).bytes(p.digest()).bytes(p.authorityPublicKey());
    w.str(p.authorityDomain());
    putU8(w, p.targetVoters());
    putU8(w, p.targetRetainedLearners());
    List<Candidate> eligible = new ArrayList<>(p.eligible());
    eligible.sort((a, b) -> ASCENDING.compare(a.nodeId(), b.nodeId()));
    w.list(eligible.size());
    byte[] prev = null;
    for (Candidate c : eligible) {
      exact(c.nodeId(), 32, "eligible node id");
      if (prev != null && Arrays.equals(prev, c.nodeId())) {
        throw new IllegalArgumentException("duplicate eligible node id");
      }
      prev = c.nodeId();
      w.bytes(c.nodeId());
      if (c.memberSeed() == null) {
        w.absent();
      } else {
        w.present().bytes(c.memberSeed().nodeId()).str(c.memberSeed().ticket());
        if (c.memberSeed().signedJoinPermit() == null) {
          w.absent();
        } else {
          w.present().bytes(c.memberSeed().signedJoinPermit());
        }
      }
    }
    putIdSet(w, p.pinnedVoters(), "pinned voter");
    putIdSet(w, p.excluded(), "excluded node");
    putU8(w, p.minimumSeparatedVoterPairs());
    w.uint(p.replacementGraceMs()).bool(p.allowHealthyRebalance());
    return w.done();
  }

  private static void putU8(W w, int v) {
    if (v < 0 || v > 255) {
      throw new IllegalArgumentException("u8 field out of range: " + v);
    }
    w.uint(v);
  }

  public static PlacementPolicy decodePolicy(byte[] buf) {
    R r = new R(buf, TAG_POLICY);
    long revision = r.uint();
    byte[] digest = r.bytes();
    exact(digest, 32, "digest");
    byte[] authority = r.bytes();
    exact(authority, 32, "authority public key");
    String domain = r.str();
    int voters = (int) r.uintMax(255, "target_voters");
    int learners = (int) r.uintMax(255, "target_retained_learners");
    int n = r.list();
    List<Candidate> eligible = new ArrayList<>(n);
    byte[] prev = null;
    for (int i = 0; i < n; i++) {
      byte[] id = r.bytes();
      exact(id, 32, "eligible node id");
      if (prev != null && Arrays.compareUnsigned(prev, id) >= 0) {
        throw new DecodeException("eligible node ids not in strictly ascending order");
      }
      prev = id;
      MemberSeed seed = null;
      if (r.present()) {
        byte[] seedId = r.bytes();
        exact(seedId, 32, "member seed node id");
        seed = new MemberSeed(seedId, r.str(), r.present() ? r.bytes() : null);
      }
      eligible.add(new Candidate(id, seed));
    }
    List<byte[]> pinned = getIdSet(r, "pinned voter");
    List<byte[]> excluded = getIdSet(r, "excluded node");
    int separated = (int) r.uintMax(255, "minimum_separated_voter_pairs");
    long grace = r.uint();
    boolean rebalance = r.bool();
    r.end();
    return new PlacementPolicy(
        revision,
        digest,
        authority,
        domain,
        voters,
        learners,
        List.copyOf(eligible),
        pinned,
        excluded,
        separated,
        grace,
        rebalance);
  }

  public static byte[] encodeProvisionRequest(ProvisionRequest q) {
    exact(q.groupId(), 32, "group id");
    exact(q.policyDigest(), 32, "policy digest");
    exact(q.nodeId(), 32, "node id");
    W w = new W(TAG_PROVISION_REQUEST).bytes(q.groupId()).bytes(q.policyDigest());
    if (q.membershipLogId() == null) {
      w.absent();
    } else {
      w.present()
          .uint(q.membershipLogId().term())
          .bytes(q.membershipLogId().leaderNodeId())
          .uint(q.membershipLogId().index());
    }
    w.bytes(q.nodeId());
    return w.done();
  }

  public static ProvisionRequest decodeProvisionRequest(byte[] buf) {
    R r = new R(buf, TAG_PROVISION_REQUEST);
    byte[] group = r.bytes();
    exact(group, 32, "group id");
    byte[] digest = r.bytes();
    exact(digest, 32, "policy digest");
    LogId log = r.present() ? new LogId(r.uint(), r.bytes(), r.uint()) : null;
    byte[] node = r.bytes();
    exact(node, 32, "node id");
    r.end();
    return new ProvisionRequest(group, digest, log, node);
  }
}
