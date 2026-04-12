using System;
using System.Linq;
using System.Text;
using System.Threading.Tasks;
using Aster;

namespace Examples;

class AsterServerExample
{
    public static async Task RunAsync()
    {
        Console.WriteLine("=== AsterServer Echo Example (.NET) ===\n");

        // 0. Compute contract_id via the Rust FFI
        Console.WriteLine("0. Computing contract_id via Rust FFI...");
        string contractJson = """{"name": "EchoService", "version": 1, "methods": [], "serialization_modes": ["xlang"], "scoped": "shared", "requires": null, "producer_language": ""}""";
        string contractId = ContractIdentity.ComputeContractId(contractJson);
        Console.WriteLine($"   contract_id: {contractId}");
        if (contractId.Length != 64)
        {
            Console.WriteLine($"   FAIL: Expected 64-char hex, got {contractId.Length} chars");
            Environment.Exit(1);
        }
        Console.WriteLine("   PASS: Got valid 64-char hex contract_id.");

        // 1. Start the echo server
        Console.WriteLine("1. Starting AsterServer (echo handler)...");
        using var server = await AsterServer.CreateAsync(call =>
        {
            Console.WriteLine($"   Server received call from {call.PeerId[..Math.Min(8, call.PeerId.Length)]}...: header={call.Header.Length} bytes, request={call.Request.Length} bytes");
            return ReactorResponse.Of(call.Request); // Echo
        });

        string nodeId = server.NodeId();
        Console.WriteLine($"   Server started! Node ID: {nodeId[..Math.Min(16, nodeId.Length)]}...");

        // 2. Create a client endpoint
        Console.WriteLine("\n2. Creating client endpoint...");
        using var clientRuntime = new Runtime();
        var builder = new EndpointConfigBuilder().Alpn(AsterServer.AsterAlpn);
        using var clientEndpoint = await Endpoint.CreateAsync(clientRuntime, builder);
        Console.WriteLine("   Client endpoint created.");

        // 3. Connect client to server
        Console.WriteLine("\n3. Connecting client to server...");
        var serverAddr = server.Node.NodeAddr();
        using var conn = await clientEndpoint.ConnectWithAddrAsync(serverAddr, AsterServer.AsterAlpn);
        Console.WriteLine("   Connected!");

        // 4. Open stream and send Aster-framed RPC
        Console.WriteLine("\n4. Opening stream and sending RPC...");
        var (send, recv) = await conn.OpenBiAsync();

        byte[] headerPayload = Encoding.UTF8.GetBytes("EchoService.echo");
        byte[] requestPayload = Encoding.UTF8.GetBytes("Hello, Aster!");

        byte[] headerFrame = AsterServer.EncodeFrame(headerPayload, AsterServer.FlagHeader);
        byte[] requestFrame = AsterServer.EncodeFrame(requestPayload, 0);

        byte[] combined = new byte[headerFrame.Length + requestFrame.Length];
        Buffer.BlockCopy(headerFrame, 0, combined, 0, headerFrame.Length);
        Buffer.BlockCopy(requestFrame, 0, combined, headerFrame.Length, requestFrame.Length);

        await send.SendAsync(combined);
        await send.FinishAsync();
        Console.WriteLine("   Sent header + request.");

        // 5. Read the response
        Console.WriteLine("\n5. Reading response...");
        byte[] response = await recv.ReadAsync(4096);
        Console.WriteLine($"   Received {response.Length} bytes: {Encoding.UTF8.GetString(response)}");

        if (response.SequenceEqual(requestPayload))
        {
            Console.WriteLine("   PASS: Response matches echoed payload!");
        }
        else
        {
            Console.WriteLine($"   FAIL: Expected {requestPayload.Length} bytes, got {response.Length} bytes.");
            Environment.Exit(1);
        }

        // 6. Cleanup
        Console.WriteLine("\n6. Cleaning up...");
        send.Dispose();
        recv.Dispose();

        Console.WriteLine("\n=== SUCCESS: AsterServer echo round-trip works! ===");
    }
}
