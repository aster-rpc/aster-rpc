package site.aster.client;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import site.aster.handle.IrohConnection;

/**
 * Client-side handle for a server-side SESSION-scoped service instance (multiplexed-streams spec §6
 * / §7.5).
 *
 * <p>A {@code ClientSession} pins one {@link IrohConnection} and one client-allocated {@code
 * sessionId}; every call routed through it carries that {@code sessionId} on its {@code
 * StreamHeader}, so the server resolves the same session-scoped instance on every invocation.
 *
 * <p>Sessions are created implicitly on the server the first time it sees a stream with the
 * allocated {@code sessionId}; there is no explicit "open session" RPC. Sessions are reaped
 * server-side when the underlying QUIC connection drops, so {@link #close()} here does no wire
 * traffic — it's a no-op kept for {@link AutoCloseable} ergonomics.
 *
 * <p>Threading: a {@code ClientSession} is safe to share across threads. Concurrent calls on the
 * same session multiplex over the connection's per-session stream pool (default {@code
 * session_pool_size=1}, which serialises them; raise it to allow parallelism — see spec §3 / §9).
 * Server-side dispatch into the session instance is the user's responsibility to make thread-safe.
 */
public final class ClientSession implements AutoCloseable {

  private final AsterClient client;
  private final IrohConnection connection;
  private final int sessionId;

  ClientSession(AsterClient client, IrohConnection connection, int sessionId) {
    this.client = client;
    this.connection = connection;
    this.sessionId = sessionId;
  }

  /** Server-allocated id this session routes through. Useful for logs and tests. */
  public int sessionId() {
    return sessionId;
  }

  /** The underlying connection. Useful for tests that want to inspect peer identity. */
  public IrohConnection connection() {
    return connection;
  }

  /** Make a unary call into this session. */
  public <Req, Resp> CompletableFuture<Resp> call(
      String service, String method, Req request, Class<Resp> responseType) {
    return client.runUnary(connection, sessionId, service, method, request, responseType);
  }

  /** Make a server-streaming call into this session. */
  public <Req, Resp> CompletableFuture<List<Resp>> callServerStream(
      String service, String method, Req request, Class<Resp> responseType) {
    return client.runServerStream(connection, sessionId, service, method, request, responseType);
  }

  /** Make a client-streaming call into this session. */
  public <Req, Resp> CompletableFuture<Resp> callClientStream(
      String service, String method, Iterable<Req> requests, Class<Resp> responseType) {
    List<Req> materialized = materialize(requests);
    if (materialized.isEmpty()) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException(
              "callClientStream requires at least one request frame; the wire format delivers"
                  + " the first frame inline with the call to bootstrap the dispatcher"));
    }
    return client.runClientStream(
        connection, sessionId, service, method, materialized, responseType);
  }

  /**
   * Make a buffered bidi-streaming call into this session. All requests are sent before any
   * response is read. Use {@link #openBidiStream} for true interleaving.
   */
  public <Req, Resp> CompletableFuture<List<Resp>> callBidiStream(
      String service, String method, Iterable<Req> requests, Class<Resp> responseType) {
    List<Req> materialized = materialize(requests);
    if (materialized.isEmpty()) {
      return CompletableFuture.failedFuture(
          new IllegalArgumentException(
              "callBidiStream requires at least one request frame; the wire format delivers"
                  + " the first frame inline with the call to bootstrap the dispatcher"));
    }
    return client.runBidiBuffered(
        connection, sessionId, service, method, materialized, responseType);
  }

  /** Open a true interleaved bidi-streaming call into this session. */
  public <Req, Resp> BidiCall<Req, Resp> openBidiStream(
      String service, String method, Class<Resp> responseType) {
    return client.openBidiStreamOn(connection, sessionId, service, method, responseType);
  }

  /**
   * Best-effort close hook. There is no wire traffic — sessions are reaped server-side when the
   * underlying QUIC connection drops (spec §7.5). Provided so callers can use this in
   * try-with-resources.
   */
  @Override
  public void close() {
    // Intentional no-op: server-side reap on connection close handles cleanup (spec §7.5).
  }

  private static <T> List<T> materialize(Iterable<T> items) {
    List<T> list = new ArrayList<>();
    items.forEach(list::add);
    return list;
  }
}
