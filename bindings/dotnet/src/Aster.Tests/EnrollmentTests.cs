using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Apache.Fory;
using Aster.Crypto;
using Aster.Interceptors;
using Aster.Rpc;

namespace Aster.Tests;

[ForyStruct]
[WireType("test.enrollment/OpsRequest")]
public sealed class OpsRequest
{
    public int N { get; set; }
}

[ForyStruct]
[WireType("test.enrollment/OpsReply")]
public sealed class OpsReply
{
    public string Roles { get; set; } = "";
}

/// <summary>
/// Enrollment through the high-level path only, mirroring
/// <c>aster/tests/enrollment_aster_server.rs</c> (and Go's <c>enrollment_test.go</c>):
/// <see cref="AsterServer"/> and <see cref="AsterClient"/> configured by <see cref="AsterConfig"/>'s
/// trust properties and <c>[enrollment]</c> section. The root is played by <see cref="TestRoot"/>
/// (root operations belong to the CLI); everything the application writes is configuration and
/// <see cref="CapabilityRequirement"/>s.
/// </summary>
public class EnrollmentTests
{
    private const string Fleet = "mission-control";

    private static byte[] Seed(byte b) => Enumerable.Repeat(b, 32).ToArray();
    private static string Pub(byte b) => Ids.NodeId(Seed(b));

    private static readonly TestRoot Root = new(Seed(1), Fleet);
    private static readonly byte[] FleetSecret = Enumerable.Repeat((byte)0x55, 32).ToArray();
    private static readonly string FleetNamespace = Convert.ToHexString(Ids.NamespaceId(FleetSecret)).ToLowerInvariant();

    private static readonly AsterConfig Loopback = new() { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    /// <summary>A root-issued credential for <paramref name="seed"/> carrying <paramref name="roles"/> in <c>aster.role</c>.</summary>
    private static string CredentialFor(byte seed, string roles) => Root.Credential(Pub(seed), TestRoot.RoleClaims(roles), TimeSpan.FromHours(1));

    /// <summary>A loopback node with the root pinned, the credential inline and the fleet's <c>[enrollment]</c> section.</summary>
    private static AsterConfig Enrolled(byte seed, string? credential) => Loopback with
    {
        SecretKey = Seed(seed),
        RootPubkey = Convert.FromHexString(Root.PublicKey),
        EnrollmentCredentialChain = credential,
        Enrollment = new EnrollmentConfig { Audience = Fleet, FleetNamespace = FleetNamespace },
    };

    /// <summary>The server (seed 2): root-issued credential, durable state in <paramref name="dir"/>, fast reconciliation.</summary>
    private static AsterConfig ServerConfig(string dir)
    {
        var c = Enrolled(2, CredentialFor(2, ""));
        return c with
        {
            Enrollment = c.Enrollment with
            {
                StatePath = Path.Combine(dir, "server-enrollment.json"),
                Limits = new EnrollmentLimits { ReconciliationIntervalMs = 200 },
            },
        };
    }

    private static string TempDir()
    {
        string dir = Path.Combine(Path.GetTempPath(), $"aster-enroll-{Guid.NewGuid():N}");
        Directory.CreateDirectory(dir);
        return dir;
    }

    private static ValueTask<OpsReply> Echo(OpsRequest _, ServerCallContext ctx)
        => ValueTask.FromResult(new OpsReply { Roles = ctx.Attributes.TryGetValue("aster.role", out var r) ? r : "" });

    /// <summary>status requires ops.status, logs ops.logs or ops.admin, ping is open; each echoes the caller's roles.</summary>
    private static ServiceDefinition OpsService() => ServiceDefinition.Create("Ops", 1)
        .Unary<OpsRequest, OpsReply>("ping", Echo)
        .Unary<OpsRequest, OpsReply>("status", Echo, requires: CapabilityRequirement.Role("ops.status"))
        .Unary<OpsRequest, OpsReply>("logs", Echo, requires: CapabilityRequirement.AnyOf("ops.logs", "ops.admin"))
        .Build();

    private static async Task<(AsterServer Server, NodeAddrInfo Addr)> StartOps(AsterServerOptions options)
    {
        var server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = options.Config,
            EnrollmentPolicy = options.EnrollmentPolicy,
            EnrollmentRoles = options.EnrollmentRoles,
            Services = { OpsService() },
        });
        return (server, (await server.TicketAsync()).ToNodeAddr());
    }

    private static async Task<AsterClient> Client(AsterConfig config)
    {
        var client = await AsterClient.StartAsync(config);
        client.RegisterTypes(OpsService());
        return client;
    }

    private static async Task<string> Call(AsterClient client, NodeAddrInfo server, string method)
    {
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        var conn = await client.ConnectAsync(server, cts.Token);
        var reply = await conn.UnaryAsync<OpsRequest, OpsReply>("Ops", method, new OpsRequest { N = 1 }, null, cts.Token);
        return reply.Roles;
    }

    // The roles in a client's credential are what Requires checks, with no application code; a
    // credential without the role is refused, and a node with no credential cannot reach the
    // service. A revocation the root publishes refuses the next call and closes the connection.
    [Fact]
    public async Task Credential_Roles_Gate_Requires_And_Root_Revocation_Closes_The_Connection()
    {
        string dir = TempDir();
        // No replicas: the server holds the fleet namespace and the root publishes into it, as
        // `aster enroll revoke --peer <server>` does.
        var (server, addr) = await StartOps(new AsterServerOptions { Config = ServerConfig(dir) });
        await using var _s = server;
        Assert.NotNull(server.Enrollment);
        Assert.Null(server.Gate);
        Assert.Null(server.Issuer);
        var status = server.Enrollment!.Status();
        Assert.Equal("server", status.Side);
        Assert.Equal(Root.PublicKey, status.TrustedRoot);
        Assert.Equal(FleetNamespace, status.FleetNamespace);
        Assert.Equal(Pub(2), status.Credential?.Subject);

        await using var operatorClient = await Client(Enrolled(5, CredentialFor(5, "ops.status, ops.logs")));
        foreach (var method in new[] { "ping", "status", "logs" })
            Assert.Equal("ops.logs,ops.status", await Call(operatorClient, addr, method));
        Assert.False(operatorClient.IsRenewing); // a root-issued credential has no issuer to renew from
        Assert.Equal("client", operatorClient.Enrollment!.Status().Side);

        await using var ingest = await Client(Enrolled(6, CredentialFor(6, "ops.ingest")));
        Assert.Equal("ops.ingest", await Call(ingest, addr, "ping"));
        foreach (var method in new[] { "status", "logs" })
        {
            var e = await Assert.ThrowsAsync<RpcError>(() => Call(ingest, addr, method));
            Assert.Equal(StatusCode.PermissionDenied, e.Code);
        }
        // The server sees the verified claims (audit), and maps them itself.
        Assert.Equal("ops.ingest", server.Enrollment.PeerClaims(Pub(6))?.Attributes?["aster.role"].ToString());

        // No enrollment at all: Gate 0 refuses the connection itself.
        await using (var stranger = await Client(Loopback))
            await Assert.ThrowsAnyAsync<Exception>(() => Call(stranger, addr, "ping"));

        // The root enrolls with the server (a publication session) and delivers a revocation of
        // the operator into the fleet namespace the server holds.
        using var wait = new CancellationTokenSource(TimeSpan.FromSeconds(30));
        var revoked = server.Enrollment.NextRevocationAsync(wait.Token);
        await using var root = await Client(Enrolled(1, CredentialFor(1, "")));
        await root.ConnectAsync(addr);
        using var fleet = await root.Node.Docs().ImportWriteNamespaceAsync(FleetSecret);
        await fleet.SetBytesAsync(Root.PublicKey, TestRoot.RegistrationKey(Pub(5)), Root.Revocation(Pub(5), "decommissioned"));
        await fleet.StartSyncAsync([Pub(2)]);
        var revocation = await revoked;
        Assert.Equal(Pub(5), revocation.Peer);
        Assert.Equal("node_revoked", revocation.Reason?.Code);
        Assert.True(revocation.Reason?.Definite);

        await Assert.ThrowsAnyAsync<Exception>(() => Call(operatorClient, addr, "status")); // the next call is refused
        await Assert.ThrowsAnyAsync<Exception>(() => Call(operatorClient, addr, "ping"));   // and the connection is closed
        Assert.Null(server.Enrollment.PeerClaims(Pub(5)));
        var check = await Assert.ThrowsAsync<EnrollmentAdmissionException>(() => server.Enrollment.CheckPeerAsync(Pub(5)));
        Assert.NotEqual(AdmissionErrorKind.Other, check.Kind);
        // Unaffected peers keep working.
        Assert.Equal("ops.ingest", await Call(ingest, addr, "ping"));
    }

    // A server configured as a delegated issuer serves renewals; a client whose credential that
    // server signed renews automatically and stays admitted, with its roles, across several short
    // lifetimes. The server synchronizes policy from a configured replica (the root).
    [Fact]
    public async Task Config_Driven_Issuer_Renews_Client_Credentials()
    {
        string dir = TempDir();
        // The root as a policy replica: it holds the fleet namespace.
        var rootConfig = Enrolled(1, CredentialFor(1, ""));
        rootConfig = rootConfig with { Enrollment = rootConfig.Enrollment with { StatePath = Path.Combine(dir, "root.json") } };
        await using var rootServer = await AsterServer.StartAsync(new AsterServerOptions { Config = rootConfig });
        string rootTicket = (await rootServer.TicketAsync()).ToString();

        string delegationFile = Path.Combine(dir, "issuer.chain");
        await File.WriteAllTextAsync(delegationFile, Root.Delegation(Pub(2), [], ["aster.role"], TimeSpan.FromHours(1)) + "\n");
        var config = ServerConfig(dir);
        config = config with
        {
            Enrollment = config.Enrollment with
            {
                Replicas = [rootTicket],
                IssuerDelegationFile = delegationFile,
                IssuerLifetimeSecs = 6,
            },
        };
        var (server, addr) = await StartOps(new AsterServerOptions { Config = config });
        await using var _s = server;
        var issuer = server.Issuer;
        Assert.NotNull(issuer);
        Assert.Equal(Pub(2), issuer!.Info().PublicKey);
        // Claims beyond the delegation are refused with a code.
        var refusal = Assert.Throws<IssuerRefusalException>(() =>
            issuer.Issue(Pub(7), new EnrollmentClaims { Scopes = ["deploy"], Tags = [] }));
        Assert.Equal("invalid_credential", refusal.Code);
        Assert.Equal(Status.PermissionDenied, refusal.Status);
        var issued = issuer.Issue(Pub(7), new EnrollmentClaims
        {
            Scopes = [],
            Tags = [],
            Attributes = new Dictionary<string, ClaimAttribute> { ["aster.role"] = ClaimAttribute.String("ops.status") },
        });

        await using var client = await Client(Enrolled(7, issued.Chain));
        Assert.Equal("ops.status", await Call(client, addr, "status"));
        Assert.True(client.IsRenewing); // the server signed the credential, so the client renews from it
        var until = DateTime.UtcNow + TimeSpan.FromSeconds(3 * 6 + 2);
        while (DateTime.UtcNow < until)
        {
            Assert.Equal("ops.status", await Call(client, addr, "status"));
            await Task.Delay(500);
        }
        var current = client.Enrollment!.Credential();
        Assert.NotNull(current);
        Assert.NotEqual(issued.CredentialId, current!.CredentialId);
    }

    // Builder overrides of the policy and role mapping (Rust enrollment_policy / enrollment_roles)
    // take precedence over the configuration.
    [Fact]
    public async Task Role_Mapping_Override()
    {
        var (server, addr) = await StartOps(new AsterServerOptions
        {
            Config = ServerConfig(TempDir()),
            // Scopes are roles; the aster.role attribute grants nothing.
            EnrollmentRoles = new RoleMapping
            {
                Attribute = "",
                ScopesAsRoles = true,
                Tags = new Dictionary<string, IReadOnlyList<string>> { ["oncall"] = ["ops.logs"] },
            },
        });
        await using var _s = server;
        string chain = Root.Credential(Pub(5), TestRoot.RoleClaims("ops.admin", ["ops.status"], ["oncall"]), TimeSpan.FromHours(1));
        await using var client = await Client(Enrolled(5, chain));
        Assert.Equal("ops.logs,ops.status", await Call(client, addr, "status"));
    }

    // Misconfiguration stops the start with an error that names the problem; nothing runs open.
    [Fact]
    public async Task Misconfiguration_Fails_Closed()
    {
        string dir = TempDir();
        AsterConfig Good() => ServerConfig(dir);
        async Task Refused(string name, AsterServerOptions options, string expect)
        {
            var e = await Assert.ThrowsAnyAsync<Exception>(async () =>
            {
                await using var s = await AsterServer.StartAsync(options);
            });
            Assert.True(e.Message.Contains(expect, StringComparison.Ordinal), $"{name}: \"{expect}\" not in {e.Message}");
        }
        AsterConfig With(Func<EnrollmentConfig, EnrollmentConfig> f) { var c = Good(); return c with { Enrollment = f(c.Enrollment) }; }

        await Refused("no audience", new() { Config = With(e => e with { Audience = null }) }, "audience is required");
        await Refused("no root", new() { Config = Good() with { RootPubkey = null } }, "no trusted root");
        await Refused("no state", new() { Config = With(e => e with { StatePath = null }) }, "keeps denials durably");
        await Refused("no credential", new() { Config = Good() with { EnrollmentCredentialChain = null, Enrollment = Good().Enrollment with { Enabled = true } } },
            "no credential");
        await Refused("a path where the chain belongs", new() { Config = Good() with { EnrollmentCredentialChain = "/etc/aster/server.cred" } },
            "ASTER_ENROLLMENT_CREDENTIAL)");
        await Refused("a credential for another key", new() { Config = Good() with { EnrollmentCredentialChain = CredentialFor(3, "") } },
            "enrollment:");
        await Refused("unknown policy", new() { Config = With(e => e with { Policy = EnrollmentPolicy.FromJson("\"allow_everyone\"") }) }, "policy");
        await Refused("policy override", new() { Config = Good(), EnrollmentPolicy = EnrollmentPolicy.AllOf() }, "policy");
        await Refused("namespace", new() { Config = With(e => e with { FleetNamespace = "not-hex" }) }, "fleet_namespace");
        await Refused("enabled alone", new() { Config = Enrolled(2, CredentialFor(2, "")) with { Enrollment = new EnrollmentConfig { Enabled = true } } },
            "audience is required");
        using (var gate = Gate.Create())
            await Assert.ThrowsAsync<ArgumentException>(() => AsterServer.StartAsync(new AsterServerOptions { Config = Good(), Gate = gate }));

        // A client: no credential, and server-only issuer settings.
        var noCredential = await Assert.ThrowsAnyAsync<Exception>(() => AsterClient.StartAsync(Enrolled(5, null)));
        Assert.Contains("no credential", noCredential.Message);
        var withIssuer = Enrolled(5, CredentialFor(5, ""));
        withIssuer = withIssuer with { Enrollment = withIssuer.Enrollment with { IssuerDelegationFile = Path.Combine(dir, "x") } };
        var issuerError = await Assert.ThrowsAnyAsync<Exception>(() => AsterClient.StartAsync(withIssuer));
        Assert.Contains("server-only", issuerError.Message);

        // Enrollment off (the root alone does not enable it): an open server.
        var open = Loopback with { RootPubkey = Convert.FromHexString(Root.PublicKey) };
        Assert.False(open.IsEnrollmentEnabled);
        await using var server = await AsterServer.StartAsync(new AsterServerOptions { Config = open });
        Assert.Null(server.Enrollment);
        Assert.Null(server.Gate);
    }

    // aster.lease.Grantor through the server path: an enrolled client acquires through the
    // server's grantor as itself; acquiring for another holder fails locally.
    [Fact]
    public async Task Lease_Grantor_Serves_Enrolled_Clients_As_Themselves()
    {
        await using var grantorServer = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = ServerConfig(TempDir()),
            LeaseGrantor = new LeaseGrantorOptions(),
        });
        Assert.NotNull(grantorServer.LeaseSerializer);
        var addr = (await grantorServer.TicketAsync()).ToNodeAddr();
        await using var client = await Client(Enrolled(5, CredentialFor(5, "")));
        using var leases = await client.LeaseGrantorAsync(addr);
        var granted = Assert.IsType<Lease.AcquireResult.Granted>(await leases.AcquireAsync("jobs/1", client.Node.NodeId()));
        await granted.Lease.ReleaseAsync();
        await Assert.ThrowsAnyAsync<Exception>(() => leases.AcquireAsync("jobs/2", Pub(6)));
    }

    [Fact]
    public void Policy_Json_Matches_The_Rust_PolicyConfig()
    {
        string zeros = new('0', 64);
        var p = EnrollmentPolicy.AllOf(
            EnrollmentPolicy.AnyOf(EnrollmentPolicy.Scope("session"), EnrollmentPolicy.Tag("router")),
            EnrollmentPolicy.NoneOf(EnrollmentPolicy.IssuedBy(zeros)),
            EnrollmentPolicy.AttributeEquals("tier", ClaimAttribute.Int(2)),
            EnrollmentPolicy.SourceCidr("10.0.0.0/8"),
            EnrollmentPolicy.Audience("kasm"),
            EnrollmentPolicy.PolicySynchronizedWithin(TimeSpan.FromMinutes(1), true),
            EnrollmentPolicy.MaxCredentialAge(TimeSpan.FromHours(1)),
            EnrollmentPolicy.RequireRegistration(),
            EnrollmentPolicy.TolerateStalePolicy());
        string want = ("{'all_of':[{'any_of':[{'scope':'session'},{'tag':'router'}]},{'none_of':[{'issued_by':'" + zeros + "'}]}," +
            "{'attribute_equals':{'key':'tier','value':{'type':'int','value':2}}}," +
            "{'source_cidr':'10.0.0.0/8'},{'audience':'kasm'},{'policy_synchronized_within':{'max_age_secs':60,'require_root':true}}," +
            "{'max_credential_age_secs':3600},'require_registration','tolerate_stale_policy']}").Replace('\'', '"');
        Assert.Equal(want, p.ToJson());
        Assert.Equal(want, EnrollmentPolicy.FromJson(want).ToJson());
    }

    // [trust] and [enrollment] are read by Rust (aster_config_enrollment_from_toml) with the Rust
    // names, and survive the round trip into the trust JSON the node starts from.
    [Fact]
    public void Config_From_Toml_Reads_Trust_And_Enrollment()
    {
        string dir = TempDir();
        string path = Path.Combine(dir, "aster.toml");
        File.WriteAllText(path, $$"""
            [trust]
            root_pubkey = "{{Root.PublicKey}}"
            enrollment_credential = "server.cred"

            [enrollment]
            audience = "mission-control"
            fleet_namespace = "{{FleetNamespace}}"
            state_path = "./mc-enrollment.json"
            replicas = ["{{Pub(9)}}"]
            policy = "require_registration"

            [enrollment.limits]
            sync_timeout_ms = 5000

            [enrollment.roles]
            scopes_as_roles = true
            [enrollment.roles.tags]
            oncall = ["ops.logs"]

            [enrollment.issuer]
            delegation_file = "issuer.chain"
            lifetime = "2h"
            """);
        var c = AsterConfig.FromToml(path);
        Assert.True(c.IsEnrollmentEnabled);
        Assert.Equal(Root.PublicKey, Convert.ToHexString(c.RootPubkey!).ToLowerInvariant());
        Assert.Equal("server.cred", c.EnrollmentCredentialFile);
        Assert.Equal("mission-control", c.Enrollment.Audience);
        Assert.Equal(FleetNamespace, c.Enrollment.FleetNamespace);
        Assert.Equal("./mc-enrollment.json", c.Enrollment.StatePath);
        Assert.Equal([Pub(9)], c.Enrollment.Replicas);
        Assert.Equal("\"require_registration\"", c.Enrollment.Policy?.ToJson());
        Assert.Equal(5000UL, c.Enrollment.Limits.SyncTimeoutMs);
        Assert.True(c.Enrollment.Roles.ScopesAsRoles);
        Assert.Equal("aster.role", c.Enrollment.Roles.Attribute);
        Assert.Equal(["ops.logs"], c.Enrollment.Roles.Tags["oncall"]);
        Assert.Equal("issuer.chain", c.Enrollment.IssuerDelegationFile);
        Assert.Equal(7200UL, c.Enrollment.IssuerLifetimeSecs);

        var json = System.Text.Encoding.UTF8.GetString(c.TrustJsonBytes());
        Assert.Contains("\"fleet_namespace\":\"" + FleetNamespace + "\"", json);
        Assert.Contains("\"issuer_delegation_file\":\"issuer.chain\"", json);
        Assert.DoesNotContain("allow_all", json);
    }
}
