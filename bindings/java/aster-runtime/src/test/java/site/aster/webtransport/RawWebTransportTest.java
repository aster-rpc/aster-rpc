package site.aster.webtransport;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import site.aster.handle.IrohRuntime;
import site.aster.server.TlsCertificates;
import site.aster.upcall.*;

@Timeout(40)
final class RawWebTransportTest {
  static byte[] bytes(String value) {
    return value.getBytes(StandardCharsets.UTF_8);
  }

  static <T> T get(CompletableFuture<T> future) throws Exception {
    return future.get(15, TimeUnit.SECONDS);
  }

  static RawWebTransport.Config config() {
    var cert = TlsCertificates.generateWebTransportCertificate("java-raw", List.of("127.0.0.1"));
    return new RawWebTransport.Config(
        "127.0.0.1:0", cert.certificatePem(), cert.privateKeyPem(), "/raw");
  }

  static Path root() {
    Path p = Path.of("").toAbsolutePath();
    while (p != null && !Files.exists(p.resolve("tests/python/raw_wt_peer.py"))) p = p.getParent();
    return Objects.requireNonNull(p, "repository fixture missing");
  }

  static Process peer(RawWebTransport.Listener listener, String scenario, String suffix, int status)
      throws Exception {
    Path root = root();
    String python = System.getenv("ASTER_TEST_PYTHON");
    if (python == null || python.isBlank())
      python =
          Files.exists(root.resolve(".venv/bin/python"))
              ? root.resolve(".venv/bin/python").toString()
              : "python3";
    return new ProcessBuilder(
            python,
            root.resolve("tests/python/raw_wt_peer.py").toString(),
            "--url",
            "https://" + listener.addresses().get("quic_addr").asText() + "/raw" + suffix,
            "--scenario",
            scenario,
            "--expected-status",
            Integer.toString(status))
        .redirectErrorStream(true)
        .start();
  }

  static void passed(Process process) throws Exception {
    assertTrue(process.waitFor(20, TimeUnit.SECONDS), "peer timed out");
    String output = new String(process.getInputStream().readAllBytes(), StandardCharsets.UTF_8);
    assertEquals(0, process.exitValue(), output);
    assertTrue(output.contains("PASS"), output);
  }

  static byte[] all(RawWebTransport.Stream stream) throws Exception {
    var out = new java.io.ByteArrayOutputStream();
    for (; ; ) {
      var read = get(stream.read(4096));
      out.write(read.bytes());
      if (read.eof()) return out.toByteArray();
    }
  }

  @Test
  void echoStreamsDatagramsPriorityAndAck() throws Exception {
    assertEquals(104, RawWebTransport.CONFIG_LAYOUT.byteSize());
    try (var runtime = IrohRuntime.create();
        var listener = RawWebTransport.startPublic(runtime, config())) {
      Process peer = peer(listener, "echo", "", 200);
      try (var session = get(listener.accept());
          var control = get(session.acceptBidirectional())) {
        assertTrue(control.hasSend());
        assertTrue(control.hasReceive());
        assertArrayEquals(bytes("control"), all(control));
        get(control.setPriority(-31));
        assertEquals(OptionalInt.of(-31), get(control.priority()));
        get(control.write(bytes("CONTROL")));
        get(control.finish());
        assertTrue(get(control.transportAcked()));
        try (var incoming = get(session.acceptUnidirectional())) {
          assertFalse(incoming.hasSend());
          assertArrayEquals(bytes("input"), all(incoming));
        }
        try (var outgoing = get(session.openUnidirectional())) {
          assertFalse(outgoing.hasReceive());
          get(outgoing.write(bytes("frame")));
          get(outgoing.finish());
          assertTrue(get(outgoing.transportAcked()));
        }
        assertArrayEquals(bytes("feedback"), get(session.receiveDatagram()));
        session.sendDatagram(bytes("reply"));
        assertTrue(session.maxDatagramSize().isPresent());
        assertTrue(session.stats().has("ack_rx"));
        assertTrue(session.info().has("connection_id"));
        passed(peer);
      } finally {
        peer.destroyForcibly();
      }
    }
  }

  @Test
  void cancelBlockedAcceptAndReadResetAndActiveClose() throws Exception {
    try (var runtime = IrohRuntime.create();
        var listener = RawWebTransport.startPublic(runtime, config())) {
      var cancelled = listener.accept();
      assertTrue(cancelled.cancel(true));
      assertThrows(CancellationException.class, cancelled::join);
      Process peer = peer(listener, "lifecycle", "", 200);
      try (var session = get(listener.accept());
          var first = get(session.acceptBidirectional());
          var reset = get(session.acceptBidirectional())) {
        assertArrayEquals(bytes("pending"), get(first.read(4096)).bytes());
        var blocked = first.read(4096);
        assertTrue(blocked.cancel(true));
        assertArrayEquals(bytes("reset-me"), get(reset.read(4096)).bytes());
        get(reset.reset(7));
        assertFalse(get(reset.transportAcked()));
        assertArrayEquals(new byte[0], get(session.receiveDatagram()));
        assertArrayEquals(bytes("lifecycle"), get(session.receiveDatagram()));
        session.sendDatagram(bytes("continue"));
        assertArrayEquals(bytes("-tail"), all(first));
        try (var outgoing = get(session.openBidirectional())) {
          get(outgoing.write(bytes("server-bi")));
          get(outgoing.finish());
          assertArrayEquals(bytes("SERVER-BI"), all(outgoing));
          assertTrue(get(outgoing.transportAcked()));
        }
        session.sendDatagram(bytes("done"));
        assertArrayEquals(bytes("ready-close"), get(session.receiveDatagram()));
        var close = getReadyClosed(session);
        var pending = session.acceptUnidirectional();
        session.close(73, bytes("done"));
        assertTrue(get(close).has("kind"));
        assertThrows(ExecutionException.class, () -> get(pending));
        passed(peer);
      } finally {
        peer.destroyForcibly();
      }
    }
  }

  static CompletableFuture<com.fasterxml.jackson.databind.JsonNode> getReadyClosed(
      RawWebTransport.Session session) {
    return session.closed();
  }

  @Test
  void foreignAuthDeniesBeforeUpgradeAndExactPath() throws Exception {
    try (var runtime = IrohRuntime.create();
        var table = Upcalls.create(runtime)) {
      table.serveRpcAuthenticator(
          (request, invocation) -> {
            assertEquals("webtransport", request.service());
            assertEquals("CONNECT", request.method());
            assertEquals("/raw", request.metadata().get("aster.webtransport.path"));
            assertEquals("ticket=opaque%2Braw", request.metadata().get("aster.webtransport.query"));
            return Ucv1.RpcAuthResponse.rejected();
          });
      try (var listener = RawWebTransport.start(runtime, table, config())) {
        Process peer = peer(listener, "connect", "?ticket=opaque%2Braw", 401);
        try {
          passed(peer);
        } finally {
          peer.destroyForcibly();
        }
      }
    }
  }

  @Test
  void acceptedForeignIdentityRemainsOnSession() throws Exception {
    try (var runtime = IrohRuntime.create();
        var table = Upcalls.create(runtime)) {
      table.serveRpcAuthenticator(
          (request, invocation) ->
              new Ucv1.RpcAuthResponse(true, "alice", Map.of("role", "publisher")));
      try (var listener = RawWebTransport.start(runtime, table, config())) {
        Process peer = peer(listener, "connect", "", 200);
        try (var session = get(listener.accept())) {
          assertEquals("alice", session.info().get("principal").asText());
          assertEquals("publisher", session.info().get("attributes").get("role").asText());
          passed(peer);
        } finally {
          peer.destroyForcibly();
        }
      }
    }
  }
}
