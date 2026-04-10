package com.aster;

import com.aster.config.EndpointConfig;
import com.aster.handle.IrohConnection;
import com.aster.handle.IrohEndpoint;
import com.aster.handle.IrohRuntime;
import com.aster.handle.IrohStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.Flow.Subscriber;
import java.util.concurrent.Flow.Subscription;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Disabled;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * Phase 2 integration test: create runtime {@literal ->} create endpoint {@literal ->} connect two
 * endpoints {@literal ->} open bidirectional stream {@literal ->} send/receive frame.
 *
 * <p>Requires {@code IROH_LIB_PATH} to point to {@code libaster_transport_ffi.dylib} and a running
 * relay server or direct network connectivity between endpoints.
 *
 * <p>Marked {@link Disabled} because connecting two fresh endpoints requires either a relay or
 * direct IP connectivity. The FFI plumbing is verified end-to-end up to the iroh layer.
 */
@Disabled("Requires relay server or direct IP connectivity between endpoints")
class ConnectionStreamsIntegrationTest {

  @TempDir Path tempDir;

  private IrohRuntime serverRuntime;
  private IrohEndpoint serverEndpoint;
  private IrohRuntime clientRuntime;
  private IrohEndpoint clientEndpoint;
  private Path nodeIdFile;

  @BeforeEach
  void setUp() throws Exception {
    nodeIdFile = tempDir.resolve("server_node_id.txt");

    // Server runtime and endpoint
    serverRuntime = IrohRuntime.create();
    serverEndpoint = serverRuntime.endpointCreateAsync(new EndpointConfig()).get();

    // Write server node ID to shared file so client can find it
    String serverNodeId = serverEndpoint.nodeId();
    Files.writeString(nodeIdFile, serverNodeId, StandardCharsets.UTF_8);

    // Client runtime and endpoint
    clientRuntime = IrohRuntime.create();
    clientEndpoint = clientRuntime.endpointCreateAsync(new EndpointConfig()).get();
  }

  @AfterEach
  void tearDown() {
    try {
      if (clientEndpoint != null) {
        clientEndpoint.close();
      }
    } finally {
      try {
        if (clientRuntime != null) {
          clientRuntime.close();
        }
      } finally {
        try {
          if (serverEndpoint != null) {
            serverEndpoint.close();
          }
        } finally {
          if (serverRuntime != null) {
            serverRuntime.close();
          }
        }
      }
    }
  }

  @Test
  void connectTwoEndpointsOpenStreamSendFrame()
      throws IOException, InterruptedException, ExecutionException {
    String serverNodeId = Files.readString(nodeIdFile, StandardCharsets.UTF_8).trim();
    byte[] testPayload = "hello from client".getBytes(StandardCharsets.UTF_8);
    byte[] serverResponse = "hello from server".getBytes(StandardCharsets.UTF_8);

    // Client connects to server using the shared node ID
    IrohConnection clientConn = clientEndpoint.connectAsync(serverNodeId, "test-alpn").get();

    // Server accepts the connection
    IrohConnection serverConn = serverEndpoint.acceptAsync().get();

    // Client opens a bidirectional stream
    IrohStream clientStream = clientConn.openBiAsync().get();

    // Server accepts the bidirectional stream
    IrohStream serverStream = serverConn.acceptBiAsync().get();

    // --- Server side: subscribe to receive client payload ---
    CountDownLatch serverFrameLatch = new CountDownLatch(1);
    byte[][] receivedPayload = new byte[1][];

    serverStream
        .receiveFrames()
        .subscribe(
            new Subscriber<>() {
              private Subscription subscription;

              @Override
              public void onSubscribe(Subscription s) {
                subscription = s;
                subscription.request(1);
              }

              @Override
              public void onNext(byte[] item) {
                receivedPayload[0] = item;
                serverFrameLatch.countDown();
                subscription.request(1);
              }

              @Override
              public void onError(Throwable t) {}

              @Override
              public void onComplete() {}
            });

    // --- Client sends a frame and waits for completion ---
    clientStream.sendAsync(testPayload).toCompletableFuture().join();
    clientStream.finishAsync().get();

    // Wait for server to receive the frame
    boolean received = serverFrameLatch.await(10, TimeUnit.SECONDS);
    assert received : "Server did not receive frame within timeout";
    assert receivedPayload[0] != null;
    assert receivedPayload[0].length == testPayload.length;

    // --- Client side: subscribe to receive server response ---
    CountDownLatch clientFrameLatch = new CountDownLatch(1);
    byte[][] clientReceivedPayload = new byte[1][];

    clientStream
        .receiveFrames()
        .subscribe(
            new Subscriber<>() {
              private Subscription subscription;

              @Override
              public void onSubscribe(Subscription s) {
                subscription = s;
                subscription.request(1);
              }

              @Override
              public void onNext(byte[] item) {
                clientReceivedPayload[0] = item;
                clientFrameLatch.countDown();
                subscription.request(1);
              }

              @Override
              public void onError(Throwable t) {}

              @Override
              public void onComplete() {}
            });

    // --- Server sends response ---
    serverStream.sendAsync(serverResponse).toCompletableFuture().join();
    serverStream.finishAsync().get();

    boolean clientReceived = clientFrameLatch.await(10, TimeUnit.SECONDS);
    assert clientReceived : "Client did not receive response within timeout";
    assert clientReceivedPayload[0] != null;
    assert clientReceivedPayload[0].length == serverResponse.length;

    // Clean up
    clientStream.close();
    serverStream.close();
    clientConn.close();
    serverConn.close();
  }
}
