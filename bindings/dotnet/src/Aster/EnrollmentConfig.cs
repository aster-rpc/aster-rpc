using System;
using System.Collections.Generic;
using System.Linq;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;

namespace Aster;

// Enrollment configuration (design docs/_internal/working_ideas/aster-enrollment.md §15.3):
// the [enrollment] section of aster.toml and the ASTER_ENROLLMENT_* variables, resolved by the
// Rust aster::AsterConfig through the C ABI (aster_config_enrollment_from_env / _from_toml), so
// names, defaults and precedence (defaults < TOML < environment < code) are the Rust SDK's.
// The JSON names below are the C ABI trust JSON's (snake_case of the property names).

/// <summary>
/// The <c>[enrollment]</c> section (Rust <c>aster::EnrollmentConfig</c>; env
/// <c>ASTER_ENROLLMENT_*</c>). The root and this node's credential live in
/// <see cref="AsterConfig"/>'s trust properties. Once enabled
/// (<see cref="AsterConfig.IsEnrollmentEnabled"/>) every value is validated when the server or
/// client starts, and any error stops the start: enrollment never degrades to an open node.
/// </summary>
public sealed record EnrollmentConfig
{
    /// <summary>
    /// Forces enrollment on or off; null enables it exactly when <see cref="Audience"/>,
    /// <see cref="FleetNamespace"/>, <see cref="IssuerDelegationFile"/> or a credential is set.
    /// TOML <c>enabled</c>; env <c>ASTER_ENROLLMENT_ENABLED</c>.
    /// </summary>
    public bool? Enabled { get; init; }

    /// <summary>The fleet audience credentials must name. Required. TOML <c>audience</c>; env <c>ASTER_ENROLLMENT_AUDIENCE</c>.</summary>
    public string? Audience { get; init; }

    /// <summary>
    /// The fleet policy namespace (64 hex, <c>aster enroll namespace create</c>). Required. TOML
    /// <c>fleet_namespace</c>; env <c>ASTER_ENROLLMENT_FLEET_NAMESPACE</c>.
    /// </summary>
    public string? FleetNamespace { get; init; }

    /// <summary>
    /// Durable denials and watermarks. Default <c>&lt;StoragePath&gt;/enrollment.json</c>; a server
    /// with neither refuses to start, a client uses a temporary directory. TOML <c>state_path</c>;
    /// env <c>ASTER_ENROLLMENT_STATE_PATH</c>.
    /// </summary>
    public string? StatePath { get; init; }

    /// <summary>
    /// Policy replicas (<c>aster1…</c> tickets or 64-hex node ids) a server enrolls with and
    /// synchronizes the fleet namespace from at start. Empty: the server holds the fleet namespace
    /// and the root publishes into it (<c>aster enroll … --peer &lt;server&gt;</c>). TOML
    /// <c>replicas</c>; env <c>ASTER_ENROLLMENT_REPLICAS</c> (comma-separated).
    /// </summary>
    public IReadOnlyList<string> Replicas { get; init; } = Array.Empty<string>();

    /// <summary>
    /// The admission policy; null = <see cref="EnrollmentPolicy.TolerateStalePolicy"/>: any valid
    /// credential from the root for the audience, minus every known revocation. TOML
    /// <c>policy</c> (inline table or string); env <c>ASTER_ENROLLMENT_POLICY</c> (JSON).
    /// </summary>
    public EnrollmentPolicy? Policy { get; init; }

    /// <summary>Refuse to start on an unsynchronized clock (Linux). TOML <c>require_synchronized_clock</c>.</summary>
    public bool RequireSynchronizedClock { get; init; }

    /// <summary><c>iat</c>/<c>nbf</c> leeway, 0–300 seconds; null = 60. TOML <c>activation_leeway_secs</c>.</summary>
    public ulong? ActivationLeewaySecs { get; init; }

    /// <summary>Enrollment limits (<c>[enrollment.limits]</c>).</summary>
    public EnrollmentLimits Limits { get; init; } = new();

    /// <summary>Claims → Gate-3 roles (<c>[enrollment.roles]</c>).</summary>
    public RoleMapping Roles { get; init; } = new();

    /// <summary>
    /// Server only: run a delegated issuer serving <c>aster.enrollment.IssuerV1</c> from this
    /// delegation chain (<c>aster enroll delegate --out</c>). A client with it set is refused. TOML
    /// <c>[enrollment.issuer] delegation_file</c>; env <c>ASTER_ENROLLMENT_ISSUER_DELEGATION_FILE</c>.
    /// </summary>
    public string? IssuerDelegationFile { get; init; }

    /// <summary>
    /// Lifetime of issued credentials, shortened to the delegation; null = 24 h. TOML
    /// <c>[enrollment.issuer] lifetime</c> (seconds or <c>"24h"</c>); env <c>ASTER_ENROLLMENT_ISSUER_LIFETIME</c>.
    /// </summary>
    public ulong? IssuerLifetimeSecs { get; init; }

    /// <summary>The issuer to renew this node's credential from (ticket or node id). TOML <c>renew_from</c>; env <c>ASTER_ENROLLMENT_RENEW_FROM</c>.</summary>
    public string? RenewFrom { get; init; }

    /// <summary>
    /// <c>false</c> disables renewal; null renews from <see cref="RenewFrom"/>, or a client from the
    /// server that signed its credential. TOML <c>renew</c>; env <c>ASTER_ENROLLMENT_RENEW</c>.
    /// </summary>
    public bool? Renew { get; init; }

    /// <summary>
    /// Client: synchronize the fleet namespace the server offers (null = true), so the client also
    /// learns revocations of the server. TOML <c>synchronize</c>; env <c>ASTER_ENROLLMENT_SYNCHRONIZE</c>.
    /// </summary>
    public bool? Synchronize { get; init; }
}

/// <summary>
/// <c>[enrollment.limits]</c> overrides (env <c>ASTER_ENROLLMENT_&lt;FIELD&gt;</c>, e.g.
/// <c>ASTER_ENROLLMENT_HANDSHAKE_TIMEOUT_MS</c>); null keeps Enrollment's default.
/// </summary>
public sealed record EnrollmentLimits
{
    public ulong? HandshakeTimeoutMs { get; init; }
    public ulong? StepTimeoutMs { get; init; }
    public ulong? PolicyTimeoutMs { get; init; }
    public ulong? ReconciliationIntervalMs { get; init; }
    public ulong? MaxAdmittedPeers { get; init; }
    /// <summary>Bounds enrolling with and synchronizing from a replica or server (default 15 000 ms).</summary>
    public ulong? SyncTimeoutMs { get; init; }
}

/// <summary>
/// Turns a peer's verified claims into the attributes Gate 3 checks
/// <see cref="Rpc.CapabilityRequirement"/>s against (<c>[enrollment.roles]</c>; Rust
/// <c>RoleMapping</c>). Roles = the comma-separated values of the string claim attribute
/// <see cref="Attribute"/>, plus every verified scope when <see cref="ScopesAsRoles"/>, plus the
/// roles <see cref="Scopes"/> and <see cref="Tags"/> map each verified scope or tag to; sorted,
/// deduplicated and comma-joined into <c>aster.role</c>. Evaluated in Rust on every call from the
/// peer's live session, so a revocation, promotion or renewal applies on the next call.
/// </summary>
public sealed record RoleMapping
{
    /// <summary>The claim attribute listing roles; null = <c>"aster.role"</c>, <c>""</c> disables it. Env <c>ASTER_ENROLLMENT_ROLE_ATTRIBUTE</c>.</summary>
    public string? Attribute { get; init; }

    /// <summary>Each verified scope is also a role. Env <c>ASTER_ENROLLMENT_SCOPES_AS_ROLES</c>.</summary>
    public bool ScopesAsRoles { get; init; }

    /// <summary>Verified scope → roles (<c>[enrollment.roles.scopes]</c>).</summary>
    public IReadOnlyDictionary<string, IReadOnlyList<string>> Scopes { get; init; } = new Dictionary<string, IReadOnlyList<string>>();

    /// <summary>Verified tag → roles (<c>[enrollment.roles.tags]</c>).</summary>
    public IReadOnlyDictionary<string, IReadOnlyList<string>> Tags { get; init; } = new Dictionary<string, IReadOnlyList<string>>();

    /// <summary>
    /// Pass the other claim attributes to handlers as strings
    /// (<see cref="Rpc.ServerCallContext.Attributes"/>); null = true. A claim named
    /// <c>aster.role</c> is never passed through. Env <c>ASTER_ENROLLMENT_INCLUDE_ATTRIBUTES</c>.
    /// </summary>
    public bool? IncludeAttributes { get; init; }
}

/// <summary>
/// A declarative admission policy, evaluated in Rust (<c>PolicyConfig</c>, design §15.5): the
/// externally tagged JSON tree <c>all_of</c> / <c>any_of</c> / <c>none_of</c>, <c>source_cidr</c>,
/// <c>audience</c>, <c>scope</c>, <c>tag</c>, <c>attribute_equals</c>, <c>issued_by</c>,
/// <c>max_credential_age_secs</c>, <c>policy_synchronized_within</c>, and the bare strings
/// <c>"tolerate_stale_policy"</c> and <c>"require_registration"</c>. Build one with the static
/// constructors, or set it in TOML (<c>[enrollment] policy</c>) or <c>ASTER_ENROLLMENT_POLICY</c>.
/// Rust validates the tree before the node starts; an unknown node is an error.
/// </summary>
[JsonConverter(typeof(EnrollmentPolicyConverter))]
public sealed class EnrollmentPolicy
{
    private readonly JsonNode _node;

    private EnrollmentPolicy(JsonNode node) => _node = node;

    /// <summary>A policy from its <c>PolicyConfig</c> JSON (kept as is; Rust validates it).</summary>
    public static EnrollmentPolicy FromJson(string json)
        => new(JsonNode.Parse(json) ?? throw new ArgumentException("policy JSON is null", nameof(json)));

    /// <summary>The <c>PolicyConfig</c> JSON.</summary>
    public string ToJson() => _node.ToJsonString();

    public override string ToString() => ToJson();

    internal JsonNode Node => _node.DeepClone();

    private static EnrollmentPolicy Tagged(string tag, JsonNode? value) => new(new JsonObject { [tag] = value });

    private static JsonArray List(EnrollmentPolicy[] children)
        => new(children.Select(c => (JsonNode?)c.Node).ToArray());

    /// <summary>Matches when every child matches.</summary>
    public static EnrollmentPolicy AllOf(params EnrollmentPolicy[] children) => Tagged("all_of", List(children));

    /// <summary>Matches when any child matches.</summary>
    public static EnrollmentPolicy AnyOf(params EnrollmentPolicy[] children) => Tagged("any_of", List(children));

    /// <summary>Matches when no child matches.</summary>
    public static EnrollmentPolicy NoneOf(params EnrollmentPolicy[] children) => Tagged("none_of", List(children));

    /// <summary>Requires the scope among the evidence's scopes.</summary>
    public static EnrollmentPolicy Scope(string scope) => Tagged("scope", scope);

    /// <summary>Requires the tag among the evidence's tags.</summary>
    public static EnrollmentPolicy Tag(string tag) => Tagged("tag", tag);

    /// <summary>Requires the audience among the evidence's audiences.</summary>
    public static EnrollmentPolicy Audience(string audience) => Tagged("audience", audience);

    /// <summary>Requires a direct path from inside the network (e.g. <c>"10.0.0.0/8"</c>).</summary>
    public static EnrollmentPolicy SourceCidr(string cidr) => Tagged("source_cidr", cidr);

    /// <summary>Requires a typed claim attribute value.</summary>
    public static EnrollmentPolicy AttributeEquals(string key, ClaimAttribute value)
        => Tagged("attribute_equals", new JsonObject { ["key"] = key, ["value"] = value.ToNode() });

    /// <summary>Requires the key (64 hex) on the presented chain's issuer path.</summary>
    public static EnrollmentPolicy IssuedBy(string key) => Tagged("issued_by", key);

    /// <summary>Requires evidence issued at most <paramref name="maxAge"/> ago.</summary>
    public static EnrollmentPolicy MaxCredentialAge(TimeSpan maxAge) => Tagged("max_credential_age_secs", (ulong)maxAge.TotalSeconds);

    /// <summary>Requires a policy synchronization at most <paramref name="maxAge"/> old (from the root itself when <paramref name="requireRoot"/>).</summary>
    public static EnrollmentPolicy PolicySynchronizedWithin(TimeSpan maxAge, bool requireRoot)
        => Tagged("policy_synchronized_within", new JsonObject { ["max_age_secs"] = (ulong)maxAge.TotalSeconds, ["require_root"] = requireRoot });

    /// <summary>Explicitly accepts cached/offline policy (known revocations still deny). The default.</summary>
    public static EnrollmentPolicy TolerateStalePolicy() => new(JsonValue.Create("tolerate_stale_policy"));

    /// <summary>Admits only peers the root registered (a presented chain alone does not match).</summary>
    public static EnrollmentPolicy RequireRegistration() => new(JsonValue.Create("require_registration"));

    internal sealed class EnrollmentPolicyConverter : JsonConverter<EnrollmentPolicy>
    {
        public override EnrollmentPolicy? Read(ref Utf8JsonReader reader, Type typeToConvert, JsonSerializerOptions options)
            => JsonNode.Parse(ref reader) is { } node ? new EnrollmentPolicy(node) : null;

        public override void Write(Utf8JsonWriter writer, EnrollmentPolicy value, JsonSerializerOptions options)
            => value._node.WriteTo(writer);
    }
}

/// <summary>A typed claim value: <see cref="Type"/> is <c>"bool"</c>, <c>"int"</c>, <c>"float"</c> or <c>"string"</c>.</summary>
public sealed record ClaimAttribute
{
    public string Type { get; init; } = "string";
    public JsonElement Value { get; init; }

    public static ClaimAttribute String(string value) => new() { Type = "string", Value = JsonSerializer.SerializeToElement(value) };
    public static ClaimAttribute Int(long value) => new() { Type = "int", Value = JsonSerializer.SerializeToElement(value) };
    public static ClaimAttribute Bool(bool value) => new() { Type = "bool", Value = JsonSerializer.SerializeToElement(value) };
    public static ClaimAttribute Float(double value) => new() { Type = "float", Value = JsonSerializer.SerializeToElement(value) };

    internal JsonNode ToNode() => new JsonObject { ["type"] = Type, ["value"] = JsonNode.Parse(Value.GetRawText()) };

    /// <summary>The value in its display form (strings unquoted).</summary>
    public override string ToString() => Value.ValueKind == JsonValueKind.String ? Value.GetString() ?? "" : Value.GetRawText();
}

/// <summary>The <c>[trust]</c> + <c>[enrollment]</c> JSON of the C ABI (field names = Rust config names).</summary>
internal sealed class TrustJson
{
    public string? RootPubkey { get; set; }
    public string? RootPubkeyFile { get; set; }
    public string? NextRootCommitment { get; set; }
    public ulong RootSeq { get; set; }
    public List<string> RootRotations { get; set; } = new();
    public string? EnrollmentCredentialFile { get; set; }
    public string? EnrollmentCredentialChain { get; set; }
    public EnrollmentConfig Enrollment { get; set; } = new();

    internal static readonly JsonSerializerOptions Options = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    public static TrustJson Parse(byte[] json)
        => JsonSerializer.Deserialize<TrustJson>(json, Options) ?? throw new IrohException(Status.Internal, "empty trust JSON");

    public byte[] Encode() => JsonSerializer.SerializeToUtf8Bytes(this, Options);
}
