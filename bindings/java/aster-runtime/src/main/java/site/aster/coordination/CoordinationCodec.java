package site.aster.coordination;

import java.util.ArrayList;
import java.util.List;
import site.aster.coordination.Coordination.*;
import site.aster.upcall.Ucv1.DecodeException;
import site.aster.upcall.Ucv1.R;
import site.aster.upcall.Ucv1.W;

/**
 * UCv1 tags {@code D0}-{@code DE} of the coordination client (design section 2), checked against
 * {@code conformance/vectors/coordination.json}: requests ({@code D0}, {@code D1}) are encoded here
 * and decoded strictly; results ({@code D2}-{@code DE}) are decoded here and encodable for
 * round-trip tests. Every id has its exact length and every decoder refuses what the vectors list
 * as invalid.
 */
public final class CoordinationCodec {

  private CoordinationCodec() {}

  public static final int TAG_MUTATION_REQUEST = 0xD0;
  public static final int TAG_READ_REQUEST = 0xD1;
  public static final int TAG_MUTATION_REPLY = 0xD2;
  public static final int TAG_READ_REPLY = 0xD3;
  public static final int TAG_ACL_STATUS = 0xD4;
  public static final int TAG_ADMIN_STATUS = 0xD5;
  public static final int TAG_MEMBERSHIP_STATUS = 0xD6;
  public static final int TAG_GROUP_STATUS = 0xD7;
  public static final int TAG_CLIENT_PROBE = 0xD8;
  public static final int TAG_AUDIT_RECORDS = 0xD9;
  public static final int TAG_RECOVERY_EXPORT = 0xDA;
  public static final int TAG_RECOVERY_EXPORT_MANIFEST = 0xDB;
  public static final int TAG_RETIRE_REPLY = 0xDC;
  public static final int TAG_COORDINATION_FENCE = 0xDD;
  public static final int TAG_PLACEMENT_REPORT = 0xDE;

  private static final long U32 = 0xFFFF_FFFFL;

  // -- primitives ----------------------------------------------------------------------------

  private static void exact(byte[] b, int n, String what) {
    if (b == null || b.length != n) {
      throw new DecodeException(
          what + " must be " + n + " bytes, got " + (b == null ? "null" : b.length));
    }
  }

  private static byte[] bytesN(R r, int n, String what) {
    byte[] b = r.bytes();
    exact(b, n, what);
    return b;
  }

  private static void putCode(W w, int code) {
    w.uint(code & U32);
  }

  private static int getCode(R r) {
    return (int) r.uintMax(U32, "code");
  }

  private static void putU8(W w, int v) {
    if (v < 0 || v > 255) {
      throw new IllegalArgumentException("u8 field out of range: " + v);
    }
    w.uint(v);
  }

  private static int getU8(R r, String what) {
    return (int) r.uintMax(255, what);
  }

  private static void putOptBytes(W w, byte[] b) {
    if (b == null) {
      w.absent();
    } else {
      w.present().bytes(b);
    }
  }

  private static byte[] getOptBytes(R r) {
    return r.present() ? r.bytes() : null;
  }

  private static byte[] getOptBytesN(R r, int n, String what) {
    if (!r.present()) {
      return null;
    }
    return bytesN(r, n, what);
  }

  private static void putBytesList(W w, List<byte[]> items, int exactLen, String what) {
    w.list(items.size());
    for (byte[] b : items) {
      if (exactLen >= 0) {
        exact(b, exactLen, what);
      }
      w.bytes(b);
    }
  }

  private static List<byte[]> getBytesList(R r, int exactLen, String what) {
    int n = r.list();
    List<byte[]> out = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      out.add(exactLen >= 0 ? bytesN(r, exactLen, what) : r.bytes());
    }
    return List.copyOf(out);
  }

  private static void putResource(W w, ResourceKey k) {
    w.str(k.namespace()).bytes(k.key());
  }

  private static ResourceKey getResource(R r) {
    return new ResourceKey(r.str(), r.bytes());
  }

  private static void putLogId(W w, LogId l) {
    w.uint(l.term()).bytes(l.leaderNodeId()).uint(l.index());
  }

  private static LogId getLogId(R r) {
    return new LogId(r.uint(), r.bytes(), r.uint());
  }

  private static void putOptLogId(W w, LogId l) {
    if (l == null) {
      w.absent();
    } else {
      w.present();
      putLogId(w, l);
    }
  }

  private static LogId getOptLogId(R r) {
    return r.present() ? getLogId(r) : null;
  }

  private static void putFence(W w, Fence f) {
    exact(f.groupId(), 32, "fence group id");
    exact(f.holder(), 32, "fence holder");
    w.bytes(f.groupId());
    putResource(w, f.resource());
    w.uint(f.epoch()).bytes(f.holder());
  }

  private static Fence getFence(R r) {
    byte[] group = bytesN(r, 32, "fence group id");
    ResourceKey res = getResource(r);
    long epoch = r.uint();
    byte[] holder = bytesN(r, 32, "fence holder");
    return new Fence(group, res, epoch, holder);
  }

  private static void putSeed(W w, MemberSeed s) {
    w.bytes(s.nodeId()).str(s.ticket());
    putOptBytes(w, s.signedJoinPermit());
  }

  private static MemberSeed getSeed(R r) {
    return new MemberSeed(r.bytes(), r.str(), getOptBytes(r));
  }

  /** A {@code MemberSeed} as its own message (tag {@code BF}, a kind-33 answer). */
  public static byte[] encodeMemberSeedMessage(MemberSeed s) {
    exact(s.nodeId(), 32, "member seed node id");
    W w = new W(0xBF);
    putSeed(w, s);
    return w.done();
  }

  public static MemberSeed decodeMemberSeedMessage(byte[] buf) {
    R r = new R(buf, 0xBF);
    MemberSeed s = getSeed(r);
    exact(s.nodeId(), 32, "member seed node id");
    r.end();
    return s;
  }

  // -- shared structures ---------------------------------------------------------------------

  private static void putSnapshot(W w, ResourceSnapshot s) {
    w.bytes(s.groupId());
    putResource(w, s.resource());
    w.uint(s.epoch());
    putOptBytes(w, s.holder());
    w.bool(s.held()).uint(s.ttlMs()).uint(s.leaseRevision()).uint(s.valueRevision());
    w.uint(s.aclRevision());
    putOptBytes(w, s.value());
    w.bool(s.valueRedacted());
  }

  private static ResourceSnapshot getSnapshot(R r) {
    byte[] group = r.bytes();
    ResourceKey res = getResource(r);
    long epoch = r.uint();
    byte[] holder = getOptBytes(r);
    boolean held = r.bool();
    long ttl = r.uint();
    long lease = r.uint();
    long value = r.uint();
    long acl = r.uint();
    byte[] val = getOptBytes(r);
    boolean redacted = r.bool();
    return new ResourceSnapshot(
        group, res, epoch, holder, held, ttl, lease, value, acl, val, redacted);
  }

  // -- D0 mutation request -------------------------------------------------------------------

  public static byte[] encodeMutationRequest(MutationRequest m) {
    W w = new W(TAG_MUTATION_REQUEST);
    switch (m) {
      case MutationRequest.Acquire a ->
          w.u8(1).str(a.resource().namespace()).bytes(a.resource().key()).uint(a.ttlMs());
      case MutationRequest.Renew a -> {
        w.u8(2);
        putFence(w, a.fence());
      }
      case MutationRequest.CompareAndSet a -> {
        w.u8(3);
        putFence(w, a.fence());
        w.uint(a.expectedValueRevision()).bytes(a.newValue());
      }
      case MutationRequest.Release a -> {
        w.u8(4);
        putFence(w, a.fence());
      }
      case MutationRequest.ReleaseWithValue a -> {
        w.u8(5);
        putFence(w, a.fence());
        w.uint(a.expectedValueRevision()).bytes(a.finalValue());
      }
      case MutationRequest.Revoke a -> {
        w.u8(6);
        putResource(w, a.resource());
        if (a.expectedEpoch() == null) {
          w.absent();
        } else {
          w.present().uint(a.expectedEpoch());
        }
      }
      case MutationRequest.SetGrant a -> {
        exact(a.targetNodeId(), 32, "target node id");
        w.u8(7);
        putResource(w, a.resource());
        w.bytes(a.targetNodeId()).uint(a.rights()).uint(a.expectedAclRevision());
      }
      case MutationRequest.SetAdmin a -> {
        exact(a.targetNodeId(), 32, "target node id");
        w.u8(8).bytes(a.targetNodeId()).bool(a.enabled()).uint(a.expectedAdminRevision());
      }
      case MutationRequest.ChangeMembership a -> {
        exact(a.operationId(), 16, "operation id");
        w.u8(9).bytes(a.operationId());
        putOptLogId(w, a.expectedMembershipLogId());
        putBytesList(w, a.desiredVoters(), 32, "voter id");
        putBytesList(w, a.retainedLearners(), 32, "learner id");
        w.list(a.memberSeeds().size());
        for (MemberSeed s : a.memberSeeds()) {
          exact(s.nodeId(), 32, "member seed node id");
          putSeed(w, s);
        }
      }
      case MutationRequest.CancelMembership a -> {
        exact(a.operationId(), 16, "operation id");
        w.u8(10).bytes(a.operationId());
        putCode(w, a.expectedPhaseCode());
      }
    }
    return w.done();
  }

  public static MutationRequest decodeMutationRequest(byte[] buf) {
    R r = new R(buf, TAG_MUTATION_REQUEST);
    int op = r.u8();
    MutationRequest out =
        switch (op) {
          case 1 -> new MutationRequest.Acquire(getResource(r), r.uint());
          case 2 -> new MutationRequest.Renew(getFence(r));
          case 3 -> new MutationRequest.CompareAndSet(getFence(r), r.uint(), r.bytes());
          case 4 -> new MutationRequest.Release(getFence(r));
          case 5 -> new MutationRequest.ReleaseWithValue(getFence(r), r.uint(), r.bytes());
          case 6 -> {
            ResourceKey k = getResource(r);
            yield new MutationRequest.Revoke(k, r.present() ? Long.valueOf(r.uint()) : null);
          }
          case 7 -> {
            ResourceKey k = getResource(r);
            byte[] target = bytesN(r, 32, "target node id");
            yield new MutationRequest.SetGrant(k, target, r.uint(), r.uint());
          }
          case 8 ->
              new MutationRequest.SetAdmin(bytesN(r, 32, "target node id"), r.bool(), r.uint());
          case 9 -> {
            byte[] opId = bytesN(r, 16, "operation id");
            LogId expected = getOptLogId(r);
            List<byte[]> voters = getBytesList(r, 32, "voter id");
            List<byte[]> learners = getBytesList(r, 32, "learner id");
            int n = r.list();
            List<MemberSeed> seeds = new ArrayList<>(n);
            for (int i = 0; i < n; i++) {
              MemberSeed s = getSeed(r);
              exact(s.nodeId(), 32, "member seed node id");
              seeds.add(s);
            }
            yield new MutationRequest.ChangeMembership(
                opId, expected, voters, learners, List.copyOf(seeds));
          }
          case 10 ->
              new MutationRequest.CancelMembership(bytesN(r, 16, "operation id"), getCode(r));
          default -> throw new DecodeException("unknown mutation op " + op);
        };
    r.end();
    return out;
  }

  // -- D1 read request -----------------------------------------------------------------------

  public static final long MAX_AUDIT_LIMIT = 10_000;

  public static byte[] encodeReadRequest(ReadRequest q) {
    W w = new W(TAG_READ_REQUEST);
    switch (q) {
      case ReadRequest.ReadLinearizable a -> {
        w.u8(1);
        putResource(w, a.resource());
      }
      case ReadRequest.InspectAcl a -> {
        w.u8(2);
        putResource(w, a.resource());
      }
      case ReadRequest.InspectAdmins a -> w.u8(3);
      case ReadRequest.MembershipStatusOf a -> {
        w.u8(4);
        if (a.operationId() == null) {
          w.absent();
        } else {
          exact(a.operationId(), 16, "operation id");
          w.present().bytes(a.operationId());
        }
      }
      case ReadRequest.GroupStatusOf a -> w.u8(5);
      case ReadRequest.ProbeClient a -> {
        exact(a.targetNodeId(), 32, "target node id");
        exact(a.targetClientId(), 16, "target client id");
        w.u8(6).bytes(a.targetNodeId()).bytes(a.targetClientId());
      }
      case ReadRequest.AuditStream a -> {
        if (a.limit() < 1 || a.limit() > MAX_AUDIT_LIMIT) {
          throw new IllegalArgumentException("audit limit must be 1..=10000");
        }
        w.u8(7);
        putOptLogId(w, a.after());
        w.uint(a.limit());
      }
      case ReadRequest.RecoveryExportRequest a -> w.u8(8);
    }
    return w.done();
  }

  public static ReadRequest decodeReadRequest(byte[] buf) {
    R r = new R(buf, TAG_READ_REQUEST);
    int op = r.u8();
    ReadRequest out =
        switch (op) {
          case 1 -> new ReadRequest.ReadLinearizable(getResource(r));
          case 2 -> new ReadRequest.InspectAcl(getResource(r));
          case 3 -> new ReadRequest.InspectAdmins();
          case 4 -> new ReadRequest.MembershipStatusOf(getOptBytesN(r, 16, "operation id"));
          case 5 -> new ReadRequest.GroupStatusOf();
          case 6 ->
              new ReadRequest.ProbeClient(
                  bytesN(r, 32, "target node id"), bytesN(r, 16, "target client id"));
          case 7 -> {
            LogId after = getOptLogId(r);
            long limit = r.uint();
            if (limit < 1 || limit > MAX_AUDIT_LIMIT) {
              throw new DecodeException("audit limit " + limit + " outside 1..=10000");
            }
            yield new ReadRequest.AuditStream(after, limit);
          }
          case 8 -> new ReadRequest.RecoveryExportRequest();
          default -> throw new DecodeException("unknown read op " + op);
        };
    r.end();
    return out;
  }

  // -- D2 mutation reply ---------------------------------------------------------------------

  public static byte[] encodeMutationReply(MutationReply m) {
    W w = new W(TAG_MUTATION_REPLY);
    w.bytes(m.clientId()).uint(m.sequence()).bytes(m.requestHash());
    putCode(w, m.dispositionCode());
    putCode(w, m.outcomeCode());
    putCode(w, m.reasonCode());
    if (m.snapshot() == null) {
      w.absent();
    } else {
      w.present();
      putSnapshot(w, m.snapshot());
    }
    w.uint(m.aclRevision()).uint(m.adminRevision());
    putOptBytes(w, m.operationId());
    putOptLogId(w, m.membershipLogId());
    return w.done();
  }

  public static MutationReply decodeMutationReply(byte[] buf) {
    R r = new R(buf, TAG_MUTATION_REPLY);
    byte[] client = r.bytes();
    long seq = r.uint();
    byte[] hash = r.bytes();
    int disposition = getCode(r);
    int outcome = getCode(r);
    int reason = getCode(r);
    ResourceSnapshot snap = r.present() ? getSnapshot(r) : null;
    long acl = r.uint();
    long admin = r.uint();
    byte[] opId = getOptBytes(r);
    LogId log = getOptLogId(r);
    r.end();
    return new MutationReply(
        client, seq, hash, disposition, outcome, reason, snap, acl, admin, opId, log);
  }

  // -- D3 .. DB ------------------------------------------------------------------------------

  public static byte[] encodeReadReply(ResourceSnapshot s) {
    W w = new W(TAG_READ_REPLY);
    putSnapshot(w, s);
    return w.done();
  }

  public static ResourceSnapshot decodeReadReply(byte[] buf) {
    R r = new R(buf, TAG_READ_REPLY);
    ResourceSnapshot s = getSnapshot(r);
    r.end();
    return s;
  }

  public static byte[] encodeAclStatus(AclStatus a) {
    W w = new W(TAG_ACL_STATUS);
    putResource(w, a.resource());
    w.uint(a.aclRevision()).list(a.grants().size());
    for (Grant g : a.grants()) {
      w.bytes(g.subjectNodeId()).uint(g.rights());
    }
    return w.done();
  }

  public static AclStatus decodeAclStatus(byte[] buf) {
    R r = new R(buf, TAG_ACL_STATUS);
    ResourceKey res = getResource(r);
    long rev = r.uint();
    int n = r.list();
    List<Grant> grants = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      grants.add(new Grant(r.bytes(), r.uint()));
    }
    r.end();
    return new AclStatus(res, rev, List.copyOf(grants));
  }

  public static byte[] encodeAdminStatus(AdminStatus a) {
    W w = new W(TAG_ADMIN_STATUS).uint(a.adminRevision());
    putBytesList(w, a.administrators(), -1, "");
    return w.done();
  }

  public static AdminStatus decodeAdminStatus(byte[] buf) {
    R r = new R(buf, TAG_ADMIN_STATUS);
    AdminStatus a = new AdminStatus(r.uint(), getBytesList(r, -1, ""));
    r.end();
    return a;
  }

  public static byte[] encodeMembershipStatus(MembershipStatus m) {
    W w = new W(TAG_MEMBERSHIP_STATUS);
    putOptBytes(w, m.operationId());
    putCode(w, m.statusCode());
    putOptLogId(w, m.expectedMembershipLogId());
    putOptLogId(w, m.observedMembershipLogId());
    putBytesList(w, m.desiredVoters(), -1, "");
    putBytesList(w, m.retainedLearners(), -1, "");
    putCode(w, m.errorCode());
    return w.done();
  }

  public static MembershipStatus decodeMembershipStatus(byte[] buf) {
    R r = new R(buf, TAG_MEMBERSHIP_STATUS);
    MembershipStatus m =
        new MembershipStatus(
            getOptBytes(r),
            getCode(r),
            getOptLogId(r),
            getOptLogId(r),
            getBytesList(r, -1, ""),
            getBytesList(r, -1, ""),
            getCode(r));
    r.end();
    return m;
  }

  public static byte[] encodeGroupStatus(GroupStatus g) {
    W w = new W(TAG_GROUP_STATUS);
    putCode(w, g.lifecycleCode());
    w.bytes(g.localNodeId());
    putCode(w, g.localRoleCode());
    putOptBytes(w, g.leaderNodeId());
    w.uint(g.term());
    putOptLogId(w, g.lastLogId());
    putOptLogId(w, g.committedLogId());
    putOptLogId(w, g.appliedLogId());
    putCode(w, g.deploymentProfileCode());
    putOptLogId(w, g.membershipLogId());
    w.bool(g.membershipJoint());
    putBytesList(w, g.voters(), -1, "");
    putBytesList(w, g.learners(), -1, "");
    putOptBytes(w, g.pendingOperationId());
    if (g.quorumAvailable() == null) {
      w.absent();
    } else {
      w.present().bool(g.quorumAvailable());
    }
    putCode(w, g.fatalCode());
    putOptLogId(w, g.auditFloor());
    w.bytes(g.genesisHash()).bytes(g.authorityPublicKey()).str(g.authorityDomain());
    RecoveredFrom rf = g.recoveredFrom();
    if (rf == null) {
      w.absent();
    } else {
      w.present()
          .bytes(rf.sourceGroupId())
          .bytes(rf.sourceGenesisHash())
          .bytes(rf.sourceAuthorityPublicKey())
          .str(rf.sourceAuthorityDomain())
          .bytes(rf.exportHash());
      putOptLogId(w, rf.sourceLastApplied());
    }
    return w.done();
  }

  public static GroupStatus decodeGroupStatus(byte[] buf) {
    R r = new R(buf, TAG_GROUP_STATUS);
    int lifecycle = getCode(r);
    byte[] local = r.bytes();
    int role = getCode(r);
    byte[] leader = getOptBytes(r);
    long term = r.uint();
    LogId last = getOptLogId(r);
    LogId committed = getOptLogId(r);
    LogId applied = getOptLogId(r);
    int profile = getCode(r);
    LogId membership = getOptLogId(r);
    boolean joint = r.bool();
    List<byte[]> voters = getBytesList(r, -1, "");
    List<byte[]> learners = getBytesList(r, -1, "");
    byte[] pending = getOptBytes(r);
    Boolean quorum = r.present() ? Boolean.valueOf(r.bool()) : null;
    int fatal = getCode(r);
    LogId floor = getOptLogId(r);
    byte[] genesis = r.bytes();
    byte[] authority = r.bytes();
    String domain = r.str();
    RecoveredFrom rf = null;
    if (r.present()) {
      rf = new RecoveredFrom(r.bytes(), r.bytes(), r.bytes(), r.str(), r.bytes(), getOptLogId(r));
    }
    r.end();
    return new GroupStatus(
        lifecycle,
        local,
        role,
        leader,
        term,
        last,
        committed,
        applied,
        profile,
        membership,
        joint,
        voters,
        learners,
        pending,
        quorum,
        fatal,
        floor,
        genesis,
        authority,
        domain,
        rf);
  }

  public static byte[] encodeClientProbe(ClientProbe p) {
    W w = new W(TAG_CLIENT_PROBE);
    w.bytes(p.targetNodeId()).bytes(p.targetClientId()).bool(p.exists()).uint(p.lastSequence());
    w.bytes(p.requestHash());
    putCode(w, p.dispositionCode());
    w.bool(p.retired());
    return w.done();
  }

  public static ClientProbe decodeClientProbe(byte[] buf) {
    R r = new R(buf, TAG_CLIENT_PROBE);
    ClientProbe p =
        new ClientProbe(r.bytes(), r.bytes(), r.bool(), r.uint(), r.bytes(), getCode(r), r.bool());
    r.end();
    return p;
  }

  public static byte[] encodeAuditRecords(List<AuditRecord> records) {
    W w = new W(TAG_AUDIT_RECORDS).list(records.size());
    for (AuditRecord a : records) {
      putLogId(w, a.logId());
      putOptBytes(w, a.actorNodeId());
      putCode(w, a.operationCode());
      if (a.resource() == null) {
        w.absent();
      } else {
        w.present();
        putResource(w, a.resource());
      }
      putCode(w, a.resultCode());
      w.uint(a.epoch()).uint(a.valueRevision());
    }
    return w.done();
  }

  public static List<AuditRecord> decodeAuditRecords(byte[] buf) {
    R r = new R(buf, TAG_AUDIT_RECORDS);
    int n = r.list();
    List<AuditRecord> out = new ArrayList<>(n);
    for (int i = 0; i < n; i++) {
      LogId id = getLogId(r);
      byte[] actor = getOptBytes(r);
      int op = getCode(r);
      ResourceKey res = r.present() ? getResource(r) : null;
      out.add(new AuditRecord(id, actor, op, res, getCode(r), r.uint(), r.uint()));
    }
    r.end();
    return List.copyOf(out);
  }

  public static byte[] encodeRecoveryExport(RecoveryExport e) {
    W w = new W(TAG_RECOVERY_EXPORT).uint(e.totalLen()).bytes(e.blake3Checksum());
    putOptLogId(w, e.appliedLogId());
    w.bytes(e.body());
    return w.done();
  }

  public static RecoveryExport decodeRecoveryExport(byte[] buf) {
    R r = new R(buf, TAG_RECOVERY_EXPORT);
    RecoveryExport e = new RecoveryExport(r.uint(), r.bytes(), getOptLogId(r), r.bytes());
    r.end();
    return e;
  }

  public static byte[] encodeRecoveryManifest(RecoveryManifest m) {
    W w = new W(TAG_RECOVERY_EXPORT_MANIFEST).uint(m.totalLen()).bytes(m.blake3Checksum());
    putOptLogId(w, m.appliedLogId());
    return w.done();
  }

  public static RecoveryManifest decodeRecoveryManifest(byte[] buf) {
    R r = new R(buf, TAG_RECOVERY_EXPORT_MANIFEST);
    RecoveryManifest m = new RecoveryManifest(r.uint(), r.bytes(), getOptLogId(r));
    r.end();
    return m;
  }

  public static byte[] encodeRetireReply(RetireReply v) {
    W w = new W(TAG_RETIRE_REPLY);
    putCode(w, v.outcomeCode());
    putCode(w, v.reasonCode());
    return w.done();
  }

  public static RetireReply decodeRetireReply(byte[] buf) {
    R r = new R(buf, TAG_RETIRE_REPLY);
    RetireReply v = new RetireReply(getCode(r), getCode(r));
    r.end();
    return v;
  }

  public static byte[] encodeFence(Fence f) {
    W w = new W(TAG_COORDINATION_FENCE);
    putFence(w, f);
    return w.done();
  }

  public static Fence decodeFence(byte[] buf) {
    R r = new R(buf, TAG_COORDINATION_FENCE);
    Fence f = getFence(r);
    r.end();
    return f;
  }

  // -- DE placement report -------------------------------------------------------------------

  public static byte[] encodePlacementReport(PlacementReport p) {
    exact(p.policyDigest(), 32, "policy digest");
    W w = new W(TAG_PLACEMENT_REPORT).uint(p.policyRevision()).bytes(p.policyDigest());
    putOptLogId(w, p.membershipLogId());
    putBytesList(w, p.currentVoters(), -1, "");
    putBytesList(w, p.currentLearners(), -1, "");
    putBytesList(w, p.selectedVoters(), -1, "");
    putBytesList(w, p.selectedLearners(), -1, "");
    putBytesList(w, p.graceCrossedMembers(), -1, "");
    Separation s = p.separation();
    if (s == null) {
      w.absent();
    } else {
      w.present();
      putU8(w, s.separatedPairs());
      putU8(w, s.connectedPairs());
      putU8(w, s.unknownPairs());
      putU8(w, s.totalPairs());
      putU8(w, s.representedClusters());
      w.bool(s.diversityProved());
    }
    putOptBytes(w, p.pendingOperationId());
    w.str(p.reason());
    return w.done();
  }

  public static PlacementReport decodePlacementReport(byte[] buf) {
    R r = new R(buf, TAG_PLACEMENT_REPORT);
    long rev = r.uint();
    byte[] digest = bytesN(r, 32, "policy digest");
    LogId log = getOptLogId(r);
    List<byte[]> cv = getBytesList(r, -1, "");
    List<byte[]> cl = getBytesList(r, -1, "");
    List<byte[]> sv = getBytesList(r, -1, "");
    List<byte[]> sl = getBytesList(r, -1, "");
    List<byte[]> grace = getBytesList(r, -1, "");
    Separation sep = null;
    if (r.present()) {
      sep =
          new Separation(
              getU8(r, "separated_pairs"),
              getU8(r, "connected_pairs"),
              getU8(r, "unknown_pairs"),
              getU8(r, "total_pairs"),
              getU8(r, "represented_clusters"),
              r.bool());
    }
    byte[] pending = getOptBytes(r);
    String reason = r.str();
    r.end();
    return new PlacementReport(rev, digest, log, cv, cl, sv, sl, grace, sep, pending, reason);
  }
}
