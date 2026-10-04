using System;
using System.Collections.Generic;
using System.Linq;
using Aster.Coordination;
using Aster.Upcall;

namespace Aster.Tests;

/// <summary>conformance/vectors/coordination.json (UCv1 D0-DE): every valid case round-trips byte for byte, every invalid input is refused.</summary>
public class CoordinationVectorTests
{
    private static readonly Dictionary<string, Func<byte[], byte[]>> RoundTrip = new()
    {
        ["MutationRequest"] = b => CoordinationCodec.EncodeMutationRequest(CoordinationCodec.DecodeMutationRequest(b)),
        ["ReadRequest"] = b => CoordinationCodec.EncodeReadRequest(CoordinationCodec.DecodeReadRequest(b)),
        ["MutationReply"] = b => CoordinationCodec.EncodeMutationReply(CoordinationCodec.DecodeMutationReply(b)),
        ["ReadReply"] = b => CoordinationCodec.EncodeReadReply(CoordinationCodec.DecodeReadReply(b)),
        ["AclStatus"] = b => CoordinationCodec.EncodeAclStatus(CoordinationCodec.DecodeAclStatus(b)),
        ["AdminStatus"] = b => CoordinationCodec.EncodeAdminStatus(CoordinationCodec.DecodeAdminStatus(b)),
        ["MembershipStatus"] = b => CoordinationCodec.EncodeMembershipStatus(CoordinationCodec.DecodeMembershipStatus(b)),
        ["GroupStatus"] = b => CoordinationCodec.EncodeGroupStatus(CoordinationCodec.DecodeGroupStatus(b)),
        ["ClientProbe"] = b => CoordinationCodec.EncodeClientProbe(CoordinationCodec.DecodeClientProbe(b)),
        ["AuditRecords"] = b => CoordinationCodec.EncodeAuditRecords(CoordinationCodec.DecodeAuditRecords(b)),
        ["RecoveryExport"] = b => CoordinationCodec.EncodeRecoveryExport(CoordinationCodec.DecodeRecoveryExport(b)),
        ["RecoveryExportManifest"] = b => CoordinationCodec.EncodeRecoveryExportManifest(CoordinationCodec.DecodeRecoveryExportManifest(b)),
        ["RetireReply"] = b => CoordinationCodec.EncodeRetireReply(CoordinationCodec.DecodeRetireReply(b)),
        ["CoordinationFence"] = b => CoordinationCodec.EncodeCoordinationFence(CoordinationCodec.DecodeCoordinationFence(b)),
        ["PlacementReport"] = b => CoordinationCodec.EncodePlacementReport(CoordinationCodec.DecodePlacementReport(b)),
    };

    public static IEnumerable<object[]> Valid()
        => Vectors.Load("coordination.json").GetProperty("valid").EnumerateArray().Select(v => new object[] { v.Str("name") });

    public static IEnumerable<object[]> Invalid()
        => Vectors.Load("coordination.json").GetProperty("invalid").EnumerateArray().Select(v => new object[] { v.Str("name") });

    [Theory]
    [MemberData(nameof(Valid))]
    public void Valid_Vector_Round_Trips_Byte_For_Byte(string name)
    {
        var v = Vectors.Load("coordination.json").GetProperty("valid").EnumerateArray().First(x => x.Str("name") == name);
        byte[] bytes = Convert.FromHexString(v.Str("hex"));
        Assert.Equal(bytes, RoundTrip[v.Str("type")](bytes));
    }

    [Theory]
    [MemberData(nameof(Invalid))]
    public void Invalid_Vector_Is_Refused(string name)
    {
        var v = Vectors.Load("coordination.json").GetProperty("invalid").EnumerateArray().First(x => x.Str("name") == name);
        byte[] bytes = Convert.FromHexString(v.Str("hex"));
        string type = v.Str("type");
        Assert.Throws<Ucv1DecodeException>(() => RoundTrip[type](bytes));
    }

    [Fact]
    public void Counts_And_Tables_Match_The_Vector_File()
    {
        var doc = Vectors.Load("coordination.json");
        Assert.Equal(40, doc.GetProperty("valid").GetArrayLength());
        Assert.Equal(22, doc.GetProperty("invalid").GetArrayLength());
        Assert.Equal(RoundTrip.Keys.OrderBy(x => x), doc.GetProperty("valid").EnumerateArray().Select(v => v.Str("type")).Distinct().OrderBy(x => x));
        var tags = doc.GetProperty("tags").EnumerateArray().ToDictionary(t => t.Str("name"), t => t.GetProperty("tag").GetInt32());
        Assert.Equal(0xD0, tags["MutationRequest"]);
        Assert.Equal(CoordinationCodec.TagMutationRequest, (byte)tags["MutationRequest"]);
        Assert.Equal(CoordinationCodec.TagReadRequest, (byte)tags["ReadRequest"]);
        Assert.Equal(CoordinationCodec.TagMutationReply, (byte)tags["MutationReply"]);
        Assert.Equal(CoordinationCodec.TagReadReply, (byte)tags["ReadReply"]);
        Assert.Equal(CoordinationCodec.TagAclStatus, (byte)tags["AclStatus"]);
        Assert.Equal(CoordinationCodec.TagAdminStatus, (byte)tags["AdminStatus"]);
        Assert.Equal(CoordinationCodec.TagMembershipStatus, (byte)tags["MembershipStatus"]);
        Assert.Equal(CoordinationCodec.TagGroupStatus, (byte)tags["GroupStatus"]);
        Assert.Equal(CoordinationCodec.TagClientProbe, (byte)tags["ClientProbe"]);
        Assert.Equal(CoordinationCodec.TagAuditRecords, (byte)tags["AuditRecords"]);
        Assert.Equal(CoordinationCodec.TagRecoveryExport, (byte)tags["RecoveryExport"]);
        Assert.Equal(CoordinationCodec.TagRecoveryExportManifest, (byte)tags["RecoveryExportManifest"]);
        Assert.Equal(CoordinationCodec.TagRetireReply, (byte)tags["RetireReply"]);
        Assert.Equal(CoordinationCodec.TagCoordinationFence, (byte)tags["CoordinationFence"]);
        Assert.Equal(CoordinationCodec.TagPlacementReport, (byte)tags["PlacementReport"]);
        foreach (var op in doc.GetProperty("mutation_ops").EnumerateArray())
            _ = op.GetProperty("code").GetInt32(); // throws if absent or not an int
    }

    [Fact]
    public void Decoded_Values_Match_The_Vector_Rendering()
    {
        var valid = Vectors.Load("coordination.json").GetProperty("valid").EnumerateArray().ToList();
        byte[] Hex(string n) => Convert.FromHexString(valid.First(x => x.Str("name") == n).Str("hex"));

        var acquire = Assert.IsType<MutationRequest.Acquire>(CoordinationCodec.DecodeMutationRequest(Hex("mutate: acquire")));
        Assert.Equal(new ResourceKey("jobs", "tree/42"), acquire.Resource);
        Assert.Equal(30000ul, acquire.TtlMs);

        var cas = Assert.IsType<MutationRequest.CompareAndSet>(CoordinationCodec.DecodeMutationRequest(Hex("mutate: compare_and_set")));
        Assert.Equal(7ul, cas.Fence.Epoch);
        Assert.Equal(2ul, cas.ExpectedValueRevision);
        Assert.Equal(new byte[] { 0x76, 0x31 }, cas.NewValue);

        var any = Assert.IsType<MutationRequest.Revoke>(CoordinationCodec.DecodeMutationRequest(Hex("mutate: revoke, any epoch")));
        Assert.Null(any.ExpectedEpoch);

        var reply = CoordinationCodec.DecodeMutationReply(Hex("mutation reply: mutated"));
        Assert.Equal(9ul, reply.Sequence);
        Assert.False(reply.IsRejected);
        Assert.Equal(7ul, reply.Snapshot!.Epoch);
        var rejected = CoordinationCodec.DecodeMutationReply(Hex("mutation reply: rejected"));
        Assert.True(rejected.IsRejected);
        Assert.Equal(4u, rejected.ReasonCode);
        Assert.Null(rejected.Snapshot);

        var audit = CoordinationCodec.DecodeAuditRecords(Hex("audit records"));
        Assert.Equal(2, audit.Count);
        Assert.Null(audit[1].ActorNodeId);
        Assert.Null(audit[1].Resource);

        var seeds = CoordinationCodec.DecodeMutationRequest(Hex("mutate: change_membership"));
        var cm = Assert.IsType<MutationRequest.ChangeMembership>(seeds);
        Assert.Equal(2, cm.DesiredVoters.Count);
        Assert.Equal("aster1ticket", cm.MemberSeeds[0].Ticket);
        Assert.Equal(new byte[] { 0xde, 0xad }, cm.MemberSeeds[0].SignedJoinPermit);
    }

    [Fact]
    public void Encoders_Refuse_Wrong_Id_Lengths()
    {
        var key = new ResourceKey("jobs", "k");
        Assert.Throws<ArgumentException>(() => CoordinationCodec.EncodeMutationRequest(new MutationRequest.SetAdmin(new byte[31], true, 0)));
        Assert.Throws<ArgumentException>(() => CoordinationCodec.EncodeReadRequest(new ReadRequest.ProbeClient(new byte[32], new byte[17])));
        Assert.Throws<ArgumentOutOfRangeException>(() => CoordinationCodec.EncodeReadRequest(new ReadRequest.AuditStream(null, 0)));
        Assert.Throws<ArgumentException>(() => CoordinationCodec.EncodeMutationRequest(new MutationRequest.Renew(new CoordFence(new byte[33], key, 1, new byte[32]))));
    }

    [Fact]
    public void Placement_Provider_Messages_Round_Trip_The_Upcall_Vectors()
    {
        // BC..BF live in upcalls.json (the Java/4.1 codec skipped them: no coordination client then).
        var valid = Vectors.Load("upcalls.json").GetProperty("valid").EnumerateArray().ToList();
        foreach (var v in valid)
        {
            byte[] bytes = Convert.FromHexString(v.Str("hex"));
            switch (v.Str("type"))
            {
                case "PlacementPolicyRequest": CoordinationCodec.DecodePlacementPolicyRequest(bytes); Assert.Equal(bytes, CoordinationCodec.EncodePlacementPolicyRequest()); break;
                case "PlacementPolicy": Assert.Equal(bytes, CoordinationCodec.EncodePlacementPolicy(CoordinationCodec.DecodePlacementPolicy(bytes))); break;
                case "PlacementProvisionRequest": Assert.Equal(bytes, CoordinationCodec.EncodeProvisionRequest(CoordinationCodec.DecodeProvisionRequest(bytes))); break;
                case "MemberSeed": Assert.Equal(bytes, CoordinationCodec.EncodeMemberSeed(CoordinationCodec.DecodeMemberSeed(bytes))); break;
            }
        }
        // A policy whose eligible set is not strictly ascending is refused.
        var policy = new PlacementPolicy(1, new byte[32], new byte[32], "example.org", 3, 0,
            [new PlacementCandidate(Enumerable.Repeat((byte)2, 32).ToArray(), null), new PlacementCandidate(Enumerable.Repeat((byte)1, 32).ToArray(), null)],
            [], [], 0, 0, false);
        Assert.Throws<Ucv1DecodeException>(() => CoordinationCodec.DecodePlacementPolicy(CoordinationCodec.EncodePlacementPolicy(policy)));
    }
}
