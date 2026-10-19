package site.aster.expose;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static site.aster.expose.ExposeE2ETest.await;
import static site.aster.expose.ExposeE2ETest.pattern;
import static site.aster.expose.ExposeE2ETest.waitFor;

import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import org.junit.jupiter.api.Test;
import site.aster.exception.PermissionDeniedException;
import site.aster.expose.ExposeE2ETest.Rig;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohStatus;
import site.aster.upcall.Ucv1.HttpHeader;

/**
 * C ABI 4.3 streaming consumer relay end to end: full duplex, an early 413 releasing the Java
 * source, a Java source failing before the head, stable failure codes, readiness.
 *
 * <p>These are wiring tests. The relay's behaviour is tested once in Rust, in {@code
 * ffi/tests/expose.rs}: closing early, a clean end and a source failing after the head ({@code
 * relay_stream_keeps_the_full_duplex_contract}), aborting before the head ({@code
 * relay_stream_failures_have_stable_codes}) and argument checks ({@code
 * relay_stream_validates_its_arguments}).
 */
final class RelayStreamE2ETest {

  private static final long T = 30;
  private static final List<HttpHeader> NO_HEADERS = List.of();

  /** Handlers: /echo (inline), /duplex (echo while reading), /early (413), /gate (waits). */
  private static final class Server {
    final List<String> log = new CopyOnWriteArrayList<>();
    final CountDownLatch gate = new CountDownLatch(1);

    void install(Rig rig, ExposeAuthorizer authorize) {
      rig.expose.exposeHttp(
          "app",
          req -> {
            switch (req.uri()) {
              case "/early":
                log.add("start /early");
                return HttpResponse.text(413, "too big");
              case "/gate":
                log.add("start /gate");
                gate.await();
                return HttpResponse.text(200, "released");
              case "/duplex":
                log.add("start /duplex");
                return HttpResponse.streaming(
                    200,
                    out -> {
                      byte[] b = new byte[8192];
                      try {
                        int n;
                        while ((n = req.body().read(b)) >= 0) {
                          out.write(b, 0, n);
                        }
                      } catch (HttpBodyException e) {
                        log.add("read err " + e.kind());
                        throw e;
                      }
                      log.add("duplex done");
                    });
              default:
                log.add("start " + req.uri());
                return HttpResponse.of(200, req.readBody());
            }
          },
          authorize);
    }

    boolean has(java.util.function.Predicate<String> p) {
      return log.stream().anyMatch(p);
    }
  }

  /** An endless source of 64 KiB chunks that records when it is closed. */
  private static final class Endless extends InputStream {
    final AtomicLong produced = new AtomicLong();
    final CountDownLatch closed = new CountDownLatch(1);

    @Override
    public int read() {
      produced.incrementAndGet();
      return 9;
    }

    @Override
    public int read(byte[] b, int off, int len) {
      int n = Math.min(len, 64 * 1024);
      java.util.Arrays.fill(b, off, off + n, (byte) 9);
      produced.addAndGet(n);
      return n;
    }

    @Override
    public void close() {
      closed.countDown();
    }
  }

  private static Throwable rootOf(ExecutionException e) {
    return e.getCause();
  }

  @Test
  void streamsAMegabyteBothWaysFullDuplex() throws Exception {
    try (Rig rig = new Rig()) {
      new Server().install(rig, null);
      byte[] up = pattern(1_000_000);
      try (RelayResponse r =
          await(
              Expose.relayStream(
                  rig.conn,
                  "app",
                  "POST",
                  "/duplex",
                  NO_HEADERS,
                  new ByteArrayInputStream(up),
                  RelayOptions.defaults().withMaxChunk(4096).withUploadCapacity(2)))) {
        assertEquals(200, r.status());
        assertArrayEquals(up, r.readBody(-1));
        assertTrue(r.ended());
      }
      waitFor("exchanges to close", () -> rig.expose.bodies().openCount() == 0);
    }
  }

  @Test
  void anEarly413ArrivesWhileTheUploadIsStillGoing() throws Exception {
    try (Rig rig = new Rig()) {
      new Server().install(rig, null);
      Endless source = new Endless();
      try (RelayResponse r =
          await(
              Expose.relayStream(
                  rig.conn, "app", "PUT", "/early", NO_HEADERS, source, RelayOptions.defaults()))) {
        assertEquals(413, r.status());
        assertEquals("too big", new String(r.readBody(-1), StandardCharsets.UTF_8));
      }
      // The handler never reads: the upload fails CANCELLED and the source is released.
      assertTrue(
          source.closed.await(T, TimeUnit.SECONDS), "the upload stops and closes its source");
    }
  }

  @Test
  void aSourceThatFailsBeforeTheHeadFailsTheCall() throws Exception {
    try (Rig rig = new Rig()) {
      Server server = new Server();
      server.install(rig, null);
      IOException boom = new IOException("source broke");
      InputStream failing =
          new InputStream() {
            @Override
            public int read() throws IOException {
              throw boom;
            }
          };
      ExecutionException e =
          org.junit.jupiter.api.Assertions.assertThrows(
              ExecutionException.class,
              () ->
                  await(
                      Expose.relayStream(
                          rig.conn,
                          "app",
                          "PUT",
                          "/gate",
                          NO_HEADERS,
                          failing,
                          RelayOptions.defaults())));
      assertSame(boom, rootOf(e));
      server.gate.countDown();
    }
  }

  @Test
  void deniedUnknownAndClosedRelaysHaveStableStatuses() throws Exception {
    try (Rig rig = new Rig()) {
      Server server = new Server();
      java.util.concurrent.atomic.AtomicBoolean allow = new AtomicBoolean(false);
      server.install(rig, (s, p, m) -> allow.get());

      // Denied: PERMISSION_DENIED for the streaming relay ...
      ExecutionException denied =
          assertThrows(
              ExecutionException.class,
              () ->
                  await(
                      Expose.relayStream(
                          rig.conn,
                          "app",
                          "PUT",
                          "/echo",
                          NO_HEADERS,
                          null,
                          RelayOptions.defaults())));
      IrohException de = assertInstanceOf(IrohException.class, rootOf(denied));
      assertEquals(IrohStatus.PERMISSION_DENIED, de.status());
      assertInstanceOf(PermissionDeniedException.class, de);

      // ... and for the buffered one (the status was INTERNAL before ABI 4.3 / status 15).
      ExecutionException buffered =
          assertThrows(
              ExecutionException.class,
              () -> await(Expose.relayRequest(rig.conn, "app", "GET", "/", null)));
      IrohException be = assertInstanceOf(IrohException.class, rootOf(buffered));
      assertEquals(IrohStatus.PERMISSION_DENIED, be.status());

      // Unknown service: NOT_FOUND.
      ExecutionException unknown =
          assertThrows(
              ExecutionException.class,
              () ->
                  await(
                      Expose.relayStream(
                          rig.conn,
                          "nope",
                          "PUT",
                          "/",
                          NO_HEADERS,
                          null,
                          RelayOptions.defaults())));
      assertEquals(IrohStatus.NOT_FOUND, ((IrohException) rootOf(unknown)).status());
      ExecutionException unknownBuffered =
          assertThrows(
              ExecutionException.class,
              () -> await(Expose.relayRequest(rig.conn, "nope", "GET", "/", null)));
      assertEquals(IrohStatus.NOT_FOUND, ((IrohException) rootOf(unknownBuffered)).status());

      // Allowed again: it works.
      allow.set(true);
      try (RelayResponse r =
          await(
              Expose.relayStream(
                  rig.conn,
                  "app",
                  "PUT",
                  "/echo",
                  NO_HEADERS,
                  new ByteArrayInputStream("hi".getBytes(StandardCharsets.UTF_8)),
                  RelayOptions.defaults()))) {
        assertEquals("hi", new String(r.readBody(-1), StandardCharsets.UTF_8));
      }

      // The connection goes away while the head is awaited: ALREADY_CLOSED.
      CompletableFuture<RelayResponse> pending =
          Expose.relayStream(
              rig.conn, "app", "PUT", "/gate", NO_HEADERS, null, RelayOptions.defaults());
      waitFor("handler started", () -> server.has(l -> l.equals("start /gate")));
      rig.conn.close();
      ExecutionException closed =
          assertThrows(ExecutionException.class, () -> pending.get(T, TimeUnit.SECONDS));
      assertEquals(IrohStatus.ALREADY_CLOSED, ((IrohException) rootOf(closed)).status());
      server.gate.countDown();
    }
  }

  @Test
  void lowLevelCallWithReadinessAndBlockingWrite() throws Exception {
    try (Rig rig = new Rig()) {
      Server server = new Server();
      server.install(rig, null);
      Expose.RelayCall call =
          Expose.relayStreaming(
              rig.conn, "app", "POST", "/duplex", NO_HEADERS, RelayOptions.defaults());
      try (RelayUpload up = call.upload();
          RelayResponse r = await(call.response())) {
        assertEquals(0, up.ready().get(T, TimeUnit.SECONDS), "room to write");
        assertTrue(up.tryWrite("x".getBytes(StandardCharsets.UTF_8)));
        // A platform thread may use the native blocking write.
        Thread t =
            new Thread(
                () -> {
                  try {
                    up.writeBlocking(
                        "y".getBytes(StandardCharsets.UTF_8), java.time.Duration.ofSeconds(10));
                  } catch (IOException e) {
                    throw new IllegalStateException(e);
                  }
                });
        t.start();
        t.join();
        up.finish();
        // The body is ready (data or the end): the readiness event carries status code 0.
        assertEquals(0, r.ready().get(T, TimeUnit.SECONDS));
        assertEquals("xy", new String(r.readBody(-1), StandardCharsets.UTF_8));
        // Finished upload: every write would fail, which readiness reports as FINISHED.
        assertEquals(HttpBodyException.Kind.FINISHED.code, up.ready().get(T, TimeUnit.SECONDS));
      }
    }
  }

  @Test
  void statusTablesMatchTheHeader() {
    assertEquals(24, RelayOptions.LAYOUT.byteSize());
    assertThrows(IllegalArgumentException.class, () -> new RelayOptions(-1, 0));
    assertEquals(IrohStatus.UNAUTHENTICATED, IrohStatus.fromCode(14));
    assertEquals(IrohStatus.PERMISSION_DENIED, IrohStatus.fromCode(15));
    assertEquals(IrohStatus.CREDENTIAL_EXPIRED, IrohStatus.fromCode(16));
    assertEquals(IrohStatus.INTERNAL, IrohStatus.fromCode(99));
    assertEquals(IrohEventKind.HTTP_RELAY_READY, IrohEventKind.fromCode(109));
    assertEquals(10, HttpBodyException.Kind.UNKNOWN_SERVICE.code);
    assertEquals(16, HttpBodyException.Kind.UPLOAD_FAILED.code);
    assertEquals(17, HttpBodyException.Kind.BUFFER_TOO_SMALL.code);
    assertEquals(HttpBodyException.Kind.CONNECTION_CLOSED, HttpBodyException.Kind.fromCode(15));
    assertEquals(
        IrohStatus.PERMISSION_DENIED,
        IrohException.forStatus(IrohStatus.PERMISSION_DENIED, "x").status());
  }
}
