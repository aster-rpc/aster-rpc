package com.aster.examples;

import com.aster.config.EndpointConfig;
import com.aster.contract.ContractIdentity;
import com.aster.handle.IrohConnection;
import com.aster.handle.IrohEndpoint;
import com.aster.handle.IrohRuntime;
import com.aster.handle.IrohStream;
import com.aster.node.NodeAddr;
import com.aster.server.AsterFraming;
import com.aster.server.AsterServer;
import com.aster.server.CallResponse;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;

/**
 * End-to-end AsterServer example: echo server + client.
 *
 * <p>The server echoes back the request payload. The client sends a header + request, reads the
 * response, and verifies the round-trip.
 */
public class AsterServerExample {

  private static final int TIMEOUT = 15;

  public static void main(String[] args) throws Exception {
    System.out.println("=== AsterServer Echo Example ===\n");

    // 0. Compute contract_id via the Rust canonicalizer
    System.out.println("0. Computing contract_id via Rust FFI...");
    String contractJson = """
        {"name": "EchoService", "version": 1,
         "methods": [], "serialization_modes": ["xlang"],
         "scoped": "shared", "requires": null, "producer_language": ""}
        """;
    String contractId = ContractIdentity.computeContractId(contractJson);
    System.out.println("   contract_id: " + contractId);
    if (contractId.length() != 64) {
      System.out.println("   FAIL: Expected 64-char hex, got " + contractId.length() + " chars");
      System.exit(1);
    }
    System.out.println("   PASS: Got valid 64-char hex contract_id.");

    // 1. Start the echo server
    System.out.println("1. Starting AsterServer (echo handler)...");
    AsterServer server =
        AsterServer.builder()
            .handler(
                call -> {
                  System.out.printf(
                      "   Server received call from %s: header=%d bytes, request=%d bytes%n",
                      call.peerId().substring(0, 8) + "...",
                      call.header().length,
                      call.request().length);
                  // Echo the request back as the response
                  return CallResponse.of(call.request());
                })
            .build()
            .get(TIMEOUT, TimeUnit.SECONDS);

    System.out.println("   Server started! Node ID: " + server.nodeId().substring(0, 16) + "...");

    // 2. Create a client endpoint
    System.out.println("\n2. Creating client endpoint...");
    IrohRuntime clientRuntime = IrohRuntime.create();
    EndpointConfig clientConfig = new EndpointConfig().alpns(List.of(AsterServer.ASTER_ALPN));
    IrohEndpoint clientEndpoint =
        clientRuntime.endpointCreateAsync(clientConfig).get(TIMEOUT, TimeUnit.SECONDS);
    System.out.println("   Client endpoint created.");

    // 3. Connect client to server
    System.out.println("\n3. Connecting client to server...");
    NodeAddr serverAddr = server.node().nodeAddr();
    IrohConnection conn =
        clientEndpoint
            .connectNodeAddrAsync(serverAddr, AsterServer.ASTER_ALPN)
            .get(TIMEOUT, TimeUnit.SECONDS);
    System.out.println("   Connected!");

    // 4. Open a bidirectional stream and send Aster-framed RPC
    System.out.println("\n4. Opening stream and sending RPC...");
    IrohStream stream = conn.openBiAsync().get(TIMEOUT, TimeUnit.SECONDS);

    byte[] headerPayload = "EchoService.echo".getBytes(StandardCharsets.UTF_8);
    byte[] requestPayload = "Hello, Aster!".getBytes(StandardCharsets.UTF_8);

    // Encode as Aster wire frames: HEADER + REQUEST
    byte[] headerFrame = AsterFraming.encodeFrame(headerPayload, AsterFraming.FLAG_HEADER);
    byte[] requestFrame = AsterFraming.encodeFrame(requestPayload, (byte) 0);

    // Send both frames in one write
    byte[] combined = new byte[headerFrame.length + requestFrame.length];
    System.arraycopy(headerFrame, 0, combined, 0, headerFrame.length);
    System.arraycopy(requestFrame, 0, combined, headerFrame.length, requestFrame.length);
    stream.sendAsync(combined).get(TIMEOUT, TimeUnit.SECONDS);
    stream.finishAsync().get(TIMEOUT, TimeUnit.SECONDS);
    System.out.println("   Sent header + request.");

    // 5. Read the response
    System.out.println("\n5. Reading response...");
    byte[] response = stream.readAsync(4096).get(TIMEOUT, TimeUnit.SECONDS);
    System.out.printf(
        "   Received %d bytes: %s%n",
        response.length, new String(response, StandardCharsets.UTF_8));

    // The reactor writes response_frame bytes directly to the QUIC stream (no framing).
    // Our echo handler returns CallResponse.of(request), so we should get the request back.
    if (Arrays.equals(response, requestPayload)) {
      System.out.println("   PASS: Response matches echoed payload!");
    } else {
      System.out.printf(
          "   FAIL: Expected %d bytes, got %d bytes.%n", requestPayload.length, response.length);
      System.exit(1);
    }

    // 6. Cleanup
    System.out.println("\n6. Cleaning up...");
    stream.close();
    conn.close();
    clientEndpoint.close();
    clientRuntime.close();
    server.close();

    System.out.println("\n=== SUCCESS: AsterServer echo round-trip works! ===");
  }
}
