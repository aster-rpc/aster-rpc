using System.Text.Json;
using System.Text.Json.Serialization;

namespace Aster.Coordination;

/// <summary>Hex32 node ID and its canonical Aster ticket.</summary>
public sealed record VoterSeed(string NodeId, string Ticket);

/// <summary>Version-1 genesis input. IDs, hashes and nonces are hex32 strings.
/// Caller ordering is preserved; encoding alone establishes no authority.</summary>
public sealed record GenesisDocument(string GroupId, string AuthorityDomain, string GenesisNonce,
    int DeploymentProfileCode, IReadOnlyList<VoterSeed> InitialVoters,
    IReadOnlyList<string> InitialAdmins, string? RecoveryExportHash = null);

/// <summary>Version-1 join input with hex32 fields. Native code derives the candidate ticket.</summary>
public sealed record JoinPermitDocument(string GroupId, string AuthorityDomain, string GenesisHash,
    string CandidateNodeId, IReadOnlyList<string> BootstrapSenders, string JoinNonce);

/// <summary>Copied metadata produced by native pinned-key verification, not a native seal or group handle.</summary>
public sealed record VerifiedGenesis(string GenesisHash, string GroupId, string AuthorityPublicKey,
    string AuthorityDomain, int DeploymentProfileCode, IReadOnlyList<VoterSeed> InitialVoters,
    IReadOnlyList<string> InitialAdmins, string? RecoveryExportHash)
{
    [JsonIgnore] public byte[] DocumentBytes { get; init; } = [];
}

/// <summary>Copied verified metadata. Group/genesis are extracted, not compared against independent
/// expectations. Verification does not commit membership or start a voter.</summary>
public sealed record VerifiedJoinPermit(string GroupId, string GenesisHash, string CandidateNodeId,
    IReadOnlyList<string> BootstrapSenders)
{
    [JsonIgnore] public byte[] DocumentBytes { get; init; } = [];
}

/// <summary>Stable native Code plus diagnostic Detail (whose text is not a stable protocol).</summary>
public sealed class ProvisioningException : IrohException
{
    public string Code { get; }
    public string Detail { get; }
    internal ProvisioningException(Status status, string code, string detail)
        : base(status, $"coordination provisioning {code}: {detail}") { Code = code; Detail = detail; }
}

/// <summary>Stateless native coordination provisioning (ABI 4.15). External signers own keys;
/// these helpers neither select policy, sort inputs, create groups nor grant membership.</summary>
public static class Provisioning
{
    private static readonly JsonSerializerOptions JsonOptions = new() { PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower };
    private delegate int Invoke(out ProvisioningResultNative output);
    private static byte[] Call(Invoke invoke)
    {
        ProvisioningResultNative output = default;
        try
        {
            int status = invoke(out output);
            if (status != 0)
            {
                byte[] error = NativeScope.Read(output.error);
                if (error.Length == 0) Native.Check(status, "coordination provisioning");
                using var json = JsonDocument.Parse(error);
                throw new ProvisioningException((Status)status, json.RootElement.GetProperty("code").GetString()!,
                    json.RootElement.GetProperty("detail").GetString()!);
            }
            return NativeScope.Read(output.data);
        }
        finally
        {
            if (output.data.ptr != IntPtr.Zero) Native.aster_string_release(output.data.ptr, output.data.len);
            if (output.error.ptr != IntPtr.Zero) Native.aster_string_release(output.error.ptr, output.error.len);
        }
    }

    public static byte[] CanonicalGenesis(GenesisDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        using var scope = new NativeScope();
        var json = scope.Bytes(JsonSerializer.SerializeToUtf8Bytes(document, JsonOptions));
        return Call((out ProvisioningResultNative output) => Native.aster_coordination_canonical_genesis(json, out output));
    }
    public static byte[] CanonicalJoinPermit(JoinPermitDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        using var scope = new NativeScope();
        var json = scope.Bytes(JsonSerializer.SerializeToUtf8Bytes(document, JsonOptions));
        return Call((out ProvisioningResultNative output) => Native.aster_coordination_canonical_join_permit(json, out output));
    }
    /// <summary>Native domain-separated preimage for an external Ed25519 signer. Does not verify input.</summary>
    public static byte[] GenesisSigningBytes(ReadOnlySpan<byte> document)
    {
        using var scope = new NativeScope(); var bytes = scope.Bytes(document);
        return Call((out ProvisioningResultNative output) => Native.aster_coordination_genesis_signing_bytes(bytes, out output));
    }
    public static byte[] JoinSigningBytes(ReadOnlySpan<byte> document)
    {
        using var scope = new NativeScope(); var bytes = scope.Bytes(document);
        return Call((out ProvisioningResultNative output) => Native.aster_coordination_join_signing_bytes(bytes, out output));
    }
    /// <summary>Hash opaque bytes without asserting authority. Verify external genesis before acting on it.</summary>
    public static byte[] GenesisHash(ReadOnlySpan<byte> document)
    {
        using var scope = new NativeScope(); var bytes = scope.Bytes(document);
        return Call((out ProvisioningResultNative output) => Native.aster_coordination_genesis_hash(bytes, out output));
    }
    /// <summary>Assemble an envelope without verification. Key/signature are raw 32/64-byte values.</summary>
    public static byte[] SignedDocumentFromParts(ReadOnlySpan<byte> document, ReadOnlySpan<byte> authorityKey, ReadOnlySpan<byte> signature)
    {
        using var scope = new NativeScope(); var bytes = scope.Bytes(document); var key = scope.Bytes(authorityKey); var sig = scope.Bytes(signature);
        return Call((out ProvisioningResultNative output) => Native.aster_coordination_signed_document_from_parts(bytes, key, sig, out output));
    }
    public static string CanonicalCandidateTicket(ReadOnlySpan<byte> nodeId)
    {
        using var scope = new NativeScope(); var bytes = scope.Bytes(nodeId);
        return System.Text.Encoding.UTF8.GetString(Call((out ProvisioningResultNative output) => Native.aster_coordination_canonical_candidate_ticket(bytes, out output)));
    }
    /// <summary>Pin a key and domain learned independently. Never trust the envelope's adjacent key.</summary>
    public static VerifiedGenesis VerifyGenesis(ReadOnlySpan<byte> authorityKey, string domain, ReadOnlySpan<byte> envelope)
    {
        ArgumentNullException.ThrowIfNull(domain);
        using var scope = new NativeScope(); var key = scope.Bytes(authorityKey); var name = scope.Utf8(domain); var bytes = scope.Bytes(envelope);
        byte[] result = Call((out ProvisioningResultNative output) => Native.aster_coordination_verify_genesis(key, name, bytes, out output));
        using var json = JsonDocument.Parse(result);
        return JsonSerializer.Deserialize<VerifiedGenesis>(result, JsonOptions)! with
        { DocumentBytes = Convert.FromHexString(json.RootElement.GetProperty("document_bytes").GetString()!) };
    }
    public static VerifiedJoinPermit VerifyJoinPermitForStart(ReadOnlySpan<byte> authorityKey, string domain,
        ReadOnlySpan<byte> candidateId, ReadOnlySpan<byte> envelope)
    {
        ArgumentNullException.ThrowIfNull(domain);
        using var scope = new NativeScope(); var key = scope.Bytes(authorityKey); var name = scope.Utf8(domain);
        var candidate = scope.Bytes(candidateId); var bytes = scope.Bytes(envelope);
        byte[] result = Call((out ProvisioningResultNative output) => Native.aster_coordination_verify_join_permit_for_start(key, name, candidate, bytes, out output));
        using var json = JsonDocument.Parse(result);
        return JsonSerializer.Deserialize<VerifiedJoinPermit>(result, JsonOptions)! with
        { DocumentBytes = Convert.FromHexString(json.RootElement.GetProperty("document_bytes").GetString()!) };
    }
}
