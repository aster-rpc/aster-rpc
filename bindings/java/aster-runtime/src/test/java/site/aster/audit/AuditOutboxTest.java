package site.aster.audit;

import static org.junit.jupiter.api.Assertions.*;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.math.BigInteger;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.time.Duration;
import java.util.HexFormat;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import site.aster.handle.IrohRuntime;

/** Native binding parity; the Rust suite owns the detailed storage/delivery fault matrix. */
final class AuditOutboxTest {
  @TempDir Path directory;

  private static String python() {
    String configured = System.getenv("ASTER_TEST_PYTHON");
    if (configured != null && !configured.isBlank()) return configured;
    return System.getProperty("os.name").startsWith("Windows") ? "python" : "python3";
  }

  private static <T> T get(CompletableFuture<T> future) throws Exception {
    return future.get(15, TimeUnit.SECONDS);
  }

  private static AuditOutboxException failure(CompletableFuture<?> future) {
    var error = assertThrows(ExecutionException.class, () -> get(future));
    return assertInstanceOf(AuditOutboxException.class, error.getCause());
  }

  private static Path receiverScript() {
    Path root = Path.of("").toAbsolutePath();
    while (root != null && !root.resolve("tests/audit_outbox/receiver.py").toFile().exists())
      root = root.getParent();
    if (root == null) throw new IllegalStateException("audit receiver fixture not found");
    return root.resolve("tests/audit_outbox/receiver.py");
  }

  private record Receiver(Process process, String endpoint) implements AutoCloseable {
    @Override
    public void close() throws Exception {
      process.destroy();
      assertTrue(process.waitFor(10, TimeUnit.SECONDS));
    }
  }

  private Receiver receiver(int port) throws Exception {
    Process process =
        new ProcessBuilder(
                python(),
                receiverScript().toString(),
                "--database",
                directory.resolve("receiver.sqlite").toString(),
                "--port",
                Integer.toString(port))
            .start();
    String endpoint =
        new BufferedReader(new InputStreamReader(process.getInputStream(), StandardCharsets.UTF_8))
            .readLine();
    assertNotNull(endpoint, "receiver starts");
    return new Receiver(process, endpoint);
  }

  @Test
  void submitReopenPersistentReceiverFlushAndTypedError() throws Exception {
    String endpoint;
    int port;
    try (Receiver fixture = receiver(0)) {
      endpoint = fixture.endpoint();
      port = URI.create(endpoint).getPort();
    }
    var config =
        new AuditOutbox.Config(
            directory.resolve("outbox"),
            new AuditOutbox.Destination(endpoint),
            BigInteger.valueOf(256L * 1024 * 1024),
            1024);
    try (IrohRuntime runtime = IrohRuntime.create()) {
      var outbox = get(AuditOutbox.open(runtime, config));
      String eventId;
      try {
        eventId =
            get(
                outbox.submit(
                    new AuditOutbox.Event(
                        BigInteger.ONE.shiftLeft(64).subtract(BigInteger.ONE),
                        "intent",
                        AuditJson.MAPPER.createObjectNode().put("count", 123))));
        assertEquals(
            AuditOutboxException.Kind.EVENT_TOO_LARGE,
            failure(
                    outbox.submit(
                        new AuditOutbox.Event(
                            1,
                            "oversized",
                            AuditJson.MAPPER.createObjectNode().put("data", "x".repeat(2048)))))
                .kind());
        assertEquals(BigInteger.ONE, get(outbox.status()).pendingRecords());
        assertEquals(
            AuditOutboxException.Kind.TIMEOUT, failure(outbox.flush(Duration.ofMillis(10))).kind());
      } finally {
        get(outbox.closeAsync());
      }
      try (Receiver fixture = receiver(port)) {
        var reopened = get(AuditOutbox.open(runtime, config));
        try {
          get(reopened.flush(Duration.ofSeconds(10)));
          assertEquals(BigInteger.ZERO, get(reopened.status()).pendingRecords());
        } finally {
          get(reopened.closeAsync());
        }
      }
      Process query =
          new ProcessBuilder(
                  python(),
                  "-c",
                  "import sqlite3,sys,base64,json; db=sqlite3.connect(sys.argv[1]); print(json.dumps([[k,base64.b64encode(v).decode()] for k,v in db.execute('SELECT id,envelope FROM events')]))",
                  directory.resolve("receiver.sqlite").toString())
              .start();
      var events = AuditJson.MAPPER.readTree(query.getInputStream());
      assertTrue(query.waitFor(10, TimeUnit.SECONDS));
      assertEquals(0, query.exitValue());
      assertEquals(1, events.size());
      assertEquals(eventId, events.get(0).get(0).asText());
      byte[] raw = java.util.Base64.getDecoder().decode(events.get(0).get(1).asText());
      assertEquals(
          eventId.substring(eventId.lastIndexOf(':') + 1),
          HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(raw)));
      assertTrue(new String(raw, StandardCharsets.UTF_8).contains("18446744073709551615"));
    }
  }

  @Test
  void clonesShareWorkersAndCancelledOpenEventuallyReleasesLock() throws Exception {
    String endpoint;
    try (Receiver fixture = receiver(0)) {
      endpoint = fixture.endpoint();
    }
    var config =
        new AuditOutbox.Config(directory.resolve("outbox"), new AuditOutbox.Destination(endpoint));
    try (IrohRuntime runtime = IrohRuntime.create()) {
      var opening = AuditOutbox.open(runtime, config);
      if (!opening.cancel(false)) get(opening).closeAsync().get(15, TimeUnit.SECONDS);
      AuditOutbox outbox = null;
      for (int attempt = 0; attempt < 100; attempt++) {
        try {
          outbox = get(AuditOutbox.open(runtime, config));
          break;
        } catch (ExecutionException error) {
          assertEquals(
              AuditOutboxException.Kind.STORAGE,
              assertInstanceOf(AuditOutboxException.class, error.getCause()).kind());
          Thread.sleep(10);
        }
      }
      assertNotNull(outbox, "canceled open releases its late handle");
      var clone = outbox.copy();
      try {
        get(clone.submit(new AuditOutbox.Event(1, "intent", AuditJson.MAPPER.createObjectNode())));
        assertEquals(BigInteger.ONE, get(outbox.status()).pendingRecords());
        get(outbox.reconfigure(new AuditOutbox.Destination(endpoint, null, "Bearer rotated")));
        assertFalse(get(outbox.status()).toString().contains("Bearer rotated"));
        var waiting = outbox.flush(Duration.ofSeconds(10));
        assertTrue(waiting.cancel(false));
        get(clone.closeAsync());
        assertEquals(
            AuditOutboxException.Kind.CLOSED,
            failure(
                    outbox.submit(
                        new AuditOutbox.Event(2, "closed", AuditJson.MAPPER.createObjectNode())))
                .kind());
      } finally {
        get(clone.closeAsync());
        get(outbox.closeAsync());
      }
      var reopened = get(AuditOutbox.open(runtime, config));
      try {
        assertEquals(BigInteger.ONE, get(reopened.status()).pendingRecords());
      } finally {
        get(reopened.closeAsync());
      }
    }
  }
}
