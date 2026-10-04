package site.aster.examples;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.handle.IrohRuntime;
import site.aster.node.IrohNode;
import site.aster.node.NodeAddr;

/**
 * Example: Connection Extras (8.1)
 *
 * <p>This example demonstrates how to: - Create runtimes and endpoints with matching ALPNs -
 * Connect two endpoints using node IDs - Accept connections - Close connections cleanly
 *
 * <p>Run with: mvn exec:java -Dexec.mainClass="site.aster.examples.ConnectionExtrasExample"
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

    // ABI 4.0: endpoints are views of nodes started with aster_node_start.
    IrohNode nodeA =
        IrohNode.memoryWithAlpns(
                java.util.List.of(ALPN.getBytes(java.nio.charset.StandardCharsets.UTF_8)))
            .get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    IrohRuntime runtimeA = nodeA.runtime();
    IrohEndpoint endpointA = IrohEndpoint.ofNode(runtimeA, nodeA.nodeHandle());

    String epAID = endpointA.nodeId();
    System.out.printf("   Endpoint A created! ID: %.8s...%n%n", epAID);

    // Create runtime B and endpoint B (dialer) with same ALPN
    System.out.println("2. Creating runtime B and endpoint B (dialer)...");

    // ABI 4.0: endpoints are views of nodes started with aster_node_start.
    IrohNode nodeB =
        IrohNode.memoryWithAlpns(
                java.util.List.of(ALPN.getBytes(java.nio.charset.StandardCharsets.UTF_8)))
            .get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    IrohRuntime runtimeB = nodeB.runtime();
    IrohEndpoint endpointB = IrohEndpoint.ofNode(runtimeB, nodeB.nodeHandle());

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
    CompletableFuture<site.aster.handle.Datagram> readFuture = connA.readDatagramAsync();

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
