package site.aster.identity;

import com.fasterxml.jackson.databind.JsonNode;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.time.Duration;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CompletableFuture;
import site.aster.ffi.DataPlane;
import site.aster.ffi.IrohEventKind;
import site.aster.ffi.IrohException;
import site.aster.ffi.IrohLibrary;
import site.aster.handle.IrohConnection;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.upcall.UpcallNative;

/**
 * Calls {@code aster.ops.NodeConfig} on the server at the other end of an {@code aster/1}
 * connection (Rust {@code NodeConfigClient}; {@code aster_node_config_call}, C ABI 7.7). Every
 * method needs {@link GatewayIdentity#CONFIG_ADMIN_ROLE} from this node's own Enrollment
 * credential; a refused or failed call completes with an {@link RpcError}. Get one from {@code
 * AsterClient.nodeConfig(target)}.
 */
public final class NodeConfigClient {

  /** {@code aster_node_config_method_t}. */
  static final int GET = 0;

  static final int SET = 1;
  static final int CLEAR = 2;
  static final int ACK = 3;

  private static final MethodHandle CALL =
      UpcallNative.handle(
          "aster_node_config_call",
          FunctionDescriptor.of(
              ValueLayout.JAVA_INT,
              ValueLayout.JAVA_LONG,
              ValueLayout.JAVA_LONG,
              ValueLayout.JAVA_INT,
              IrohLibrary.ASTER_BYTES,
              ValueLayout.JAVA_LONG,
              ValueLayout.ADDRESS));

  private final IrohConnection connection;
  private final Duration deadline;

  /** A client over {@code connection} (an {@code aster/1} connection), with no deadline. */
  public NodeConfigClient(IrohConnection connection) {
    this(connection, null);
  }

  private NodeConfigClient(IrohConnection connection, Duration deadline) {
    this.connection = Objects.requireNonNull(connection, "connection");
    this.deadline = deadline;
  }

  /** A copy whose calls carry {@code deadline} ({@code null} or non-positive: none). */
  public NodeConfigClient withDeadline(Duration deadline) {
    return new NodeConfigClient(
        connection,
        deadline == null || deadline.isNegative() || deadline.isZero() ? null : deadline);
  }

  /** The record and every overridable setting's effective value. */
  public CompletableFuture<NodeConfigView> get() {
    return call(GET, new LinkedHashMap<>())
        .thenApply(r -> IdentityJson.MAPPER.convertValue(r, NodeConfigView.class));
  }

  /** Replace the record; completes once the new generation is in effect. */
  public CompletableFuture<NodeConfigAck> set(NodeConfigSetRequest request) {
    Objects.requireNonNull(request, "request");
    return call(SET, request.toJson()).thenApply(NodeConfigClient::ack);
  }

  /**
   * Revert the node to its file configuration (a tombstone with the next generation); completes
   * once that is in effect. {@code expectedGeneration} 0 is unconditional.
   */
  public CompletableFuture<NodeConfigAck> clear(long expectedGeneration) {
    Map<String, Object> req = new LinkedHashMap<>();
    req.put("expected_generation", expectedGeneration);
    return call(CLEAR, req).thenApply(NodeConfigClient::ack);
  }

  /**
   * Whether {@code generation} is stored and in effect, waiting up to {@code timeout} ({@code null}
   * or zero = 10 s, at most 60 s) for it to be applied.
   */
  public CompletableFuture<NodeConfigAck> ack(long generation, Duration timeout) {
    Map<String, Object> req = new LinkedHashMap<>();
    req.put("generation", generation);
    req.put("timeout_ms", timeout == null ? 0L : Math.max(0L, timeout.toMillis()));
    return call(ACK, req).thenApply(NodeConfigClient::ack);
  }

  private static NodeConfigAck ack(JsonNode r) {
    return IdentityJson.MAPPER.convertValue(r, NodeConfigAck.class);
  }

  private CompletableFuture<JsonNode> call(int method, Map<String, Object> request) {
    if (deadline != null) {
      long ms = deadline.toMillis() + (deadline.toNanosPart() % 1_000_000 != 0 ? 1 : 0);
      request.put("deadline_ms", ms);
    }
    byte[] body = IdentityJson.encode(request);
    long rt = connection.runtime().nativeHandle();
    long conn = connection.nativeHandle();
    return UpcallNative.submit(
            connection.runtime(),
            "aster_node_config_call",
            (a, op) -> (int) CALL.invoke(rt, conn, method, DataPlane.bytes(a, body), 0L, op))
        .thenApply(
            r -> {
              if (r.event().kind() != IrohEventKind.NODE_CONFIG_RESULT) {
                throw new IrohException(
                    "aster.ops.NodeConfig: unexpected event " + r.event().kind());
              }
              JsonNode res;
              try {
                res = IdentityJson.MAPPER.readTree(r.data());
              } catch (java.io.IOException e) {
                throw new IrohException("aster.ops.NodeConfig: malformed result: " + e);
              }
              int code = res.path("code").asInt(0);
              if (code != 0) {
                throw new RpcError(StatusCode.fromValue(code), res.path("message").asText(""));
              }
              return res.path("result");
            });
  }
}
