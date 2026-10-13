using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Threading;
using System.Threading.Tasks;
using Apache.Fory;
using Aster;
using Aster.Interceptors;
using Aster.Rpc;

namespace Examples;

// Mission Control for .NET: the MissionControl service of the Rust, Python, TypeScript and Go
// examples (all four RPC patterns) with the auth chapter's roles. Same service, method, wire type
// and role names as examples/rust/mission-control.

/// <summary>
/// Capability roles (the auth chapter). Each is a string in the <c>aster.role</c> claim of a node's
/// enrollment credential (comma-separated for several). The server maps a peer's verified claims
/// to these roles and Gate 3 matches them against each method's requirement. Identical to the Rust
/// example's <c>roles.rs</c>, Python's <c>roles.py</c> and Go's <c>mc.go</c>.
/// </summary>
public static class Roles
{
    public const string Status = "ops.status"; // may read fleet status (getStatus)
    public const string Logs = "ops.logs";     // may tail logs (tailLogs)
    public const string Admin = "ops.admin";   // administers agents: tails logs, runs commands
    public const string Ingest = "ops.ingest"; // may push metrics (ingestMetrics)
}

[ForyStruct, WireType("mission/StatusRequest")]
public sealed class StatusRequest { public string AgentId { get; set; } = ""; }

[ForyStruct, WireType("mission/StatusResponse")]
public sealed class StatusResponse
{
    public string AgentId { get; set; } = "";
    public string Status { get; set; } = "";
    public long UptimeSecs { get; set; }
}

[ForyStruct, WireType("mission/LogEntry")]
public sealed class LogEntry
{
    public double Timestamp { get; set; }
    public string Level { get; set; } = "";
    public string Message { get; set; } = "";
    public string AgentId { get; set; } = "";
}

[ForyStruct, WireType("mission/SubmitLogResult")]
public sealed class SubmitLogResult { public bool Accepted { get; set; } }

[ForyStruct, WireType("mission/TailRequest")]
public sealed class TailRequest
{
    public string AgentId { get; set; } = "";
    public string Level { get; set; } = "";
}

[ForyStruct, WireType("mission/MetricPoint")]
public sealed class MetricPoint
{
    public string Name { get; set; } = "";
    public double Value { get; set; }
    public double Timestamp { get; set; }
}

[ForyStruct, WireType("mission/IngestResult")]
public sealed class IngestResult
{
    public int Accepted { get; set; }
    public int Dropped { get; set; }
}

[ForyStruct, WireType("mission/Command")]
public sealed class CommandRequest { public string Command { get; set; } = ""; }

[ForyStruct, WireType("mission/CommandResult")]
public sealed class CommandResult
{
    public string Stdout { get; set; } = "";
    public string Stderr { get; set; } = "";
    public int ExitCode { get; set; }
}

public static class MissionControl
{
    /// <summary>
    /// MissionControl. With <paramref name="auth"/> each method carries the auth chapter's requirement:
    /// <c>getStatus</c> ops.status; <c>submitLog</c> any enrolled node; <c>tailLogs</c> ops.logs or
    /// ops.admin; <c>ingestMetrics</c> ops.ingest; <c>runCommand</c> ops.admin. No handler reads a
    /// credential: the server's <c>[enrollment]</c> configuration admits only enrolled nodes and turns
    /// their credentials into roles.
    /// </summary>
    public static ServiceDefinition Service(bool auth)
    {
        CapabilityRequirement? Req(CapabilityRequirement r) => auth ? r : null;
        return ServiceDefinition.Create("MissionControl", 1)
            .Unary<StatusRequest, StatusResponse>("getStatus",
                (r, _) => ValueTask.FromResult(new StatusResponse { AgentId = r.AgentId, Status = "running", UptimeSecs = 3600 }),
                requires: Req(CapabilityRequirement.Role(Roles.Status)))
            .Unary<LogEntry, SubmitLogResult>("submitLog", (_, _) => ValueTask.FromResult(new SubmitLogResult { Accepted = true }))
            .ServerStream<TailRequest, LogEntry>("tailLogs", TailLogs, requires: Req(CapabilityRequirement.AnyOf(Roles.Logs, Roles.Admin)))
            .ClientStream<MetricPoint, IngestResult>("ingestMetrics", IngestMetrics, requires: Req(CapabilityRequirement.Role(Roles.Ingest)))
            .BidiStream<CommandRequest, CommandResult>("runCommand", RunCommand, requires: Req(CapabilityRequirement.Role(Roles.Admin)))
            .Build();
    }

    private static async IAsyncEnumerable<LogEntry> TailLogs(TailRequest r, ServerCallContext ctx)
    {
        for (int i = 0; i < 3; i++)
        {
            await Task.Yield();
            yield return new LogEntry { Timestamp = i, Level = "info", Message = $"log {i}", AgentId = r.AgentId };
        }
    }

    private static async ValueTask<IngestResult> IngestMetrics(IAsyncEnumerable<MetricPoint> points, ServerCallContext ctx)
    {
        int accepted = 0;
        await foreach (var _ in points.WithCancellation(ctx.CancellationToken)) accepted++;
        return new IngestResult { Accepted = accepted };
    }

    // Echoes each command; it does not run a shell.
    private static async IAsyncEnumerable<CommandResult> RunCommand(IAsyncEnumerable<CommandRequest> commands, ServerCallContext ctx)
    {
        await foreach (var c in commands.WithCancellation(ctx.CancellationToken))
            yield return new CommandResult { Stdout = "ran: " + c.Command };
    }

    private static AsterConfig LoadConfig(string? configFile)
        => configFile is null ? AsterConfig.FromEnv() : AsterConfig.FromToml(configFile);

    /// <summary>
    /// <c>mc-server [--auth] [--config aster.toml]</c>. Everything comes from the configuration:
    /// <c>ASTER_*</c> variables, or an <c>aster.toml</c>. With <c>--auth</c> the server needs
    /// enrollment configured (see the README).
    /// </summary>
    public static async Task<int> ServerAsync(string[] args)
    {
        bool auth = args.Contains("--auth");
        int i = Array.IndexOf(args, "--config");
        var config = LoadConfig(i >= 0 && i + 1 < args.Length ? args[i + 1] : null);
        if (auth && !config.IsEnrollmentEnabled)
        {
            Console.Error.WriteLine("--auth needs enrollment configured: ASTER_ROOT_PUBKEY, ASTER_ENROLLMENT_AUDIENCE, " +
                "ASTER_ENROLLMENT_FLEET_NAMESPACE, ASTER_ENROLLMENT_CREDENTIAL and ASTER_ENROLLMENT_STATE_PATH (see the README)");
            return 2;
        }
        await using var server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = config,
            Services = { Service(auth) },
        });
        Console.WriteLine($"Iroh node id: {server.NodeId()}");
        Console.WriteLine($"Address: {await server.TicketAsync()}");
        var stop = new TaskCompletionSource();
        Console.CancelKeyPress += (_, e) => { e.Cancel = true; stop.TrySetResult(); };
        await stop.Task;
        return 0;
    }

    /// <summary>
    /// <c>mc-agent &lt;server address&gt;</c> (or <c>ASTER_ENDPOINT_ADDR</c>): calls getStatus,
    /// submitLog and ingestMetrics. Against the <c>--auth</c> server set this agent's key and
    /// credential (<c>ASTER_SECRET_KEY_FILE</c>, <c>ASTER_ENROLLMENT_CREDENTIAL</c>) and the same root,
    /// audience and fleet namespace as the server; the roles in the credential decide which calls succeed.
    /// </summary>
    public static async Task<int> AgentAsync(string[] args)
    {
        var config = AsterConfig.FromEnv();
        string? address = args.Length > 0 ? args[0] : config.EndpointAddr;
        if (string.IsNullOrEmpty(address))
        {
            Console.Error.WriteLine("usage: mc-agent <server address> (or ASTER_ENDPOINT_ADDR)");
            return 2;
        }
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(60));
        await using var client = await AsterClient.StartAsync(config, cancellationToken: timeout.Token);
        client.RegisterTypes(Service(auth: false));
        try
        {
            // Enrolls with the server first when enrollment is configured.
            var conn = await client.ConnectAsync(AsterTicket.Parse(address), timeout.Token);
            return await CallsAsync(conn, timeout.Token);
        }
        catch (Exception e) when (e is RpcError or IrohException)
        {
            // No credential, a revoked one, or a server that is gone: the connection is refused.
            Console.WriteLine($"refused: {e.Message}");
            return 1;
        }
    }

    private static async Task<int> CallsAsync(RpcConnection conn, CancellationToken ct)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(ct);

        try
        {
            var status = await conn.UnaryAsync<StatusRequest, StatusResponse>("MissionControl", "getStatus",
                new StatusRequest { AgentId = "dotnet-agent-1" }, cancellationToken: timeout.Token);
            Console.WriteLine($"Status: {status.AgentId} {status.Status} uptime={status.UptimeSecs}s");
        }
        catch (RpcError e) { Console.WriteLine($"getStatus refused: {e.Code}"); }

        double now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() / 1000.0;
        await conn.UnaryAsync<LogEntry, SubmitLogResult>("MissionControl", "submitLog",
            new LogEntry { Timestamp = now, Level = "info", Message = "agent started", AgentId = "dotnet-agent-1" },
            cancellationToken: timeout.Token);
        Console.WriteLine("Log submitted");

        try
        {
            var points = Enumerable.Range(0, 100).Select(i => new MetricPoint { Name = "cpu.usage", Value = i % 100, Timestamp = now });
            var result = await conn.ClientStreamAsync<MetricPoint, IngestResult>("MissionControl", "ingestMetrics", points,
                cancellationToken: timeout.Token);
            Console.WriteLine($"Metrics accepted: {result.Accepted}");
        }
        catch (RpcError e) { Console.WriteLine($"ingestMetrics refused: {e.Code}"); }
        return 0;
    }
}
