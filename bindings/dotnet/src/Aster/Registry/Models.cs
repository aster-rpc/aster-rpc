using System.Text.Json;
using System.Text.Json.Serialization;

namespace Aster.Registry;

/// <summary>
/// Compact service descriptor returned in ConsumerAdmissionResponse (Aster-SPEC.md §3.2.2).
/// </summary>
public sealed class ServiceSummary
{
    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    [JsonPropertyName("version")]
    public int Version { get; set; }

    [JsonPropertyName("contract_id")]
    public string ContractId { get; set; } = "";

    [JsonPropertyName("channels")]
    public Dictionary<string, string> Channels { get; set; } = new();

    [JsonPropertyName("pattern")]
    public string Pattern { get; set; } = "shared";

    [JsonPropertyName("serialization_modes")]
    public List<string> SerializationModes { get; set; } = new();

    public string ToJson() => JsonSerializer.Serialize(this, RegistryJson.Options);

    public static ServiceSummary FromJson(string json) =>
        JsonSerializer.Deserialize<ServiceSummary>(json, RegistryJson.Options)
            ?? throw new InvalidOperationException("null ServiceSummary");
}

/// <summary>
/// Docs pointer to an immutable Iroh collection (Aster-SPEC.md §11.2.1).
/// Stored at "contracts/{contract_id}" in the registry doc.
/// </summary>
/// <remarks>
/// <see cref="CollectionFormat"/> is "raw" for single-blob (Phase 10 default) or "index"
/// for multi-file collections. Old records without this field default to "raw".
/// </remarks>
public sealed class ArtifactRef
{
    [JsonPropertyName("contract_id")]
    public string ContractId { get; set; } = "";

    [JsonPropertyName("collection_hash")]
    public string CollectionHash { get; set; } = "";

    [JsonPropertyName("provider_endpoint_id")]
    public string? ProviderEndpointId { get; set; }

    [JsonPropertyName("relay_url")]
    public string? RelayUrl { get; set; }

    [JsonPropertyName("ticket")]
    public string? Ticket { get; set; }

    [JsonPropertyName("published_by")]
    public string PublishedBy { get; set; } = "";

    [JsonPropertyName("published_at_epoch_ms")]
    public long PublishedAtEpochMs { get; set; }

    [JsonPropertyName("collection_format")]
    public string CollectionFormat { get; set; } = "raw";

    public string ToJson() => JsonSerializer.Serialize(this, RegistryJson.Options);

    public static ArtifactRef FromJson(string json)
    {
        var r = JsonSerializer.Deserialize<ArtifactRef>(json, RegistryJson.Options)
            ?? throw new InvalidOperationException("null ArtifactRef");
        if (string.IsNullOrEmpty(r.CollectionFormat))
            r.CollectionFormat = "raw";
        return r;
    }
}

/// <summary>
/// Renewable advertisement for a live endpoint (Aster-SPEC.md §11.6).
/// Stored at "services/{name}/contracts/{cid}/endpoints/{eid}".
/// </summary>
public sealed class EndpointLease
{
    [JsonPropertyName("endpoint_id")]
    public string EndpointId { get; set; } = "";

    [JsonPropertyName("contract_id")]
    public string ContractId { get; set; } = "";

    [JsonPropertyName("service")]
    public string Service { get; set; } = "";

    [JsonPropertyName("version")]
    public int Version { get; set; }

    [JsonPropertyName("lease_expires_epoch_ms")]
    public long LeaseExpiresEpochMs { get; set; }

    [JsonPropertyName("lease_seq")]
    public long LeaseSeq { get; set; }

    [JsonPropertyName("alpn")]
    public string Alpn { get; set; } = "aster/1";

    [JsonPropertyName("serialization_modes")]
    public List<string> SerializationModes { get; set; } = new();

    [JsonPropertyName("feature_flags")]
    public List<string> FeatureFlags { get; set; } = new();

    [JsonPropertyName("relay_url")]
    public string? RelayUrl { get; set; }

    [JsonPropertyName("direct_addrs")]
    public List<string> DirectAddrs { get; set; } = new();

    [JsonPropertyName("load")]
    public float? Load { get; set; }

    [JsonPropertyName("language_runtime")]
    public string? LanguageRuntime { get; set; }

    [JsonPropertyName("aster_version")]
    public string AsterVersion { get; set; } = "";

    [JsonPropertyName("policy_realm")]
    public string? PolicyRealm { get; set; }

    [JsonPropertyName("health_status")]
    public string HealthStatusValue { get; set; } = HealthStatus.Starting;

    [JsonPropertyName("tags")]
    public List<string> Tags { get; set; } = new();

    [JsonPropertyName("updated_at_epoch_ms")]
    public long UpdatedAtEpochMs { get; set; }

    public bool IsFresh(int leaseDurationS = 45)
    {
        long nowMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        return (nowMs - UpdatedAtEpochMs) <= leaseDurationS * 1000L;
    }

    public bool IsRoutable() => HealthStatus.IsRoutable(HealthStatusValue);

    public string ToJson() => JsonSerializer.Serialize(this, RegistryJson.Options);

    public static EndpointLease FromJson(string json) =>
        JsonSerializer.Deserialize<EndpointLease>(json, RegistryJson.Options)
            ?? throw new InvalidOperationException("null EndpointLease");
}

/// <summary>Flat change notification broadcast over gossip (Aster-SPEC.md §11.7).</summary>
public sealed class GossipEvent
{
    [JsonPropertyName("type")]
    public GossipEventType Type { get; set; }

    [JsonPropertyName("service")]
    public string? Service { get; set; }

    [JsonPropertyName("version")]
    public int? Version { get; set; }

    [JsonPropertyName("channel")]
    public string? Channel { get; set; }

    [JsonPropertyName("contract_id")]
    public string? ContractId { get; set; }

    [JsonPropertyName("endpoint_id")]
    public string? EndpointId { get; set; }

    [JsonPropertyName("key_prefix")]
    public string? KeyPrefix { get; set; }

    [JsonPropertyName("timestamp_ms")]
    public long TimestampMs { get; set; }

    public string ToJson() => JsonSerializer.Serialize(this, RegistryJson.Options);

    public static GossipEvent FromJson(string json) =>
        JsonSerializer.Deserialize<GossipEvent>(json, RegistryJson.Options)
            ?? throw new InvalidOperationException("null GossipEvent");
}

internal static class RegistryJson
{
    internal static readonly JsonSerializerOptions Options = new()
    {
        PropertyNamingPolicy = null,
        DefaultIgnoreCondition = JsonIgnoreCondition.Never,
        WriteIndented = false,
    };
}
