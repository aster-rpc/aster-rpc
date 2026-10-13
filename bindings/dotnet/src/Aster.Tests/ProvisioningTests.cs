using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Nodes;
using Aster.Coordination;

namespace Aster.Tests;

public class ProvisioningTests
{
    private static readonly JsonSerializerOptions Options = new() { PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower };
    private static JsonDocument Vector() => JsonDocument.Parse(File.ReadAllBytes(Path.Combine(AppContext.BaseDirectory, "fixtures", "coordination_provisioning_v1.json")));
    private static byte[] Hex(JsonElement parent, string key) => Convert.FromHexString(parent.GetProperty(key).GetString()!);
    private static string Text(JsonElement parent, string key) => parent.GetProperty(key).GetString()!;
    private static void Code(string expected, Action operation, Status status = Status.VerificationFailed)
    {
        var error = Assert.Throws<ProvisioningException>(operation);
        Assert.Equal(expected, error.Code); Assert.NotEmpty(error.Detail); Assert.Equal(status, error.Status);
    }

    [Fact]
    public void SharedVectorsCoverAllNineOperations()
    {
        using var vector = Vector(); var v = vector.RootElement; var g = v.GetProperty("genesis"); var j = v.GetProperty("join");
        byte[] key = Hex(v, "authority_public_key");
        var genesis = g.GetProperty("request").Deserialize<GenesisDocument>(Options)!;
        var join = j.GetProperty("request").Deserialize<JoinPermitDocument>(Options)!;
        Assert.Equal(Hex(g, "document_bytes"), Provisioning.CanonicalGenesis(genesis));
        Assert.Equal(Hex(j, "document_bytes"), Provisioning.CanonicalJoinPermit(join));
        Assert.Equal(Hex(g, "signing_bytes"), Provisioning.GenesisSigningBytes(Hex(g, "document_bytes")));
        Assert.Equal(Hex(j, "signing_bytes"), Provisioning.JoinSigningBytes(Hex(j, "document_bytes")));
        Assert.Equal(Hex(g, "genesis_hash"), Provisioning.GenesisHash(Hex(g, "document_bytes")));
        Assert.Equal(Hex(g, "signed_document"), Provisioning.SignedDocumentFromParts(Hex(g, "document_bytes"), key, Hex(g, "signature")));
        Assert.Equal(Hex(j, "signed_document"), Provisioning.SignedDocumentFromParts(Hex(j, "document_bytes"), key, Hex(j, "signature")));
        byte[] candidate = Convert.FromHexString(join.CandidateNodeId);
        Assert.Equal(Text(j, "candidate_ticket"), Provisioning.CanonicalCandidateTicket(candidate));
        var verified = Provisioning.VerifyGenesis(key, genesis.AuthorityDomain, Hex(g, "signed_document"));
        Assert.Equal(Hex(g, "document_bytes"), verified.DocumentBytes);
        Assert.Equal(Text(g, "genesis_hash"), verified.GenesisHash);
        Assert.Equal(genesis.GroupId, verified.GroupId); Assert.Equal(Text(v, "authority_public_key"), verified.AuthorityPublicKey);
        var genesisSummary = JsonSerializer.SerializeToNode(verified, Options)!;
        genesisSummary["document_bytes"] = Convert.ToHexString(verified.DocumentBytes).ToLowerInvariant();
        Assert.True(JsonNode.DeepEquals(JsonNode.Parse(g.GetProperty("verified").GetRawText()), genesisSummary));
        Assert.Equal(genesis.InitialVoters, verified.InitialVoters); Assert.Equal(genesis.InitialAdmins, verified.InitialAdmins);
        var permit = Provisioning.VerifyJoinPermitForStart(key, join.AuthorityDomain, candidate, Hex(j, "signed_document"));
        Assert.Equal(Hex(j, "document_bytes"), permit.DocumentBytes); Assert.Equal(join.GroupId, permit.GroupId);
        Assert.Equal(join.GenesisHash, permit.GenesisHash); Assert.Equal(join.CandidateNodeId, permit.CandidateNodeId);
        Assert.Equal(join.BootstrapSenders, permit.BootstrapSenders);
        var joinSummary = JsonSerializer.SerializeToNode(permit, Options)!;
        joinSummary["document_bytes"] = Convert.ToHexString(permit.DocumentBytes).ToLowerInvariant();
        Assert.True(JsonNode.DeepEquals(JsonNode.Parse(j.GetProperty("verified").GetRawText()), joinSummary));
    }

    [Fact]
    public void NativeVerificationPinsTrustAndReportsStableCodes()
    {
        using var vector = Vector(); var v = vector.RootElement; var g = v.GetProperty("genesis"); var j = v.GetProperty("join");
        byte[] key = Hex(v, "authority_public_key"); byte[] wrong = (byte[])key.Clone(); wrong[0] ^= 1;
        var genesis = g.GetProperty("request").Deserialize<GenesisDocument>(Options)!;
        var join = j.GetProperty("request").Deserialize<JoinPermitDocument>(Options)!;
        Code("wrong_authority_key", () => Provisioning.VerifyGenesis(wrong, genesis.AuthorityDomain, Hex(g, "signed_document")));
        Code("wrong_domain", () => Provisioning.VerifyGenesis(key, "wrong-domain", Hex(g, "signed_document")));
        byte[] signature = Hex(g, "signature"); signature[0] ^= 1;
        byte[] tampered = Provisioning.SignedDocumentFromParts(Hex(g, "document_bytes"), key, signature);
        Code("bad_signature", () => Provisioning.VerifyGenesis(key, genesis.AuthorityDomain, tampered));
        byte[] candidate = Convert.FromHexString(join.CandidateNodeId); candidate[0] ^= 1;
        Code("wrong_node", () => Provisioning.VerifyJoinPermitForStart(key, join.AuthorityDomain, candidate, Hex(j, "signed_document")));
        Code("invalid_input", () => Provisioning.CanonicalCandidateTicket([1]), Status.InvalidArgument);
        Code("invalid_input", () => Provisioning.SignedDocumentFromParts([], key, [1]), Status.InvalidArgument);
        Code("invalid_input", () => Provisioning.CanonicalGenesis(genesis with { GroupId = "00" }), Status.InvalidArgument);
    }

    [Fact]
    public void EncodingIsDetachedAndDoesNotChoosePolicy()
    {
        using var vector = Vector(); var g = vector.RootElement.GetProperty("genesis");
        var genesis = g.GetProperty("request").Deserialize<GenesisDocument>(Options)!;
        byte[] original = Provisioning.CanonicalGenesis(genesis);
        Assert.NotEqual(original, Provisioning.CanonicalGenesis(genesis with { DeploymentProfileCode = 99 }));
        original[0] ^= 1;
        Assert.Equal(Hex(g, "document_bytes"), Provisioning.CanonicalGenesis(genesis));
    }

    [Fact]
    public void ResultLayoutAndFloor()
    {
        Assert.Equal(32, Marshal.SizeOf<ProvisioningResultNative>());
        Assert.Equal(0, Marshal.OffsetOf<ProvisioningResultNative>("data").ToInt32());
        Assert.Equal(16, Marshal.OffsetOf<ProvisioningResultNative>("error").ToInt32());
        Assert.Equal(1u, Native.RequiredAbiMinor);
    }
}
