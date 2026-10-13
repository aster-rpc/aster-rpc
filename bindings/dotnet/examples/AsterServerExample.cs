using System;
using System.Threading.Tasks;
using Apache.Fory;
using Aster;
using Aster.Rpc;

namespace Examples;

[ForyStruct]
[WireType("examples/EchoRequest")]
public sealed class EchoRequest
{
    public string Message { get; set; } = "";
}

[ForyStruct]
[WireType("examples/EchoReply")]
public sealed class EchoReply
{
    public string Message { get; set; } = "";
}

/// <summary>A unary echo service served and called in-process over loopback.</summary>
static class AsterServerExample
{
    public static async Task RunAsync()
    {
        Console.WriteLine("=== AsterServer Echo Example (.NET) ===\n");

        var service = ServiceDefinition.Create("EchoService", 1)
            .Unary<EchoRequest, EchoReply>("echo", (req, ctx) =>
                ValueTask.FromResult(new EchoReply { Message = $"echo from {ctx.PeerId[..8]}: {req.Message}" }))
            .Build();
        Console.WriteLine($"contract_id: {service.ContractId}");

        // Loopback only, relays off: runs offline.
        var config = AsterConfig.FromEnv() with { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

        await using var server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = config, // no [enrollment] configured: an open server (see mc-server --auth)
            Services = { service },
        });
        var ticket = await server.TicketAsync();
        Console.WriteLine($"server: {ticket}");

        await using var client = await AsterClient.StartAsync(config);
        client.RegisterTypes(service);
        var conn = await client.ConnectAsync(ticket);
        var reply = await conn.UnaryAsync<EchoRequest, EchoReply>("EchoService", "echo", new EchoRequest { Message = "hello" });
        Console.WriteLine($"reply: {reply.Message}");
    }
}
