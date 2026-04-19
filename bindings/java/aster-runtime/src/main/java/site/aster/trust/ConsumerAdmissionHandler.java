package site.aster.trust;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.function.BiConsumer;
import java.util.function.Supplier;
import site.aster.contract.Capabilities;
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
  private static final ObjectMapper CRED_MAPPER = new ObjectMapper();

  private final Supplier<List<ServiceSummary>> servicesSupplier;
  private final Supplier<String> registryNamespaceSupplier;
  private final BiConsumer<String, Map<String, String>> peerAttributesSink;
  private final Supplier<Boolean> allowUnenrolledSupplier;
  private final ExecutorService executor;
  private volatile boolean closed;

  public ConsumerAdmissionHandler(Supplier<List<ServiceSummary>> services) {
    this(services, () -> "", null, () -> true);
  }

  /**
   * Construct the handler with a supplier for the registry namespace hex. The namespace is the
   * 64-char hex doc-id of the producer's registry doc; consumers use it to {@code
   * join_and_subscribe_namespace} and sync published contract manifests (spec §3.2).
   */
  public ConsumerAdmissionHandler(
      Supplier<List<ServiceSummary>> services, Supplier<String> registryNamespace) {
    this(services, registryNamespace, null, () -> true);
  }

  public ConsumerAdmissionHandler(
      Supplier<List<ServiceSummary>> services,
      Supplier<String> registryNamespace,
      BiConsumer<String, Map<String, String>> peerAttributesSink) {
    this(services, registryNamespace, peerAttributesSink, () -> true);
  }

  /**
   * Full constructor: {@code peerAttributesSink} is invoked on every successful admission with the
   * consumer's peer id and the attributes extracted from its credential JSON. Dev-mode trust — no
   * signature verification — so the credential is read at face value. Production verification is
   * future work (tracked by the trust-anchor inversion design).
   *
   * <p>{@code allowUnenrolledSupplier} controls the empty-credential path: when it returns {@code
   * false} the handler denies any request without a credential (mirroring Python's {@code
   * AsterConfig.allow_all_consumers=False}). Defaults to {@code true} (open-gate) in the other
   * constructors so existing dev callers keep their current behaviour.
   */
  public ConsumerAdmissionHandler(
      Supplier<List<ServiceSummary>> services,
      Supplier<String> registryNamespace,
      BiConsumer<String, Map<String, String>> peerAttributesSink,
      Supplier<Boolean> allowUnenrolledSupplier) {
    this.servicesSupplier = services;
    this.registryNamespaceSupplier = registryNamespace == null ? () -> "" : registryNamespace;
    this.peerAttributesSink = peerAttributesSink;
    this.allowUnenrolledSupplier =
        allowUnenrolledSupplier == null ? () -> true : allowUnenrolledSupplier;
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
        ConsumerAdmissionWire.Request req;
        try {
          req = ConsumerAdmissionWire.Request.fromJson(reqBytes);
        } catch (Exception parseErr) {
          byte[] denied = ConsumerAdmissionWire.Response.denied().toJsonBytes();
          stream.sendAsync(denied).get();
          stream.finishAsync().get();
          return;
        }

        boolean hasCredential = req.credentialJson != null && !req.credentialJson.isEmpty();
        if (!hasCredential && !allowUnenrolledSupplier.get()) {
          // Strict mode: deny empty credentials so unenrolled peers can't sneak in.
          byte[] denied = ConsumerAdmissionWire.Response.denied().toJsonBytes();
          stream.sendAsync(denied).get();
          stream.finishAsync().get();
          return;
        }

        // Dev-mode trust: lift the credential's attributes map into the peer store without
        // verifying the signature. This unblocks capability gating end-to-end for tests and
        // the mission-control example; production trust needs signature + expiry + root pubkey
        // verification and belongs to the trust-anchor work (future).
        if (peerAttributesSink != null && hasCredential) {
          Map<String, String> attrs = extractAttributes(req.credentialJson);
          if (!attrs.isEmpty()) {
            peerAttributesSink.accept(conn.remoteId(), attrs);
          }
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

  /**
   * Parse the inner credential JSON and extract its {@code attributes} object as a flat {@code
   * Map<String, String>}. Values that are objects or arrays are stringified via {@code toString}.
   * Matches the shape of Python's {@code consumer_cred_to_json} output / TS's {@code
   * consumerCredToJson}. Returns an empty map on any parse error.
   */
  static Map<String, String> extractAttributes(String credentialJson) {
    Map<String, String> out = new LinkedHashMap<>();
    try {
      JsonNode root = CRED_MAPPER.readTree(credentialJson);
      JsonNode attrs = root.path("attributes");
      if (attrs.isObject()) {
        Iterator<String> keys = attrs.fieldNames();
        while (keys.hasNext()) {
          String k = keys.next();
          JsonNode v = attrs.get(k);
          if (v == null || v.isNull()) continue;
          out.put(k, v.isTextual() ? v.asText() : v.toString());
        }
      }
      // Backstop: some tooling embeds the canonical role under the flat aster.role field.
      JsonNode flat = root.path(Capabilities.ROLE_ATTRIBUTE);
      if (flat.isTextual() && !out.containsKey(Capabilities.ROLE_ATTRIBUTE)) {
        out.put(Capabilities.ROLE_ATTRIBUTE, flat.asText());
      }
    } catch (Exception ignored) {
      // Malformed credential → empty attrs → client proceeds with no roles (gating denies them).
    }
    return out;
  }

  @Override
  public void close() {
    closed = true;
    executor.shutdownNow();
  }
}
