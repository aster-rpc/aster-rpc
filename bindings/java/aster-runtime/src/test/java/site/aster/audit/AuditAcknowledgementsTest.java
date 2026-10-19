package site.aster.audit;

import static org.junit.jupiter.api.Assertions.*;

import com.fasterxml.jackson.databind.JsonNode;
import com.sun.net.httpserver.HttpServer;
import java.math.BigInteger;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import site.aster.handle.IrohRuntime;

/** ABI 7.1: the audit outbox acknowledgement log (page, wait, watch) and its config bounds. */
final class AuditAcknowledgementsTest {
  @TempDir Path directory;

  private static <T> T get(CompletableFuture<T> future) throws Exception {
    return future.get(30, TimeUnit.SECONDS);
  }

  private static AuditOutboxException failure(CompletableFuture<?> future) {
    var error = assertThrows(ExecutionException.class, () -> get(future));
    return assertInstanceOf(AuditOutboxException.class, error.getCause());
  }

  /** Acknowledges every batch by echoing its digest; stores nothing. */
  private static HttpServer ackingReceiver() throws Exception {
    HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
    server.createContext(
        "/audit",
        exchange -> {
          try (exchange) {
            JsonNode batch = AuditJson.MAPPER.readTree(exchange.getRequestBody());
            byte[] body =
                AuditJson.MAPPER.writeValueAsBytes(
                    java.util.Map.of(
                        "version",
                        1,
                        "batch_id",
                        batch.path("batch_id").asText(),
                        "durable",
                        true));
            exchange.getResponseHeaders().set("Content-Type", "application/json");
            exchange.sendResponseHeaders(200, body.length);
            exchange.getResponseBody().write(body);
          }
        });
    server.start();
    return server;
  }

  @Test
  void acknowledgementLogPagesWaitsAndWatches() throws Exception {
    HttpServer receiver = ackingReceiver();
    String endpoint = "http://127.0.0.1:" + receiver.getAddress().getPort() + "/audit";
    var config =
        new AuditOutbox.Config(directory.resolve("outbox"), new AuditOutbox.Destination(endpoint))
            .withAcknowledgementLog(1000L, null);
    assertTrue(
        new String(AuditJson.config(config), StandardCharsets.UTF_8)
            .contains("\"acknowledgement_log_max_entries\":1000"));
    try (IrohRuntime runtime = IrohRuntime.create()) {
      AuditOutbox outbox = get(AuditOutbox.open(runtime, config));
      try {
        assertEquals(
            AuditOutboxException.Kind.INVALID_EVENT, failure(outbox.acknowledgements(0, 0)).kind());
        assertEquals(
            AuditOutboxException.Kind.INVALID_EVENT,
            failure(outbox.waitAcknowledgements(0, 1, Duration.ofHours(2))).kind());
        // short-wait-ok: nothing is acknowledged yet; the wait must expire empty
        assertEquals(List.of(), get(outbox.waitAcknowledgements(0, 10, Duration.ofMillis(20))));

        LinkedBlockingQueue<AuditOutbox.Acknowledgement> watched = new LinkedBlockingQueue<>();
        AuditOutbox.Watch watch = outbox.watchAcknowledgements(0, watched::add);
        List<String> ids = new ArrayList<>();
        for (int i = 0; i < 3; i++) {
          ids.add(
              get(
                  outbox.submit(
                      new AuditOutbox.Event(
                          System.currentTimeMillis(),
                          "java.ack",
                          AuditJson.MAPPER.createObjectNode().put("n", i)))));
        }
        get(outbox.flush(Duration.ofSeconds(30)));
        for (int i = 0; i < ids.size(); i++) {
          AuditOutbox.Acknowledgement ack = watched.poll(30, TimeUnit.SECONDS);
          assertNotNull(ack, "watch delivered acknowledgement " + i);
          assertEquals(ids.get(i), ack.eventId());
          assertEquals(i + 1, ack.cursor());
          assertEquals(64, ack.batchId().length());
          assertTrue(ack.acknowledgedAt().toEpochMilli() > 0);
        }
        watch.close();
        get(watch.done());

        List<AuditOutbox.Acknowledgement> all = get(outbox.acknowledgements(0, 100));
        assertEquals(3, all.size());
        for (int i = 0; i < all.size(); i++) {
          assertEquals(ids.get(i), all.get(i).eventId());
          assertEquals(i + 1, all.get(i).sequence());
        }
        List<AuditOutbox.Acknowledgement> tail = get(outbox.acknowledgements(2, 10));
        assertEquals(1, tail.size());
        assertEquals(3, tail.getFirst().cursor());
        assertEquals(BigInteger.ZERO, get(outbox.status()).pendingRecords());
      } finally {
        get(outbox.closeAsync());
      }

      var tooMany =
          new AuditOutbox.Config(
                  directory.resolve("other"),
                  new AuditOutbox.Destination("http://127.0.0.1:1/audit"))
              .withAcknowledgementLog(1_000_001L, null);
      assertEquals(
          AuditOutboxException.Kind.INVALID_CONFIG,
          failure(AuditOutbox.open(runtime, tooMany)).kind());
    } finally {
      receiver.stop(0);
    }
  }
}
