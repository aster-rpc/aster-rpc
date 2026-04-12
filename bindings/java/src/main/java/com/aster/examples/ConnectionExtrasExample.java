package com.aster.examples;

import com.aster.config.EndpointConfig;
import com.aster.handle.IrohConnection;
import com.aster.handle.IrohEndpoint;
import com.aster.handle.IrohRuntime;
import com.aster.node.NodeAddr;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

/**
 * Example: Connection Extras (8.1)
 *
 * <p>This example demonstrates how to: - Create runtimes and endpoints with matching ALPNs -
 * Connect two endpoints using node IDs - Accept connections - Close connections cleanly
 *
 * <p>Run with: mvn exec:java -Dexec.mainClass="com.aster.examples.ConnectionExtrasExample"
 */
public class ConnectionExtrasExample {

  private static final String ALPN = "aster";
  private static final int TIMEOUT_SECONDS = 15;

  public static void main(String[] args) {
    System.out.println("=== Connection Extras Example (8.1) ===\n");

    try {
      runExample();
    } catch (Exception e) {
      System.err.println("Error: " + e.getMessage());
      e.printStackTrace();
      System.exit(1);
    }
  }

  private static void runExample() throws Exception {
    // Create runtime A and endpoint A (listener) with ALPN
    System.out.println("1. Creating runtime A and endpoint A (listener)...");

    IrohRuntime runtimeA = IrohRuntime.create();
    EndpointConfig configA = new EndpointConfig().alpns(java.util.List.of(ALPN));
    IrohEndpoint endpointA =
        runtimeA.endpointCreateAsync(configA).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);

    String epAID = endpointA.nodeId();
    System.out.printf("   Endpoint A created! ID: %.8s...%n%n", epAID);

    // Create runtime B and endpoint B (dialer) with same ALPN
    System.out.println("2. Creating runtime B and endpoint B (dialer)...");

    IrohRuntime runtimeB = IrohRuntime.create();
    EndpointConfig configB = new EndpointConfig().alpns(java.util.List.of(ALPN));
    IrohEndpoint endpointB =
        runtimeB.endpointCreateAsync(configB).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);

    String epBID = endpointB.nodeId();
    System.out.printf("   Endpoint B created! ID: %.8s...%n%n", epBID);

    // A accepts connections
    System.out.println("3. A accepting connections...");
    CompletableFuture<IrohConnection> acceptFuture = endpointA.acceptAsync();

    // Give A time to start accepting
    Thread.sleep(100);

    // B connects to A using A's address info (includes relay URL)
    System.out.println("4. B connecting to A...");
    NodeAddr addrA = endpointA.addrInfo();
    CompletableFuture<IrohConnection> connectFuture = endpointB.connectNodeAddrAsync(addrA, ALPN);
    IrohConnection connB = connectFuture.get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.println("   Connected!\n");

    // Wait for A to accept
    IrohConnection connA = acceptFuture.get(TIMEOUT_SECONDS, TimeUnit.SECONDS);

    System.out.println("5. Testing Connection Extras...\n");

    // RemoteID: A gets B's ID
    String remoteIdB = connA.remoteId();
    System.out.printf("   RemoteID: %.8s... (expected: %.8s...)%n", remoteIdB, epBID);
    if (remoteIdB.equals(epBID)) {
      System.out.println("   ✓ RemoteID matches!");
    } else {
      System.out.println("   ✗ RemoteID mismatch!");
    }
    System.out.println();

    // MaxDatagramSize
    var maxSize = connB.maxDatagramSize();
    if (maxSize.isPresent()) {
      System.out.printf("   MaxDatagramSize: %d bytes%n", maxSize.getAsInt());
      System.out.println("   ✓ Datagrams supported!");
    } else {
      System.out.println("   MaxDatagramSize: not configured");
    }
    System.out.println();

    // Test bidirectional datagram send/receive.
    // NOTE: readDatagramAsync() relies on the iroh FFI emitting BYTES_RESULT (91), not
    // DATAGRAM_RECEIVED (60).
    // DATAGRAM_RECEIVED is defined in the FFI spec but no FFI function emits it.
    // See IrohEventKind.java TODO comment about this discrepancy.
    System.out.println("   --- Testing Datagram Send/Receive ---");
    byte[] testData = "Hello Datagram!".getBytes(java.nio.charset.StandardCharsets.UTF_8);

    // Start reading datagram on A (non-blocking)
    CompletableFuture<com.aster.handle.Datagram> readFuture = connA.readDatagramAsync();

    // Send datagram from B to A
    connB.sendDatagramAsync(testData).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.printf(
        "   B sent datagram: %s%n", new String(testData, java.nio.charset.StandardCharsets.UTF_8));

    // A receives the datagram
    var received = readFuture.get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.printf(
        "   A received datagram: %s%n",
        new String(received.data(), java.nio.charset.StandardCharsets.UTF_8));

    java.util.Arrays.equals(testData, received.data());
    if (java.util.Arrays.equals(testData, received.data())) {
      System.out.println("   ✓ Datagram data matches!");
    } else {
      System.out.println("   ✗ Datagram data mismatch!");
    }
    System.out.println();

    // Close connection
    System.out.println("6. Closing connection...");
    connB.close();
    connA.close();
    endpointA.close();
    endpointB.close();
    runtimeA.close();
    runtimeB.close();
    System.out.println("   ✓ Connection closed!");
    System.out.println();

    System.out.println("=== SUCCESS ===");
  }
}
