using System;
using System.Collections.Generic;
using System.Linq;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Aster;

// Gateway identity (C ABI 7.7; design docs/_internal/web_gateway/gateway-identity.md): an edge holding
// GatewayIdentity.GatewayRole calls a service on behalf of a user with one JWT per call, either the
// identity provider's own token (forwarded) or one the edge mints (Enrollment.MintUserToken). The server
// verifies it before Gate 3 and the handler sees the user (ServerCallContext.Principal = iss|sub), the
// edge (ServerCallContext.Actor) and how the user signed in (ServerCallContext.AuthMethods). Everything
// is configured through AsterConfig.Identity / AsterConfig.RemoteConfig, as the Rust AsterServer /
// AsterClient are; the binding validates and wires nothing itself.

/// <summary>Roles, metadata and sign-in methods of gateway identity (Rust <c>aster::rpc::identity</c>).</summary>
public static class GatewayIdentity
{
    /// <summary>The call-metadata entry that carries the user's JWT (<see cref="CallOptions.OnBehalfOf"/>).</summary>
    public const string OnBehalfOfMetadata = "aster-on-behalf-of";
    /// <summary>Lets an edge forward users' identities.</summary>
    public const string GatewayRole = "aster.gateway";
    /// <summary>Required on every <c>aster.ops.NodeConfig</c> method.</summary>
    public const string ConfigAdminRole = "aster.config-admin";
    /// <summary>Lets a node write issuer keys into the fleet namespace.</summary>
    public const string IdentityPublisherRole = "aster.identity-publisher";
    /// <summary>The credential attribute naming the roles an edge may put in tokens it mints (comma-separated).</summary>
    public const string IssuableRolesAttribute = "aster.identity.issuable_roles";
    /// <summary>Sign-in with a passkey (<c>amr</c>).</summary>
    public const string AmrWebAuthn = "webauthn";
    /// <summary>Sign-in with a password (<c>amr</c>).</summary>
    public const string AmrPassword = "pwd";
    /// <summary>Sign-in with an API key (<c>amr</c>).</summary>
    public const string AmrApiKey = "apikey";

    /// <summary>
    /// The token of an <c>Authorization: Bearer &lt;token&gt;</c> header value, for forwarding a provider's
    /// token unchanged (Rust <c>aster::rpc::bearer_token</c>); null when it is not a bearer token.
    /// </summary>
    public static string? BearerToken(string? authorization)
    {
        var value = authorization?.Trim();
        if (string.IsNullOrEmpty(value)) return null;
        int space = value.IndexOf(' ');
        if (space < 0 || !value[..space].Equals("bearer", StringComparison.OrdinalIgnoreCase)) return null;
        var token = value[(space + 1)..].Trim();
        return token.Length == 0 ? null : token;
    }

    internal static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };
}

// ─── Configuration ([identity], [identity.publisher], [remote_config]) ────────

/// <summary>
/// An identity provider whose tokens the server accepts in <see cref="GatewayIdentity.OnBehalfOfMetadata"/>
/// (<c>[[identity.trusted_issuers]]</c>; Rust <c>aster::TrustedIssuer</c>). Its keys come from the fleet
/// namespace, published by a node holding <see cref="GatewayIdentity.IdentityPublisherRole"/>; this says whom
/// to trust and how to map roles. Needs Enrollment.
/// </summary>
public sealed record TrustedIssuer
{
    /// <summary>The exact <c>iss</c> claim.</summary>
    public string Iss { get; init; } = "";
    /// <summary>The accepted <c>aud</c> values (required).</summary>
    public IReadOnlyList<string> Audiences { get; init; } = Array.Empty<string>();
    /// <summary>The claim roles are read from (an array of strings, or one space-separated string); null = the tokens grant no roles.</summary>
    public string? RolesClaim { get; init; }
    /// <summary>Maps claim values to Aster roles; unmapped values are dropped.</summary>
    public IReadOnlyDictionary<string, string>? RoleMap { get; init; }
    /// <summary>The longest <c>exp - iat</c> accepted; null = 3600.</summary>
    public ulong? MaxTokenLifetimeSecs { get; init; }
}

/// <summary>
/// <c>[identity.publisher]</c> (Rust <c>aster::IdentityPublisherConfig</c>): on the node that publishes issuer
/// keys, normally the gateway. It fetches each issuer's JWK set over validated HTTPS and writes it into the
/// fleet namespace. Needs Enrollment, a credential granting <see cref="GatewayIdentity.IdentityPublisherRole"/>
/// and the root's sealed write grant (<c>aster enroll identity-publisher</c>). Env <c>ASTER_IDENTITY_PUBLISHER</c> (JSON).
/// </summary>
public sealed record IdentityPublisherConfig
{
    public IReadOnlyList<string> Issuers { get; init; } = Array.Empty<string>();
    /// <summary>Each issuer's <c>https</c> JWK set URL.</summary>
    [JsonPropertyName("jwks_url")]
    public IReadOnlyDictionary<string, string>? JwksUrl { get; init; }
    /// <summary>The scheduled refresh (60..86400); null = 3600.</summary>
    public ulong? RefreshSecs { get; init; }
    /// <summary>Bounds unknown-kid refreshes per issuer; null = 60.</summary>
    public ulong? MinRefreshSecs { get; init; }
    /// <summary>Extra PEM trust anchors for the providers' TLS.</summary>
    public IReadOnlyList<string>? CaCerts { get; init; }
    /// <summary>Trust only <see cref="CaCerts"/>.</summary>
    public bool? CaOnly { get; init; }
}

/// <summary>
/// The <c>[identity]</c> section (Rust <c>AsterConfig</c> <c>identity_*</c> fields; env <c>ASTER_IDENTITY_*</c>).
/// <see cref="AsterConfig.FromEnv"/> / <see cref="AsterConfig.FromToml"/> fill it through the Rust config, with
/// the same names, defaults and precedence.
/// </summary>
public sealed record IdentityConfig
{
    /// <summary>
    /// Accept calls made on behalf of a user (default false: a call carrying the entry is refused with
    /// <c>PERMISSION_DENIED</c>). Env <c>ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF</c>; TOML <c>[identity] accept_on_behalf_of</c>.
    /// </summary>
    public bool AcceptOnBehalfOf { get; init; }
    /// <summary>The providers whose tokens may be forwarded. Env <c>ASTER_IDENTITY_TRUSTED_ISSUERS</c> (JSON); TOML <c>[[identity.trusted_issuers]]</c>.</summary>
    public IReadOnlyList<TrustedIssuer> TrustedIssuers { get; init; } = Array.Empty<TrustedIssuer>();
    /// <summary>Longest edge-minted token lifetime (1..300); null = 300.</summary>
    public ulong? EdgeTokenMaxLifetimeSecs { get; init; }
    /// <summary>Clock leeway applied to <c>iat</c> / <c>nbf</c> (0..300); null = 60.</summary>
    public ulong? ClockLeewaySecs { get; init; }
    /// <summary>Oldest issuer key set accepted (60..2592000); null = 172800.</summary>
    public ulong? MaxKeySetAgeSecs { get; init; }
    /// <summary><c>[identity.publisher]</c> (null = no publisher).</summary>
    public IdentityPublisherConfig? Publisher { get; init; }

    // Where each overridable setting came from (aster_config_source_t) and its value then, so
    // aster.ops.NodeConfig get reports file / env faithfully; a value changed in code since reports "code".
    internal byte[] Sources { get; init; } = new byte[SettingCount];
    internal string[]? Loaded { get; init; }

    internal const int SettingCount = 5;

    /// <summary>The JSON of each overridable setting, in <c>ASTER_IDENTITY_SETTING_*</c> order.</summary>
    internal string[] Fingerprints()
    {
        static string U(ulong? v) => v is { } x ? x.ToString(System.Globalization.CultureInfo.InvariantCulture) : "null";
        string issuers = TrustedIssuers.Count == 0 ? "null" : JsonSerializer.Serialize(TrustedIssuers, GatewayIdentity.Json);
        return [AcceptOnBehalfOf ? "true" : "false", issuers, U(EdgeTokenMaxLifetimeSecs), U(ClockLeewaySecs), U(MaxKeySetAgeSecs)];
    }
}

/// <summary>
/// <c>[remote_config]</c>: <c>aster.ops.NodeConfig</c>, remote overrides of overridable sections
/// (<c>[identity]</c> today). Served by default behind <see cref="GatewayIdentity.ConfigAdminRole"/>; a node
/// without Enrollment refuses every change.
/// </summary>
public sealed record RemoteConfig
{
    /// <summary>Turns the service off (<c>[remote_config] enabled = false</c>); a stored record is then ignored. Env <c>ASTER_REMOTE_CONFIG_ENABLED</c>.</summary>
    public bool Disabled { get; init; }
    /// <summary>Sections that may not be overridden, e.g. <c>identity</c>. Env <c>ASTER_REMOTE_CONFIG_LOCK</c> (comma-separated).</summary>
    public IReadOnlyList<string> Lock { get; init; } = Array.Empty<string>();
}

// ─── Handlers: the forwarded identity and NodeConfig properties ───────────────

/// <summary>The verified user of a call a gateway made on its behalf (Rust <c>aster::rpc::ForwardedIdentity</c>).</summary>
public sealed record ForwardedIdentity
{
    /// <summary>The token's <c>iss</c>: a provider's URL, or the gateway's node id for a token it minted.</summary>
    public string Issuer { get; init; } = "";
    public string Subject { get; init; } = "";
    /// <summary>The token id (empty when it has none).</summary>
    public string Jti { get; init; } = "";
    public DateTimeOffset ExpiresAt { get; init; }
    /// <summary>The token's <c>amr</c> (how the user signed in).</summary>
    public IReadOnlyList<string> AuthMethods { get; init; } = Array.Empty<string>();

    /// <summary>Parses the <c>aster_dispatch_on_behalf_of</c> k/v list; null when empty.</summary>
    internal static ForwardedIdentity? FromKv(IReadOnlyList<KeyValuePair<string, string>> kv)
    {
        if (kv.Count == 0) return null;
        string iss = "", sub = "", jti = "";
        DateTimeOffset exp = default;
        var amr = new List<string>();
        foreach (var (k, v) in kv)
        {
            switch (k)
            {
                case "iss": iss = v; break;
                case "sub": sub = v; break;
                case "jti": jti = v; break;
                case "exp":
                    if (long.TryParse(v, System.Globalization.NumberStyles.Integer, System.Globalization.CultureInfo.InvariantCulture, out long s))
                        exp = DateTimeOffset.FromUnixTimeSeconds(s);
                    break;
                case "amr": amr.Add(v); break;
            }
        }
        return new ForwardedIdentity { Issuer = iss, Subject = sub, Jti = jti, ExpiresAt = exp, AuthMethods = amr };
    }
}

/// <summary>
/// A typed <c>aster.ops.NodeConfig</c> attribute (Rust <c>aster::rpc::AttrValue</c>): <see cref="Type"/> is
/// <c>bool</c>, <c>int</c>, <c>float</c> or <c>string</c>. Attributes describe the node and never authorise anything.
/// </summary>
public sealed record NodeAttribute
{
    public string Type { get; init; } = "";
    public JsonElement Value { get; init; }

    public static NodeAttribute Bool(bool v) => Of("bool", v);
    public static NodeAttribute Int(long v) => Of("int", v);
    public static NodeAttribute Float(double v) => Of("float", v);
    public static NodeAttribute String(string v) => Of("string", v);

    private static NodeAttribute Of<T>(string type, T v) => new() { Type = type, Value = JsonSerializer.SerializeToElement(v) };

    /// <summary>The value of a <c>bool</c> attribute, else null.</summary>
    public bool? AsBool() => Type == "bool" && Value.ValueKind is JsonValueKind.True or JsonValueKind.False ? Value.GetBoolean() : null;
    /// <summary>The value of an <c>int</c> attribute, else null.</summary>
    public long? AsInt() => Type == "int" && Value.ValueKind == JsonValueKind.Number && Value.TryGetInt64(out long v) ? v : null;
    /// <summary>The value of a <c>float</c> attribute, else null.</summary>
    public double? AsFloat() => Type == "float" && Value.ValueKind == JsonValueKind.Number ? Value.GetDouble() : null;
    /// <summary>The value of a <c>string</c> attribute, else null.</summary>
    public string? AsString() => Type == "string" && Value.ValueKind == JsonValueKind.String ? Value.GetString() : null;
}

/// <summary>The node's <c>aster.ops.NodeConfig</c> properties as handlers see them now (Rust <c>aster::rpc::NodeProperties</c>).</summary>
public sealed record NodeProperties
{
    /// <summary>The NodeConfig generation in effect (0: never changed, or NodeConfig off).</summary>
    public ulong Generation { get; init; }
    public string Name { get; init; } = "";
    public IReadOnlyList<string> Tags { get; init; } = Array.Empty<string>();
    public IReadOnlyDictionary<string, NodeAttribute> Attributes { get; init; } = new Dictionary<string, NodeAttribute>();

    /// <summary>Whether the node carries <paramref name="tag"/>.</summary>
    public bool HasTag(string tag) => Tags.Contains(tag);

    internal static readonly NodeProperties Empty = new();
}

/// <summary>What the issuer-key publisher last did for one issuer (<see cref="Enrollment.PublisherStatus"/>).</summary>
public sealed record IssuerPublishStatus
{
    public IReadOnlyList<string> Kids { get; init; } = Array.Empty<string>();
    /// <summary>Unix seconds of the last fetch, or null.</summary>
    public ulong? FetchedAt { get; init; }
    public ulong Publications { get; init; }
    public ulong Attempts { get; init; }
    public string? LastError { get; init; }
}
