// The .NET side of the InteropEcho contract (tests/interop/): a leg of the
// tests/interop/run.py matrix (contract / server / client).
//
// Contract (must match tests/interop/rust/contract.rs):
//
//   service InteropEcho v1
//     echo(interop/EchoRequest) -> interop/EchoReply                (unary)
//     generate(interop/EchoRequest) -> stream interop/EchoReply     (server stream)
//     ingest(stream interop/EchoRequest) -> interop/EchoReply       (client stream)
//   interop/EchoRequest { message: string, count: varint32, data: binary }
//   interop/EchoReply   { message: string, count: varint32, data: binary, server_lang: string }
//
// Usage: dotnet InteropEcho.dll contract | server | client <aster1-ticket>
// Every mode prints CONTRACT_ID <hex> first; `server` prints ADDRESS <aster1...> and serves
// until stdin closes (loopback, relays off); `client` prints CASE <name> OK|FAIL lines.

using System.Runtime.CompilerServices;
using Apache.Fory;
using Aster;
using Aster.Rpc;

var service = InteropEcho.Service("dotnet");
Console.WriteLine($"CONTRACT_ID {service.ContractId}");

switch (args)
{
    case ["contract"]:
        return 0;
    case ["server"]:
        await InteropEcho.ServeAsync(service);
        return 0;
    case ["client", var ticket]:
        return await InteropEcho.ClientAsync(service, ticket) ? 0 : 1;
    default:
        Console.Error.WriteLine("usage: InteropEcho contract | server | client <aster1-ticket>");
        return 2;
}

[ForyStruct]
[WireType("interop/EchoRequest")]
public sealed class EchoRequest
{
    public string Message { get; set; } = "";
    public int Count { get; set; }
    public byte[] Data { get; set; } = [];
}

[ForyStruct]
[WireType("interop/EchoReply")]
public sealed class EchoReply
{
    public string Message { get; set; } = "";
    public int Count { get; set; }
    public byte[] Data { get; set; } = [];
    public string ServerLang { get; set; } = "";
}

internal static class InteropEcho
{
    /// <summary>The service (same semantics in every language).</summary>
    public static ServiceDefinition Service(string lang) => ServiceDefinition.Create("InteropEcho", 1)
        .Unary<EchoRequest, EchoReply>("echo", (req, _) => ValueTask.FromResult(new EchoReply
        {
            Message = req.Message,
            Count = unchecked(req.Count + 1),
            Data = req.Data,
            ServerLang = lang,
        }))
        .ServerStream<EchoRequest, EchoReply>("generate", Generate)
        .ClientStream<EchoRequest, EchoReply>("ingest", async (reqs, ctx) =>
        {
            int n = 0;
            string last = "";
            await foreach (var r in reqs.WithCancellation(ctx.CancellationToken))
            {
                n = unchecked(n + 1);
                last = r.Message;
            }
            return new EchoReply { Message = last, Count = n, Data = [], ServerLang = lang };
        })
        .Build();

    private static async IAsyncEnumerable<EchoReply> Generate(EchoRequest req, ServerCallContext ctx)
    {
        for (int i = 0; i < Math.Max(req.Count, 0); i++)
        {
            yield return new EchoReply { Message = req.Message, Count = i, Data = req.Data, ServerLang = "dotnet" };
            await Task.Yield();
        }
    }

    private static AsterConfig Config() => AsterConfig.FromEnv() with { RelayMode = "disabled", BindAddr = "127.0.0.1:0" };

    public static async Task ServeAsync(ServiceDefinition service)
    {
        await using var server = await AsterServer.StartAsync(new AsterServerOptions
        {
            Config = Config() with { AllowAllConsumers = true },
            Services = { service },
        });
        var ticket = await server.TicketAsync();
        Console.Error.WriteLine($"node addr: {server.Node.NodeAddr()}");
        Console.WriteLine($"ADDRESS {ticket}");
        Console.Out.Flush();
        while (await Console.In.ReadLineAsync() != null)
        {
            // serve until stdin closes
        }
    }

    /// <summary>The shared cases. Must match cases() in the other drivers.</summary>
    private static IEnumerable<(string Name, EchoRequest Request)> Cases() =>
    [
        ("small", new EchoRequest { Message = "hello from dotnet", Count = 41, Data = [0, 1, 2, 255] }),
        // > 4096 bytes on the wire and highly compressible: exercises zstd.
        ("zstd", new EchoRequest
        {
            Message = string.Concat(Enumerable.Repeat("héllo ✓ ", 200)),
            Count = -123_456,
            Data = Enumerable.Range(0, 20_000).Select(i => (byte)(i % 251)).ToArray(),
        }),
        // A single string field longer than 4095 bytes (Fory string-length edge).
        ("long_string", new EchoRequest { Message = new string('x', 5000), Count = int.MaxValue - 1, Data = [] }),
    ];

    private static string Check(EchoRequest req, EchoReply reply)
    {
        if (reply.Message != req.Message)
            throw new InvalidOperationException($"message mismatch (len {reply.Message.Length} vs {req.Message.Length})");
        if (reply.Count != unchecked(req.Count + 1))
            throw new InvalidOperationException($"count {reply.Count} != {unchecked(req.Count + 1)}");
        if (!reply.Data.AsSpan().SequenceEqual(req.Data))
            throw new InvalidOperationException($"data mismatch (len {reply.Data.Length} vs {req.Data.Length})");
        if (string.IsNullOrEmpty(reply.ServerLang))
            throw new InvalidOperationException("empty server_lang");
        return reply.ServerLang;
    }

    private static async Task<bool> RunCase(string name, Func<Task<string>> body)
    {
        try
        {
            using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(15));
            string lang = await body().WaitAsync(timeout.Token);
            Console.WriteLine($"CASE {name} OK server_lang={lang}");
            return true;
        }
        catch (Exception e)
        {
            Console.WriteLine($"CASE {name} FAIL {e.GetType().Name}: {e.Message}");
            return false;
        }
        finally
        {
            Console.Out.Flush();
        }
    }

    public static async Task<bool> ClientAsync(ServiceDefinition service, string ticketText)
    {
        await using var client = await AsterClient.StartAsync(Config());
        client.RegisterTypes(service);
        RpcConnection conn;
        try
        {
            conn = await client.ConnectAsync(AsterTicket.Parse(ticketText)).WaitAsync(TimeSpan.FromSeconds(30));
        }
        catch (Exception e)
        {
            Console.WriteLine($"CASE connect FAIL {e.GetType().Name}: {e.Message}");
            return false;
        }

        bool ok = true;
        foreach (var (name, req) in Cases())
            ok &= await RunCase(name, async () =>
                Check(req, await conn.UnaryAsync<EchoRequest, EchoReply>("InteropEcho", "echo", req)));

        ok &= await RunCase("server_stream", async () =>
        {
            var req = new EchoRequest { Message = "stream", Count = 3, Data = [7, 8, 9] };
            var got = new List<EchoReply>();
            await foreach (var r in conn.ServerStreamAsync<EchoRequest, EchoReply>("InteropEcho", "generate", req))
                got.Add(r);
            if (got.Count != 3) throw new InvalidOperationException($"{got.Count} replies != 3");
            for (int i = 0; i < 3; i++)
                if (got[i].Count != i || got[i].Message != req.Message || !got[i].Data.AsSpan().SequenceEqual(req.Data))
                    throw new InvalidOperationException($"reply {i} mismatch");
            return got[0].ServerLang;
        });

        ok &= await RunCase("client_stream", async () =>
        {
            var r = await conn.ClientStreamAsync<EchoRequest, EchoReply>("InteropEcho", "ingest", Requests());
            if (r.Count != 3 || r.Message != "m2") throw new InvalidOperationException($"reply mismatch: {r.Count} {r.Message}");
            if (string.IsNullOrEmpty(r.ServerLang)) throw new InvalidOperationException("empty server_lang");
            return r.ServerLang;
        });
        return ok;
    }

    private static async IAsyncEnumerable<EchoRequest> Requests([EnumeratorCancellation] CancellationToken ct = default)
    {
        for (int i = 0; i < 3; i++)
        {
            ct.ThrowIfCancellationRequested();
            yield return new EchoRequest { Message = $"m{i}", Count = i, Data = [(byte)i] };
            await Task.Yield();
        }
    }
}
