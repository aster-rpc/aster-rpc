using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using Apache.Fory;
using Aster.Crypto;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster.Tests;

[ForyStruct]
[WireType("test.gateway/WhoRequest")]
public sealed class WhoRequest
{
    [WireName("n")] public int N { get; set; }
}

[ForyStruct]
[WireType("test.gateway/WhoReply")]
public sealed class WhoReply
{
    [WireName("principal")] public string Principal { get; set; } = "";
    [WireName("peer")] public string Peer { get; set; } = "";
    [WireName("actor")] public string Actor { get; set; } = "";
    [WireName("amr")] public string Amr { get; set; } = "";
    [WireName("roles")] public string Roles { get; set; } = "";
    [WireName("issuer")] public string Issuer { get; set; } = "";
    [WireName("tier")] public long Tier { get; set; }
    [WireName("name")] public string Name { get; set; } = "";
}

/// <summary>
/// Gateway identity (C ABI 7.7) on the high-level path only: <see cref="AsterServer"/> and <see cref="AsterClient"/>
/// configured by <see cref="AsterConfig.Identity"/> / <see cref="AsterConfig.RemoteConfig"/> and the credentials,
/// mirroring <c>aster/tests/enrollment_gateway_identity.rs</c>, <c>enrollment_provider_tokens.rs</c>,
/// <c>node_config.rs</c> and Go's <c>identity_test.go</c>. The root is <see cref="TestRoot"/>.
/// </summary>
public class GatewayIdentityTests
{
    private const string Fleet = "mission-control";
    private static readonly TimeSpan Budget = TimeSpan.FromSeconds(30);

    private static byte[] Seed(byte b) => Enumerable.Repeat(b, 32).ToArray();
    private static string Pub(byte b) => Ids.NodeId(Seed(b));

    private static readonly TestRoot Root = new(Seed(1), Fleet);
    private static readonly byte[] FleetSecret = Enumerable.Repeat((byte)0x55, 32).ToArray();
    private static readonly string FleetNamespace = Convert.ToHexString(Ids.NamespaceId(FleetSecret)).ToLowerInvariant();
    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    /// <summary>Roles in <c>aster.role</c> and, when given, an issuer delegation (<c>aster.identity.issuable_roles</c>).</summary>
    private static string CredentialFor(byte seed, string roles, string? issuable = null)
    {
        var claims = TestRoot.RoleClaims(roles);
        if (issuable is not null)
            ((Dictionary<string, object?>)claims["attributes"]!)[GatewayIdentity.IssuableRolesAttribute] =
                new Dictionary<string, object?> { ["type"] = "string", ["value"] = issuable };
        return Root.Credential(Pub(seed), claims, TimeSpan.FromHours(1));
    }

    private static AsterConfig Enrolled(byte seed, string credential) => Loopback with
    {
        SecretKey = Seed(seed),
        RootPubkey = Convert.FromHexString(Root.PublicKey),
        EnrollmentCredentialChain = credential,
        Enrollment = new EnrollmentConfig { Audience = Fleet, FleetNamespace = FleetNamespace },
    };

    /// <summary>The server (seed 2), accepting calls on behalf of users.</summary>
    private static AsterConfig ServerConfig()
    {
        string dir = Path.Combine(Path.GetTempPath(), $"aster-gateway-{Guid.NewGuid():N}");
        Directory.CreateDirectory(dir);
        var c = Enrolled(2, CredentialFor(2, ""));
        return c with
        {
            Enrollment = c.Enrollment with
            {
                StatePath = Path.Combine(dir, "server-enrollment.json"),
                Limits = new EnrollmentLimits { ReconciliationIntervalMs = 200 },
            },
            Identity = new IdentityConfig { AcceptOnBehalfOf = true },
        };
    }

    /// <summary>What the handler sees, read through <see cref="ServerCallContext.Current"/>.</summary>
    private static WhoReply Who()
    {
        var ctx = ServerCallContext.Require();
        return new WhoReply
        {
            Principal = ctx.Principal,
            Peer = ctx.Peer,
            Actor = ctx.Actor ?? "",
            Amr = string.Join(",", ctx.AuthMethods),
            Roles = ctx.Attributes.TryGetValue("aster.role", out var r) ? r : "",
            Issuer = ctx.OnBehalfOf?.Issuer ?? "",
            Tier = ctx.NodeAttribute("tier")?.AsInt() ?? 0,
            Name = ctx.NodeProperties.Name,
        };
    }

    /// <summary>whoami is open, read needs notes.read, publish needs notes.read and a passkey sign-in.</summary>
    private static ServiceDefinition Notes() => ServiceDefinition.Create("Notes", 1)
        .Unary<WhoRequest, WhoReply>("whoami", (_, _) => ValueTask.FromResult(Who()))
        .Unary<WhoRequest, WhoReply>("read", (_, _) => ValueTask.FromResult(Who()), requires: CapabilityRequirement.Role("notes.read"))
        .Unary<WhoRequest, WhoReply>("publish", (_, _) =>
        {
            ServerCallContext.Require().RequireAuthMethod(GatewayIdentity.AmrWebAuthn);
            return ValueTask.FromResult(Who());
        }, requires: CapabilityRequirement.Role("notes.read"))
        .Build();

    private static async Task<(AsterServer, NodeAddrInfo)> StartNotes(AsterConfig config, bool? acceptOnBehalfOf = null)
    {
        var server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = config,
            AcceptOnBehalfOf = acceptOnBehalfOf,
            Services = { Notes() },
        }).WaitAsync(Budget);
        return (server, (await server.TicketAsync()).ToNodeAddr());
    }

    private static async Task<AsterClient> Client(AsterConfig config)
    {
        var client = await AsterClient.StartAsync(config).WaitAsync(Budget);
        client.RegisterTypes(Notes());
        return client;
    }

    private static Task<WhoReply> Call(RpcConnection rc, string method, string? token = null)
        => rc.UnaryAsync<WhoRequest, WhoReply>("Notes", method, new WhoRequest { N = 1 },
            new CallOptions { Deadline = TimeSpan.FromSeconds(20), OnBehalfOf = token }).WaitAsync(Budget);

    private static async Task WantCode(StatusCode code, Func<Task> call)
    {
        var e = await Assert.ThrowsAsync<RpcError>(call);
        Assert.Equal(code, e.Code);
    }

    [Fact]
    public void Bearer_Token_And_Metadata_Entry()
    {
        Assert.Equal("abc.def", GatewayIdentity.BearerToken("Bearer abc.def"));
        Assert.Equal("abc", GatewayIdentity.BearerToken("  bearer   abc "));
        Assert.Null(GatewayIdentity.BearerToken("Basic x"));
        Assert.Null(GatewayIdentity.BearerToken("Bearer "));
        var md = new CallOptions { Metadata = [new("Aster-On-Behalf-Of", "old"), new("k", "v")], OnBehalfOf = "new" }.EffectiveMetadata()!;
        Assert.Equal([new("k", "v"), new(GatewayIdentity.OnBehalfOfMetadata, "new")], md);
    }

    // [identity] / [remote_config] from the environment and TOML, through the Rust loaders, with
    // the file / env sources kept across the native round trip unless code changed the value.
    [Fact]
    public void Identity_Config_Loads_From_Env_And_Toml_And_Round_Trips()
    {
        var d = AsterConfig.Default();
        Assert.False(d.Identity.AcceptOnBehalfOf);
        Assert.Empty(d.Identity.TrustedIssuers);
        Assert.Null(d.Identity.Publisher);
        Assert.False(d.RemoteConfig.Disabled);

        string[] keys = ["ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF", "ASTER_IDENTITY_TRUSTED_ISSUERS", "ASTER_IDENTITY_CLOCK_LEEWAY_SECS",
            "ASTER_IDENTITY_PUBLISHER", "ASTER_REMOTE_CONFIG_LOCK"];
        lock (Abi7Tests.EnvLock)
        {
            // Rust reads the native environment; on Unix Environment.SetEnvironmentVariable does not reach getenv.
            if (!OperatingSystem.IsWindows())
            try
            {
                Abi7Tests.Env("ASTER_IDENTITY_ACCEPT_ON_BEHALF_OF", "true");
                Abi7Tests.Env("ASTER_IDENTITY_TRUSTED_ISSUERS",
                    """[{"iss":"https://idp.test","audiences":["notes"],"roles_claim":"groups","role_map":{"readers":"notes.read"}}]""");
                Abi7Tests.Env("ASTER_IDENTITY_CLOCK_LEEWAY_SECS", "0");
                Abi7Tests.Env("ASTER_IDENTITY_PUBLISHER",
                    """{"issuers":["https://idp.test"],"jwks_url":{"https://idp.test":"https://idp.test/jwks"},"refresh_secs":600}""");
                Abi7Tests.Env("ASTER_REMOTE_CONFIG_LOCK", "identity");
                var cfg = AsterConfig.FromEnv();
                var id = cfg.Identity;
                Assert.True(id.AcceptOnBehalfOf);
                var issuer = Assert.Single(id.TrustedIssuers);
                Assert.Equal("https://idp.test", issuer.Iss);
                Assert.Equal(["notes"], issuer.Audiences);
                Assert.Equal("groups", issuer.RolesClaim);
                Assert.Equal("notes.read", issuer.RoleMap!["readers"]);
                Assert.Equal(0UL, id.ClockLeewaySecs);
                Assert.Null(id.EdgeTokenMaxLifetimeSecs);
                Assert.Equal(600UL, id.Publisher!.RefreshSecs);
                Assert.Equal("https://idp.test/jwks", id.Publisher.JwksUrl!["https://idp.test"]);
                Assert.Equal(["identity"], cfg.RemoteConfig.Lock);
                Assert.False(cfg.RemoteConfig.Disabled);
                Assert.Equal((byte)3, id.Sources[0]); // ASTER_CONFIG_SOURCE_ENV
                Assert.Equal((byte)3, id.Sources[1]);
                Assert.Equal((byte)0, id.Sources[2]);

                AsterConfig back;
                using (var scope = new NativeScope())
                {
                    var n = cfg.ToNative(scope);
                    back = AsterConfig.FromNative(in n, new AsterConfig());
                }
                Assert.True(back.Identity.AcceptOnBehalfOf);
                Assert.Equal(0UL, back.Identity.ClockLeewaySecs);
                Assert.Equal(id.Sources, back.Identity.Sources);
                Assert.Equal(["https://idp.test"], back.Identity.Publisher!.Issuers);
                Assert.Equal(["identity"], back.RemoteConfig.Lock);

                // A value changed in code no longer claims to come from the environment.
                var changed = cfg with { Identity = cfg.Identity with { AcceptOnBehalfOf = false } };
                using (var scope = new NativeScope())
                {
                    var n = changed.ToNative(scope);
                    back = AsterConfig.FromNative(in n, new AsterConfig());
                }
                Assert.Equal((byte)0, back.Identity.Sources[0]);
                Assert.Equal((byte)3, back.Identity.Sources[1]);
            }
            finally
            {
                foreach (var k in keys) Abi7Tests.unsetenv(k);
            }

            string path = Path.Combine(Path.GetTempPath(), $"aster-identity-{Guid.NewGuid():N}.toml");
            try
            {
                File.WriteAllText(path, "[identity]\naccept_on_behalf_of = true\nedge_token_max_lifetime_secs = 120\n\n" +
                    "[[identity.trusted_issuers]]\niss = \"https://idp.test\"\naudiences = [\"notes\"]\nmax_token_lifetime_secs = 600\n\n" +
                    "[remote_config]\nenabled = false\n");
                var file = AsterConfig.FromToml(path);
                Assert.True(file.Identity.AcceptOnBehalfOf);
                Assert.Equal(120UL, file.Identity.EdgeTokenMaxLifetimeSecs);
                Assert.Equal(600UL, Assert.Single(file.Identity.TrustedIssuers).MaxTokenLifetimeSecs);
                Assert.True(file.RemoteConfig.Disabled);
                Assert.Equal((byte)2, file.Identity.Sources[0]); // ASTER_CONFIG_SOURCE_FILE
                Assert.Equal((byte)2, file.Identity.Sources[2]);
            }
            finally { File.Delete(path); }
        }

        // Nothing set: no library storage is allocated; a malformed value is refused when it is copied in.
        using (var scope = new NativeScope())
            Assert.Equal(IntPtr.Zero, Loopback.ToNative(scope).storage);
        using (var scope = new NativeScope())
            Assert.NotEqual(IntPtr.Zero, (Loopback with { RemoteConfig = new RemoteConfig { Lock = ["identity"] } }).ToNative(scope).storage);
    }

    // An edge holding aster.gateway mints a token for a user; the .NET handler sees the user, the edge
    // as the actor and the passkey sign-in. The edge, holding aster.config-admin, changes the server
    // through NodeConfig: the handler sees the attributes, and turning accept_on_behalf_of off refuses
    // the entry.
    [Fact]
    public async Task Edge_Minted_Token_Reaches_The_Handler_And_NodeConfig_Set_Ack_Clear()
    {
        var (server, addr) = await StartNotes(ServerConfig());
        await using var _s = server;
        await using var edge = await Client(Enrolled(5, CredentialFor(5, GatewayIdentity.GatewayRole + "," + GatewayIdentity.ConfigAdminRole, "notes.read")));
        var rc = await edge.ConnectAsync(addr).WaitAsync(Budget);
        string edgeId = Pub(5);

        var me = await Call(rc, "whoami");
        Assert.Equal((edgeId, "", ""), (me.Principal, me.Actor, me.Amr));
        await WantCode(StatusCode.PermissionDenied, () => Call(rc, "read")); // the edge itself lacks notes.read

        string token = edge.MintUserToken("alice", ["notes.read"], [GatewayIdentity.AmrWebAuthn], TimeSpan.FromMinutes(1));
        var alice = await Call(rc, "publish", token);
        Assert.Equal(edgeId + "|alice", alice.Principal);
        Assert.Equal(edgeId, alice.Peer);
        Assert.Equal(edgeId, alice.Actor);
        Assert.Equal(GatewayIdentity.AmrWebAuthn, alice.Amr);
        Assert.Equal("notes.read", alice.Roles);
        Assert.Equal(edgeId, alice.Issuer);

        string pwd = edge.Enrollment!.MintUserToken("bob", ["notes.read"], [GatewayIdentity.AmrPassword], TimeSpan.FromMinutes(1));
        await WantCode(StatusCode.PermissionDenied, () => Call(rc, "publish", pwd)); // publish needs a passkey
        Assert.Equal("notes.read", (await Call(rc, "read", pwd)).Roles);
        var outside = Assert.Throws<IrohException>(() => edge.MintUserToken("bob", ["notes.admin"], [GatewayIdentity.AmrPassword], TimeSpan.FromMinutes(1)));
        Assert.Equal(Status.InvalidArgument, outside.Status);
        Assert.Equal(Status.NotFound, Assert.Throws<IrohException>(() => edge.Enrollment.NoticeKid("https://idp.test", "k1")).Status);
        Assert.Equal(Status.NotFound, Assert.Throws<IrohException>(() => edge.Enrollment.PublisherStatus()).Status);

        // NodeConfig: get, set, ack, clear from the admin edge.
        var admin = new NodeConfigClient(rc);
        var view = await admin.GetAsync().WaitAsync(Budget);
        Assert.Equal(Pub(2), view.NodeId);
        var setting = view.Setting("identity.accept_on_behalf_of");
        Assert.NotNull(setting);
        Assert.Equal(JsonValueKind.True, setting!.Value.ValueKind);
        Assert.Equal("code", setting.Source);

        var ack = await admin.SetAsync(new NodeConfigSetRequest
        {
            Name = "notes-1",
            Tags = ["eu"],
            Attributes = new Dictionary<string, NodeAttribute> { ["tier"] = NodeAttribute.Int(3) },
            Identity = new IdentityOverride { AcceptOnBehalfOf = false },
        }).WaitAsync(Budget);
        Assert.True(ack.Complete);
        Assert.Equal(1L, ack.Generation);
        me = await Call(rc, "whoami");
        Assert.Equal((3L, "notes-1"), (me.Tier, me.Name));
        var props = server.NodeProperties();
        Assert.Equal(1UL, props.Generation);
        Assert.True(props.HasTag("eu"));
        Assert.Same(props, server.NodeProperties()); // cached per generation
        await WantCode(StatusCode.PermissionDenied, () => Call(rc, "read", token)); // accept_on_behalf_of = false refuses the entry
        Assert.True((await admin.AckAsync(1, TimeSpan.FromSeconds(1)).WaitAsync(Budget)).Complete);
        await WantCode(StatusCode.PermissionDenied, () => admin.SetAsync(new NodeConfigSetRequest
        {
            Sections = new Dictionary<string, JsonElement> { ["network"] = JsonDocument.Parse("""{"bind_port":1}""").RootElement },
        })); // a section that is not overridable
        Assert.True((await admin.ClearAsync(0).WaitAsync(Budget)).Complete);
        alice = await Call(rc, "read", token);
        Assert.Equal((edgeId, 0L), (alice.Actor, alice.Tier));

        // A peer without aster.config-admin is refused by Gate 3, and one without aster.gateway cannot forward.
        await using var plain = await Client(Enrolled(6, CredentialFor(6, "notes.read")));
        var prc = await plain.ConnectAsync(addr).WaitAsync(Budget);
        await WantCode(StatusCode.PermissionDenied, () => new NodeConfigClient(prc).GetAsync());
        await WantCode(StatusCode.PermissionDenied, () => Call(prc, "read", token));
    }

    // AsterServerOptions.AcceptOnBehalfOf overrides the config: off, the entry is refused with
    // PERMISSION_DENIED rather than ignored.
    [Fact]
    public async Task Accept_On_Behalf_Of_Off_Refuses_The_Entry()
    {
        var (server, addr) = await StartNotes(ServerConfig(), acceptOnBehalfOf: false);
        await using var _s = server;
        await using var edge = await Client(Enrolled(5, CredentialFor(5, GatewayIdentity.GatewayRole + ",notes.read", "notes.read")));
        var rc = await edge.ConnectAsync(addr).WaitAsync(Budget);
        string token = edge.MintUserToken("alice", ["notes.read"], [GatewayIdentity.AmrPassword], TimeSpan.FromMinutes(1));
        Assert.Equal("", (await Call(rc, "read")).Actor);
        await WantCode(StatusCode.PermissionDenied, () => Call(rc, "read", token));
    }
}
