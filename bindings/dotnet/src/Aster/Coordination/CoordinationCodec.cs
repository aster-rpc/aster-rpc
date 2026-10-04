using System;
using System.Collections.Generic;
using Aster.Upcall;
using W = Aster.Upcall.Ucv1.W;
using R = Aster.Upcall.Ucv1.R;

namespace Aster.Coordination;

/// <summary>
/// UCv1 messages <c>D0</c>-<c>DE</c> (coordination client, design §2) and <c>BC</c>-<c>BF</c> (placement
/// providers), checked against <c>conformance/vectors/coordination.json</c> and <c>upcalls.json</c>.
/// Requests carry exact id lengths (node ids and group ids 32 bytes, client / operation ids 16); anything else,
/// and every non-canonical encoding, is refused with <see cref="Ucv1DecodeException"/>.
/// </summary>
public static class CoordinationCodec
{
    public const byte TagMutationRequest = 0xD0;
    public const byte TagReadRequest = 0xD1;
    public const byte TagMutationReply = 0xD2;
    public const byte TagReadReply = 0xD3;
    public const byte TagAclStatus = 0xD4;
    public const byte TagAdminStatus = 0xD5;
    public const byte TagMembershipStatus = 0xD6;
    public const byte TagGroupStatus = 0xD7;
    public const byte TagClientProbe = 0xD8;
    public const byte TagAuditRecords = 0xD9;
    public const byte TagRecoveryExport = 0xDA;
    public const byte TagRecoveryExportManifest = 0xDB;
    public const byte TagRetireReply = 0xDC;
    public const byte TagCoordinationFence = 0xDD;
    public const byte TagPlacementReport = 0xDE;
    public const byte TagPlacementPolicyRequest = 0xBC;
    public const byte TagPlacementPolicy = 0xBD;
    public const byte TagPlacementProvisionRequest = 0xBE;
    public const byte TagMemberSeed = 0xBF;

    // ── primitives ───────────────────────────────────────────────────────────

    private static Ucv1DecodeException Bad(string m) => new(m);

    private static byte[] Fixed(R r, int n, string what)
    {
        byte[] b = r.Bytes();
        if (b.Length != n) throw Bad($"{what}: {b.Length} bytes, expected {n}");
        return b;
    }

    private static byte[] Check(byte[] b, int n, string what)
        => b.Length == n ? b : throw new ArgumentException($"{what} must be {n} bytes (got {b.Length})");

    private static uint Code(R r) => (uint)r.UintMax(0xFFFF_FFFF, "code");
    private static byte U8Field(R r, string what) => (byte)r.UintMax(0xFF, what);

    private static T? Opt<T>(R r, Func<R, T> read) where T : class => r.Present() ? read(r) : null;

    private static void PutOpt<T>(W w, T? v, Action<W, T> put) where T : class
    {
        if (v == null) w.Absent(); else { w.Present(); put(w, v); }
    }

    private static List<T> Many<T>(R r, Func<R, T> read)
    {
        int n = r.List();
        var o = new List<T>(n);
        for (int i = 0; i < n; i++) o.Add(read(r));
        return o;
    }

    private static void PutMany<T>(W w, IReadOnlyList<T> v, Action<W, T> put)
    {
        w.List(v.Count);
        foreach (var x in v) put(w, x);
    }

    private static void PutIds(W w, IReadOnlyList<byte[]> ids, int len, string what)
    {
        w.List(ids.Count);
        foreach (var id in ids) w.Bytes(Check(id, len, what));
    }

    private static List<byte[]> GetIds(R r, int len, string what) => Many(r, x => Fixed(x, len, what));
    private static List<byte[]> GetBytesList(R r) => Many(r, x => x.Bytes());

    private static void PutAnyIds(W w, IReadOnlyList<byte[]> ids) { w.List(ids.Count); foreach (var id in ids) w.Bytes(id); }

    // ── shared structures ────────────────────────────────────────────────────

    private static void PutKey(W w, ResourceKey k) => w.Str(k.Namespace).Bytes(k.Key);
    private static ResourceKey GetKey(R r) => new(r.Str(), r.Bytes());

    private static void PutLogId(W w, LogId l) => w.Uint(l.Term).Bytes(l.LeaderNodeId).Uint(l.Index);
    private static LogId GetLogId(R r) => new(r.Uint(), r.Bytes(), r.Uint());

    private static void PutFence(W w, CoordFence f)
    {
        w.Bytes(Check(f.GroupId, 32, "fence group id"));
        PutKey(w, f.Resource);
        w.Uint(f.Epoch).Bytes(Check(f.Holder, 32, "fence holder"));
    }

    private static CoordFence GetFence(R r)
    {
        byte[] group = Fixed(r, 32, "fence group id");
        var key = GetKey(r);
        ulong epoch = r.Uint();
        return new CoordFence(group, key, epoch, Fixed(r, 32, "fence holder"));
    }

    private static void PutSnapshot(W w, ResourceSnapshot s)
    {
        w.Bytes(s.GroupId);
        PutKey(w, s.Resource);
        w.Uint(s.Epoch);
        PutOpt(w, s.Holder, (x, b) => x.Bytes(b));
        w.Bool(s.Held).Uint(s.TtlMs).Uint(s.LeaseRevision).Uint(s.ValueRevision).Uint(s.AclRevision);
        PutOpt(w, s.Value, (x, b) => x.Bytes(b));
        w.Bool(s.ValueRedacted);
    }

    private static ResourceSnapshot GetSnapshot(R r)
    {
        byte[] group = r.Bytes();
        var key = GetKey(r);
        ulong epoch = r.Uint();
        byte[]? holder = Opt(r, x => x.Bytes());
        bool held = r.Bool();
        ulong ttl = r.Uint(), lease = r.Uint(), value = r.Uint(), acl = r.Uint();
        byte[]? val = Opt(r, x => x.Bytes());
        bool redacted = r.Bool();
        return new ResourceSnapshot(group, key, epoch, holder, held, ttl, lease, value, acl, val, redacted);
    }

    private static void PutSeed(W w, MemberSeed s)
    {
        w.Bytes(s.NodeId).Str(s.Ticket);
        PutOpt(w, s.SignedJoinPermit, (x, b) => x.Bytes(b));
    }

    private static MemberSeed GetSeed(R r) => new(r.Bytes(), r.Str(), Opt(r, x => x.Bytes()));

    // ── D0 MutationRequest ───────────────────────────────────────────────────

    public static byte[] EncodeMutationRequest(MutationRequest m)
    {
        var w = new W(TagMutationRequest);
        switch (m)
        {
            case MutationRequest.Acquire a: w.U8(1); PutKey(w, a.Resource); w.Uint(a.TtlMs); break;
            case MutationRequest.Renew a: w.U8(2); PutFence(w, a.Fence); break;
            case MutationRequest.CompareAndSet a: w.U8(3); PutFence(w, a.Fence); w.Uint(a.ExpectedValueRevision).Bytes(a.NewValue); break;
            case MutationRequest.Release a: w.U8(4); PutFence(w, a.Fence); break;
            case MutationRequest.ReleaseWithValue a: w.U8(5); PutFence(w, a.Fence); w.Uint(a.ExpectedValueRevision).Bytes(a.FinalValue); break;
            case MutationRequest.Revoke a:
                w.U8(6); PutKey(w, a.Resource);
                if (a.ExpectedEpoch is ulong e) w.Present().Uint(e); else w.Absent();
                break;
            case MutationRequest.SetGrant a:
                w.U8(7); PutKey(w, a.Resource);
                w.Bytes(Check(a.TargetNodeId, 32, "target node id")).Uint(a.Rights).Uint(a.ExpectedAclRevision);
                break;
            case MutationRequest.SetAdmin a:
                w.U8(8).Bytes(Check(a.TargetNodeId, 32, "target node id")).Bool(a.Enabled).Uint(a.ExpectedAdminRevision);
                break;
            case MutationRequest.ChangeMembership a:
                w.U8(9).Bytes(Check(a.OperationId, 16, "operation id"));
                PutOpt(w, a.ExpectedMembershipLogId, PutLogId);
                PutIds(w, a.DesiredVoters, 32, "desired voter");
                PutIds(w, a.RetainedLearners, 32, "retained learner");
                PutMany(w, a.MemberSeeds, PutSeed);
                break;
            case MutationRequest.CancelMembership a:
                w.U8(10).Bytes(Check(a.OperationId, 16, "operation id")).Uint(a.ExpectedPhaseCode);
                break;
            default: throw new ArgumentException("unknown mutation", nameof(m));
        }
        return w.Done();
    }

    public static MutationRequest DecodeMutationRequest(byte[] buf)
    {
        var r = new R(buf, TagMutationRequest);
        byte op = r.U8();
        MutationRequest m;
        switch (op)
        {
            case 1: m = new MutationRequest.Acquire(GetKey(r), r.Uint()); break;
            case 2: m = new MutationRequest.Renew(GetFence(r)); break;
            case 3: { var f = GetFence(r); m = new MutationRequest.CompareAndSet(f, r.Uint(), r.Bytes()); break; }
            case 4: m = new MutationRequest.Release(GetFence(r)); break;
            case 5: { var f = GetFence(r); m = new MutationRequest.ReleaseWithValue(f, r.Uint(), r.Bytes()); break; }
            case 6: { var k = GetKey(r); m = new MutationRequest.Revoke(k, r.Present() ? r.Uint() : null); break; }
            case 7: { var k = GetKey(r); byte[] t = Fixed(r, 32, "target node id"); m = new MutationRequest.SetGrant(k, t, r.Uint(), r.Uint()); break; }
            case 8: { byte[] t = Fixed(r, 32, "target node id"); m = new MutationRequest.SetAdmin(t, r.Bool(), r.Uint()); break; }
            case 9:
            {
                byte[] opId = Fixed(r, 16, "operation id");
                var exp = Opt(r, GetLogId);
                var voters = GetIds(r, 32, "desired voter");
                var learners = GetIds(r, 32, "retained learner");
                m = new MutationRequest.ChangeMembership(opId, exp, voters, learners, Many(r, GetSeed));
                break;
            }
            case 10: { byte[] opId = Fixed(r, 16, "operation id"); m = new MutationRequest.CancelMembership(opId, Code(r)); break; }
            default: throw Bad($"unknown mutation op {op}");
        }
        r.End();
        return m;
    }

    // ── D1 ReadRequest ───────────────────────────────────────────────────────

    public static byte[] EncodeReadRequest(ReadRequest q)
    {
        var w = new W(TagReadRequest);
        switch (q)
        {
            case ReadRequest.ReadLinearizable a: w.U8(1); PutKey(w, a.Resource); break;
            case ReadRequest.InspectAcl a: w.U8(2); PutKey(w, a.Resource); break;
            case ReadRequest.InspectAdmins: w.U8(3); break;
            case ReadRequest.MembershipStatus a:
                w.U8(4);
                if (a.OperationId == null) w.Absent(); else w.Present().Bytes(Check(a.OperationId, 16, "operation id"));
                break;
            case ReadRequest.GroupStatus: w.U8(5); break;
            case ReadRequest.ProbeClient a: w.U8(6).Bytes(Check(a.TargetNodeId, 32, "target node id")).Bytes(Check(a.TargetClientId, 16, "target client id")); break;
            case ReadRequest.AuditStream a:
                if (a.Limit is < 1 or > 10000) throw new ArgumentOutOfRangeException(nameof(q), "audit limit must be 1..=10000");
                w.U8(7); PutOpt(w, a.After, PutLogId); w.Uint(a.Limit);
                break;
            case ReadRequest.RecoveryExport: w.U8(8); break;
            default: throw new ArgumentException("unknown read", nameof(q));
        }
        return w.Done();
    }

    public static ReadRequest DecodeReadRequest(byte[] buf)
    {
        var r = new R(buf, TagReadRequest);
        byte op = r.U8();
        ReadRequest q;
        switch (op)
        {
            case 1: q = new ReadRequest.ReadLinearizable(GetKey(r)); break;
            case 2: q = new ReadRequest.InspectAcl(GetKey(r)); break;
            case 3: q = new ReadRequest.InspectAdmins(); break;
            case 4: q = new ReadRequest.MembershipStatus(r.Present() ? Fixed(r, 16, "operation id") : null); break;
            case 5: q = new ReadRequest.GroupStatus(); break;
            case 6: { byte[] n = Fixed(r, 32, "target node id"); q = new ReadRequest.ProbeClient(n, Fixed(r, 16, "target client id")); break; }
            case 7:
            {
                var after = Opt(r, GetLogId);
                ulong limit = r.Uint();
                if (limit is < 1 or > 10000) throw Bad($"audit limit {limit} out of 1..=10000");
                q = new ReadRequest.AuditStream(after, (uint)limit);
                break;
            }
            case 8: q = new ReadRequest.RecoveryExport(); break;
            default: throw Bad($"unknown read op {op}");
        }
        r.End();
        return q;
    }

    // ── D2 .. DE ─────────────────────────────────────────────────────────────

    public static byte[] EncodeMutationReply(MutationReply v)
    {
        var w = new W(TagMutationReply).Bytes(v.ClientId).Uint(v.Sequence).Bytes(v.RequestHash).Uint(v.DispositionCode).Uint(v.OutcomeCode).Uint(v.ReasonCode);
        PutOpt(w, v.Snapshot, PutSnapshot);
        w.Uint(v.AclRevision).Uint(v.AdminRevision);
        PutOpt(w, v.OperationId, (x, b) => x.Bytes(b));
        PutOpt(w, v.MembershipLogId, PutLogId);
        return w.Done();
    }

    public static MutationReply DecodeMutationReply(byte[] buf)
    {
        var r = new R(buf, TagMutationReply);
        byte[] client = r.Bytes();
        ulong seq = r.Uint();
        byte[] hash = r.Bytes();
        uint disp = Code(r), outcome = Code(r), reason = Code(r);
        var snap = Opt(r, GetSnapshot);
        ulong acl = r.Uint(), admin = r.Uint();
        byte[]? op = Opt(r, x => x.Bytes());
        var log = Opt(r, GetLogId);
        r.End();
        return new MutationReply(client, seq, hash, disp, outcome, reason, snap, acl, admin, op, log);
    }

    public static byte[] EncodeReadReply(ResourceSnapshot s)
    {
        var w = new W(TagReadReply);
        PutSnapshot(w, s);
        return w.Done();
    }

    public static ResourceSnapshot DecodeReadReply(byte[] buf)
    {
        var r = new R(buf, TagReadReply);
        var s = GetSnapshot(r);
        r.End();
        return s;
    }

    public static byte[] EncodeAclStatus(AclStatus v)
    {
        var w = new W(TagAclStatus);
        PutKey(w, v.Resource);
        w.Uint(v.AclRevision);
        PutMany(w, v.Grants, (x, g) => x.Bytes(g.SubjectNodeId).Uint(g.Rights));
        return w.Done();
    }

    public static AclStatus DecodeAclStatus(byte[] buf)
    {
        var r = new R(buf, TagAclStatus);
        var key = GetKey(r);
        ulong rev = r.Uint();
        var grants = Many(r, x => new AclGrant(x.Bytes(), x.Uint()));
        r.End();
        return new AclStatus(key, rev, grants);
    }

    public static byte[] EncodeAdminStatus(AdminStatus v)
    {
        var w = new W(TagAdminStatus).Uint(v.AdminRevision);
        PutAnyIds(w, v.Administrators);
        return w.Done();
    }

    public static AdminStatus DecodeAdminStatus(byte[] buf)
    {
        var r = new R(buf, TagAdminStatus);
        ulong rev = r.Uint();
        var admins = GetBytesList(r);
        r.End();
        return new AdminStatus(rev, admins);
    }

    public static byte[] EncodeMembershipStatus(MembershipStatus v)
    {
        var w = new W(TagMembershipStatus);
        PutOpt(w, v.OperationId, (x, b) => x.Bytes(b));
        w.Uint(v.StatusCode);
        PutOpt(w, v.ExpectedMembershipLogId, PutLogId);
        PutOpt(w, v.ObservedMembershipLogId, PutLogId);
        PutAnyIds(w, v.DesiredVoters);
        PutAnyIds(w, v.RetainedLearners);
        w.Uint(v.ErrorCode);
        return w.Done();
    }

    public static MembershipStatus DecodeMembershipStatus(byte[] buf)
    {
        var r = new R(buf, TagMembershipStatus);
        var op = Opt(r, x => x.Bytes());
        uint status = Code(r);
        var expected = Opt(r, GetLogId);
        var observed = Opt(r, GetLogId);
        var voters = GetBytesList(r);
        var learners = GetBytesList(r);
        uint err = Code(r);
        r.End();
        return new MembershipStatus(op, status, expected, observed, voters, learners, err);
    }

    public static byte[] EncodeGroupStatus(GroupStatus v)
    {
        var w = new W(TagGroupStatus).Uint(v.LifecycleCode).Bytes(v.LocalNodeId).Uint(v.LocalRoleCode);
        PutOpt(w, v.LeaderNodeId, (x, b) => x.Bytes(b));
        w.Uint(v.Term);
        PutOpt(w, v.LastLogId, PutLogId);
        PutOpt(w, v.CommittedLogId, PutLogId);
        PutOpt(w, v.AppliedLogId, PutLogId);
        w.Uint(v.DeploymentProfileCode);
        PutOpt(w, v.MembershipLogId, PutLogId);
        w.Bool(v.MembershipJoint);
        PutAnyIds(w, v.Voters);
        PutAnyIds(w, v.Learners);
        PutOpt(w, v.PendingOperationId, (x, b) => x.Bytes(b));
        if (v.QuorumAvailable is bool q) w.Present().Bool(q); else w.Absent();
        w.Uint(v.FatalCode);
        PutOpt(w, v.AuditFloor, PutLogId);
        w.Bytes(v.GenesisHash).Bytes(v.AuthorityPublicKey).Str(v.AuthorityDomain);
        PutOpt(w, v.RecoveredFrom, (x, rf) =>
        {
            x.Bytes(rf.SourceGroupId).Bytes(rf.SourceGenesisHash).Bytes(rf.SourceAuthorityPublicKey).Str(rf.SourceAuthorityDomain).Bytes(rf.ExportHash);
            PutOpt(x, rf.SourceLastApplied, PutLogId);
        });
        return w.Done();
    }

    public static GroupStatus DecodeGroupStatus(byte[] buf)
    {
        var r = new R(buf, TagGroupStatus);
        uint lifecycle = Code(r);
        byte[] local = r.Bytes();
        uint role = Code(r);
        var leader = Opt(r, x => x.Bytes());
        ulong term = r.Uint();
        var last = Opt(r, GetLogId);
        var committed = Opt(r, GetLogId);
        var applied = Opt(r, GetLogId);
        uint profile = Code(r);
        var mlog = Opt(r, GetLogId);
        bool joint = r.Bool();
        var voters = GetBytesList(r);
        var learners = GetBytesList(r);
        var pending = Opt(r, x => x.Bytes());
        bool? quorum = r.Present() ? r.Bool() : null;
        uint fatal = Code(r);
        var floor = Opt(r, GetLogId);
        byte[] genesis = r.Bytes(), authKey = r.Bytes();
        string domain = r.Str();
        var recovered = Opt(r, x =>
        {
            byte[] g = x.Bytes(), h = x.Bytes(), k = x.Bytes();
            string d = x.Str();
            byte[] e = x.Bytes();
            return new RecoveredFrom(g, h, k, d, e, Opt(x, GetLogId));
        });
        r.End();
        return new GroupStatus(lifecycle, local, role, leader, term, last, committed, applied, profile, mlog, joint, voters, learners,
            pending, quorum, fatal, floor, genesis, authKey, domain, recovered);
    }

    public static byte[] EncodeClientProbe(ClientProbe v)
        => new W(TagClientProbe).Bytes(v.TargetNodeId).Bytes(v.TargetClientId).Bool(v.Exists).Uint(v.LastSequence).Bytes(v.RequestHash)
            .Uint(v.DispositionCode).Bool(v.Retired).Done();

    public static ClientProbe DecodeClientProbe(byte[] buf)
    {
        var r = new R(buf, TagClientProbe);
        var v = new ClientProbe(r.Bytes(), r.Bytes(), r.Bool(), r.Uint(), r.Bytes(), Code(r), r.Bool());
        r.End();
        return v;
    }

    public static byte[] EncodeAuditRecords(IReadOnlyList<AuditRecord> v)
    {
        var w = new W(TagAuditRecords);
        PutMany(w, v, (x, a) =>
        {
            PutLogId(x, a.LogId);
            PutOpt(x, a.ActorNodeId, (y, b) => y.Bytes(b));
            x.Uint(a.OperationCode);
            PutOpt(x, a.Resource, PutKey);
            x.Uint(a.ResultCode).Uint(a.Epoch).Uint(a.ValueRevision);
        });
        return w.Done();
    }

    public static List<AuditRecord> DecodeAuditRecords(byte[] buf)
    {
        var r = new R(buf, TagAuditRecords);
        var v = Many(r, x =>
        {
            var log = GetLogId(x);
            var actor = Opt(x, y => y.Bytes());
            uint op = Code(x);
            var res = Opt(x, GetKey);
            uint result = Code(x);
            return new AuditRecord(log, actor, op, res, result, x.Uint(), x.Uint());
        });
        r.End();
        return v;
    }

    public static byte[] EncodeRecoveryExport(RecoveryExport v)
    {
        var w = new W(TagRecoveryExport).Uint(v.TotalLen).Bytes(v.Blake3Checksum);
        PutOpt(w, v.AppliedLogId, PutLogId);
        return w.Bytes(v.Body).Done();
    }

    public static RecoveryExport DecodeRecoveryExport(byte[] buf)
    {
        var r = new R(buf, TagRecoveryExport);
        ulong total = r.Uint();
        byte[] sum = r.Bytes();
        var applied = Opt(r, GetLogId);
        var v = new RecoveryExport(total, sum, applied, r.Bytes());
        r.End();
        return v;
    }

    public static byte[] EncodeRecoveryExportManifest(RecoveryExportManifest v)
    {
        var w = new W(TagRecoveryExportManifest).Uint(v.TotalLen).Bytes(v.Blake3Checksum);
        PutOpt(w, v.AppliedLogId, PutLogId);
        return w.Done();
    }

    public static RecoveryExportManifest DecodeRecoveryExportManifest(byte[] buf)
    {
        var r = new R(buf, TagRecoveryExportManifest);
        ulong total = r.Uint();
        byte[] sum = r.Bytes();
        var v = new RecoveryExportManifest(total, sum, Opt(r, GetLogId));
        r.End();
        return v;
    }

    public static byte[] EncodeRetireReply(RetireReply v) => new W(TagRetireReply).Uint(v.OutcomeCode).Uint(v.ReasonCode).Done();

    public static RetireReply DecodeRetireReply(byte[] buf)
    {
        var r = new R(buf, TagRetireReply);
        var v = new RetireReply(Code(r), Code(r));
        r.End();
        return v;
    }

    public static byte[] EncodeCoordinationFence(CoordFence f)
    {
        var w = new W(TagCoordinationFence);
        PutFence(w, f);
        return w.Done();
    }

    public static CoordFence DecodeCoordinationFence(byte[] buf)
    {
        var r = new R(buf, TagCoordinationFence);
        var f = GetFence(r);
        r.End();
        return f;
    }

    public static byte[] EncodePlacementReport(PlacementReport v)
    {
        var w = new W(TagPlacementReport).Uint(v.PolicyRevision).Bytes(Check(v.PolicyDigest, 32, "policy digest"));
        PutOpt(w, v.MembershipLogId, PutLogId);
        PutAnyIds(w, v.CurrentVoters);
        PutAnyIds(w, v.CurrentLearners);
        PutAnyIds(w, v.SelectedVoters);
        PutAnyIds(w, v.SelectedLearners);
        PutAnyIds(w, v.GraceCrossedMembers);
        PutOpt(w, v.Separation, (x, s) => x.Uint(s.SeparatedPairs).Uint(s.ConnectedPairs).Uint(s.UnknownPairs).Uint(s.TotalPairs).Uint(s.RepresentedClusters).Bool(s.DiversityProved));
        PutOpt(w, v.PendingOperationId, (x, b) => x.Bytes(b));
        return w.Str(v.Reason).Done();
    }

    public static PlacementReport DecodePlacementReport(byte[] buf)
    {
        var r = new R(buf, TagPlacementReport);
        ulong rev = r.Uint();
        byte[] digest = Fixed(r, 32, "policy digest");
        var log = Opt(r, GetLogId);
        var cv = GetBytesList(r);
        var cl = GetBytesList(r);
        var sv = GetBytesList(r);
        var sl = GetBytesList(r);
        var grace = GetBytesList(r);
        var sep = Opt(r, x => new PlacementSeparation(U8Field(x, "separated"), U8Field(x, "connected"), U8Field(x, "unknown"), U8Field(x, "total"), U8Field(x, "clusters"), x.Bool()));
        var pending = Opt(r, x => x.Bytes());
        string reason = r.Str();
        r.End();
        return new PlacementReport(rev, digest, log, cv, cl, sv, sl, grace, sep, pending, reason);
    }

    // ── placement providers: BC .. BF ────────────────────────────────────────

    public static byte[] EncodePlacementPolicyRequest() => new W(TagPlacementPolicyRequest).Done();

    public static void DecodePlacementPolicyRequest(byte[] buf) => new R(buf, TagPlacementPolicyRequest).End();

    private static void Ascending(IReadOnlyList<byte[]> ids, string what)
    {
        for (int i = 1; i < ids.Count; i++)
            if (ids[i - 1].AsSpan().SequenceCompareTo(ids[i]) >= 0) throw Bad($"{what} not strictly ascending");
    }

    public static byte[] EncodePlacementPolicy(PlacementPolicy p)
    {
        var w = new W(TagPlacementPolicy).Uint(p.Revision).Bytes(Check(p.Digest, 32, "digest")).Bytes(Check(p.AuthorityPublicKey, 32, "authority key"))
            .Str(p.AuthorityDomain).Uint(p.TargetVoters).Uint(p.TargetRetainedLearners);
        w.List(p.Eligible.Count);
        foreach (var c in p.Eligible)
        {
            w.Bytes(Check(c.NodeId, 32, "eligible node id"));
            PutOpt(w, c.MemberSeed, PutSeed);
        }
        PutIds(w, p.PinnedVoters, 32, "pinned voter");
        PutIds(w, p.Excluded, 32, "excluded");
        return w.Uint(p.MinimumSeparatedVoterPairs).Uint(p.ReplacementGraceMs).Bool(p.AllowHealthyRebalance).Done();
    }

    public static PlacementPolicy DecodePlacementPolicy(byte[] buf)
    {
        var r = new R(buf, TagPlacementPolicy);
        ulong rev = r.Uint();
        byte[] digest = Fixed(r, 32, "digest"), key = Fixed(r, 32, "authority key");
        string domain = r.Str();
        byte tv = U8Field(r, "target_voters"), tl = U8Field(r, "target_retained_learners");
        var eligible = Many(r, x => new PlacementCandidate(Fixed(x, 32, "eligible node id"), Opt(x, GetSeed)));
        Ascending(eligible.ConvertAll(c => c.NodeId), "eligible");
        var pinned = GetIds(r, 32, "pinned voter");
        Ascending(pinned, "pinned_voters");
        var excluded = GetIds(r, 32, "excluded");
        Ascending(excluded, "excluded");
        byte minSep = U8Field(r, "minimum_separated_voter_pairs");
        ulong grace = r.Uint();
        bool rebalance = r.Bool();
        r.End();
        return new PlacementPolicy(rev, digest, key, domain, tv, tl, eligible, pinned, excluded, minSep, grace, rebalance);
    }

    public static byte[] EncodeProvisionRequest(ProvisionRequest p)
    {
        var w = new W(TagPlacementProvisionRequest).Bytes(Check(p.GroupId, 32, "group id")).Bytes(Check(p.PolicyDigest, 32, "policy digest"));
        PutOpt(w, p.MembershipLogId, PutLogId);
        return w.Bytes(Check(p.NodeId, 32, "node id")).Done();
    }

    public static ProvisionRequest DecodeProvisionRequest(byte[] buf)
    {
        var r = new R(buf, TagPlacementProvisionRequest);
        byte[] group = Fixed(r, 32, "group id"), digest = Fixed(r, 32, "policy digest");
        var log = Opt(r, GetLogId);
        var v = new ProvisionRequest(group, digest, log, Fixed(r, 32, "node id"));
        r.End();
        return v;
    }

    public static byte[] EncodeMemberSeed(MemberSeed s)
    {
        var w = new W(TagMemberSeed);
        PutSeed(w, s);
        return w.Done();
    }

    public static MemberSeed DecodeMemberSeed(byte[] buf)
    {
        var r = new R(buf, TagMemberSeed);
        var s = GetSeed(r);
        r.End();
        return s;
    }
}
