package site.aster.examples;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import site.aster.config.EndpointConfig;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.handle.IrohRuntime;
import site.aster.handle.IrohStream;
import site.aster.node.NodeAddr;

/** Test bidirectional streams: OpenBi, Write, Read between two endpoints. */
public class StreamExample {

  private static final String ALPN = "aster";
  private static final int TIMEOUT_SECONDS = 15;

  public static void main(String[] args) {
    System.out.println("=== Stream Test ===\n");

    try {
      runExample();
    } catch (Exception e) {
      System.err.println("Error: " + e.getMessage());
      e.printStackTrace();
      System.exit(1);
    }
  }

  private static void runExample() throws Exception {
    // Create runtime A and endpoint A (listener)
    System.out.println("1. Creating endpoint A (listener)...");

    IrohRuntime runtimeA = IrohRuntime.create();
    EndpointConfig configA = new EndpointConfig().alpns(java.util.List.of(ALPN));
    IrohEndpoint endpointA =
        runtimeA.endpointCreateAsync(configA).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);

    String epAID = endpointA.nodeId();
    System.out.printf("   Endpoint A created!%n%n", epAID);

    // Create runtime B and endpoint B (dialer)
    System.out.println("2. Creating endpoint B (dialer)...");

    IrohRuntime runtimeB = IrohRuntime.create();
    EndpointConfig configB = new EndpointConfig().alpns(java.util.List.of(ALPN));
    IrohEndpoint endpointB =
        runtimeB.endpointCreateAsync(configB).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);

    String epBID = endpointB.nodeId();
    System.out.printf("   Endpoint B created!%n%n", epBID);

    // A accepts connections
    System.out.println("3. A accepting connections...");
    CompletableFuture<IrohConnection> acceptFuture = endpointA.acceptAsync();

    // Give A time to start accepting
    Thread.sleep(100);

    // B connects to A using A's address info
    System.out.println("4. B connecting to A...");
    NodeAddr addrA = endpointA.addrInfo();
    CompletableFuture<IrohConnection> connectFuture = endpointB.connectNodeAddrAsync(addrA, ALPN);
    IrohConnection connB = connectFuture.get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.println("   Connected!\n");

    // Wait for A to accept
    IrohConnection connA = acceptFuture.get(TIMEOUT_SECONDS, TimeUnit.SECONDS);

    // B opens a bidirectional stream
    System.out.println("5. B opening bidirectional stream...");
    CompletableFuture<IrohStream> openBiFuture = connB.openBiAsync();
    IrohStream streamB = openBiFuture.get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.println("   Stream opened!\n");

    // B sends data to A
    byte[] testData = "Hello Stream!".getBytes(java.nio.charset.StandardCharsets.UTF_8);
    System.out.printf(
        "6. B sending: %s%n", new String(testData, java.nio.charset.StandardCharsets.UTF_8));
    streamB.sendAsync(testData).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.println("   Write completed!\n");

    // A accepts a bidirectional stream
    System.out.println("7. A accepting bidirectional stream...");
    CompletableFuture<IrohStream> acceptBiFuture = connA.acceptBiAsync();
    IrohStream streamA = acceptBiFuture.get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.println("   Stream accepted!\n");

    // A reads the data
    System.out.println("8. A reading data...");
    byte[] received = streamA.readAsync(4096).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.printf(
        "   A received: %s%n", new String(received, java.nio.charset.StandardCharsets.UTF_8));

    if (java.util.Arrays.equals(testData, received)) {
      System.out.println("   ✓ Data matches!\n");
    } else {
      System.out.println("   ✗ Data mismatch!");
      System.exit(1);
    }

    // A sends response back
    byte[] response = "Hello from A!".getBytes(java.nio.charset.StandardCharsets.UTF_8);
    System.out.printf(
        "9. A sending response: %s%n",
        new String(response, java.nio.charset.StandardCharsets.UTF_8));
    streamA.sendAsync(response).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.println("   Write completed!\n");

    // B reads the response
    System.out.println("10. B reading response...");
    byte[] resp = streamB.readAsync(4096).get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.printf(
        "   B received: %s%n", new String(resp, java.nio.charset.StandardCharsets.UTF_8));

    if (java.util.Arrays.equals(response, resp)) {
      System.out.println("   ✓ Response matches!\n");
    } else {
      System.out.println("   ✗ Response mismatch!");
      System.exit(1);
    }

    // Finish streams
    System.out.println("11. Finishing streams...");
    streamB.finishAsync().get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    streamA.finishAsync().get(TIMEOUT_SECONDS, TimeUnit.SECONDS);
    System.out.println("   Streams finished!\n");

    // Close streams
    streamB.close();
    streamA.close();
    System.out.println("   Streams closed!\n");

    // Close connections
    System.out.println("12. Closing connections...");
    connB.close();
    connA.close();
    System.out.println("   Connections closed!\n");

    // Close endpoints
    endpointA.close();
    endpointB.close();
    runtimeA.close();
    runtimeB.close();

    System.out.println("=== SUCCESS: Bidirectional stream works! ===");
  }
}
