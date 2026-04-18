package site.aster.trust;

import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.function.Supplier;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohStream;
import site.aster.registry.ServiceSummary;

/**
 * Server-side handler for the {@code aster.consumer_admission} ALPN (Aster-SPEC §3.2).
 *
 * <p>Accepts a bi-directional stream on each connection routed to it, reads the {@code
 * ConsumerAdmissionRequest} JSON to end, builds a {@code ConsumerAdmissionResponse}, writes it
 * back, finishes the stream. In dev/open-gate mode every request is admitted without credential
 * checks; auth-mode verification is future work (see tasks #14/#15).
 *
 * <p>The caller (typically {@link site.aster.server.AsterServer}) drives a reactor-based accept
 * loop and invokes {@link #onConnection(IrohConnection)} for connections negotiating the admission
 * ALPN.
 */
public final class ConsumerAdmissionHandler implements AutoCloseable {

  public static final String ALPN_STRING = "aster.consumer_admission";
  public static final byte[] ALPN = ALPN_STRING.getBytes(StandardCharsets.UTF_8);

  private static final int MAX_REQUEST_BYTES = 64 * 1024;

  private final Supplier<List<ServiceSummary>> servicesSupplier;
  private final Supplier<String> registryNamespaceSupplier;
  private final ExecutorService executor;
  private volatile boolean closed;

  public ConsumerAdmissionHandler(Supplier<List<ServiceSummary>> services) {
    this(services, () -> "");
  }

  /**
   * Construct the handler with a supplier for the registry namespace hex. The namespace is the
   * 64-char hex doc-id of the producer's registry doc; consumers use it to {@code
   * join_and_subscribe_namespace} and sync published contract manifests (spec §3.2).
   */
  public ConsumerAdmissionHandler(
      Supplier<List<ServiceSummary>> services, Supplier<String> registryNamespace) {
    this.servicesSupplier = services;
    this.registryNamespaceSupplier = registryNamespace == null ? () -> "" : registryNamespace;
    this.executor = Executors.newVirtualThreadPerTaskExecutor();
  }

  /** Match a raw ALPN byte sequence against {@link #ALPN}. */
  public static boolean matches(byte[] alpn) {
    if (alpn == null || alpn.length != ALPN.length) {
      return false;
    }
    for (int i = 0; i < ALPN.length; i++) {
      if (alpn[i] != ALPN[i]) {
        return false;
      }
    }
    return true;
  }

  /**
   * Handle one inbound admission connection. Runs the read/respond/finish exchange on the handler's
   * virtual-thread executor; returns immediately.
   */
  public void onConnection(IrohConnection conn) {
    if (closed) {
      conn.close();
      return;
    }
    executor.submit(() -> handle(conn));
  }

  private void handle(IrohConnection conn) {
    try {
      IrohStream stream = conn.acceptBiAsync().get();
      try {
        byte[] reqBytes = stream.readToEndAsync(MAX_REQUEST_BYTES).get();
        // Parse purely for validation — open-gate ignores credential content but we still
        // reject a syntactically broken request so misconfigured clients see an actionable
        // denial instead of a silent admit.
        try {
          ConsumerAdmissionWire.Request.fromJson(reqBytes);
        } catch (Exception parseErr) {
          byte[] denied = ConsumerAdmissionWire.Response.denied().toJsonBytes();
          stream.sendAsync(denied).get();
          stream.finishAsync().get();
          return;
        }

        ConsumerAdmissionWire.Response resp =
            ConsumerAdmissionWire.Response.admitted(
                servicesSupplier.get(), registryNamespaceSupplier.get());
        stream.sendAsync(resp.toJsonBytes()).get();
        stream.finishAsync().get();
      } finally {
        stream.close();
      }
    } catch (Exception e) {
      // Per-connection failures must not take the server down. Log-once / metrics hook is
      // future work; for now swallow so one malformed client cannot block others.
    }
    // NOTE: do NOT call conn.close() here. Closing the QUIC connection sends CONNECTION_CLOSE
    // which terminates in-flight data before the consumer's read_to_end() can observe the
    // stream-finished signal, surfacing as "read error: connection lost" on the client. Let
    // QUIC drain the streams naturally — matches Python's handle_consumer_admission_connection
    // in bindings/python/aster/trust/consumer.py (line 355).
  }

  @Override
  public void close() {
    closed = true;
    executor.shutdownNow();
  }
}
