package site.aster.trust;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.handle.IrohStream;
import site.aster.interceptors.RpcError;
import site.aster.interceptors.StatusCode;
import site.aster.node.NodeAddr;

/**
 * Client-side handshake for the {@code aster.consumer_admission} ALPN (Aster-SPEC §3.2). Port of
 * Python's {@code AsterClient._run_admission} in {@code bindings/python/aster/runtime.py} and of
 * TypeScript's {@code performAdmission} in {@code bindings/typescript/.../trust/consumer.ts}.
 *
 * <p>Flow:
 *
 * <ol>
 *   <li>Open a QUIC connection to {@code target} on the admission ALPN
 *   <li>Open a bidi stream
 *   <li>Write {@link ConsumerAdmissionWire.Request} JSON + {@code finishAsync} on the send side
 *   <li>Read the response to end
 *   <li>Return the parsed {@link ConsumerAdmissionWire.Response}
 * </ol>
 *
 * <p>The returned response carries the admitted services (with {@code channels["rpc"]} pointing at
 * the producer's RPC endpoint) and any auth attributes the server is propagating. Callers map a
 * denied response to {@link StatusCode#PERMISSION_DENIED}; an OK response indicates the caller may
 * now open RPC connections on the {@code aster/1} ALPN.
 */
public final class ConsumerAdmissionClient {

  public static final int MAX_RESPONSE_BYTES = 64 * 1024;
  public static final int DEFAULT_TIMEOUT_SECONDS = 15;

  private ConsumerAdmissionClient() {}

  /**
   * Present {@code credentialJson} to {@code target} and return the admission response. Uses the
   * default 15s timeout per hop.
   *
   * @param endpoint local endpoint (must carry the admission ALPN in its advertised list)
   * @param target producer node address extracted from the {@code aster1…} ticket
   * @param credentialJson inner credential JSON (empty string = dev-mode / open-gate)
   * @param iidToken cloud instance identity token, or empty string
   */
  public static CompletableFuture<ConsumerAdmissionWire.Response> performAdmission(
      IrohEndpoint endpoint, NodeAddr target, String credentialJson, String iidToken) {
    return performAdmission(endpoint, target, credentialJson, iidToken, DEFAULT_TIMEOUT_SECONDS);
  }

  public static CompletableFuture<ConsumerAdmissionWire.Response> performAdmission(
      IrohEndpoint endpoint,
      NodeAddr target,
      String credentialJson,
      String iidToken,
      int timeoutSeconds) {
    CompletableFuture<ConsumerAdmissionWire.Response> out = new CompletableFuture<>();
    endpoint
        .connectNodeAddrAsync(target, ConsumerAdmissionHandler.ALPN_STRING)
        .orTimeout(timeoutSeconds, TimeUnit.SECONDS)
        .whenComplete(
            (conn, err) -> {
              if (err != null) {
                out.completeExceptionally(
                    new RpcError(StatusCode.UNAVAILABLE, "admission connect failed: " + err));
                return;
              }
              admitOnConnection(conn, credentialJson, iidToken, timeoutSeconds, out);
            });
    return out;
  }

  private static void admitOnConnection(
      IrohConnection conn,
      String credentialJson,
      String iidToken,
      int timeoutSeconds,
      CompletableFuture<ConsumerAdmissionWire.Response> out) {
    conn.openBiAsync()
        .orTimeout(timeoutSeconds, TimeUnit.SECONDS)
        .whenComplete(
            (stream, err) -> {
              if (err != null) {
                safeClose(conn);
                out.completeExceptionally(
                    new RpcError(StatusCode.UNAVAILABLE, "admission openBi failed: " + err));
                return;
              }
              runHandshake(conn, stream, credentialJson, iidToken, timeoutSeconds, out);
            });
  }

  private static void runHandshake(
      IrohConnection conn,
      IrohStream stream,
      String credentialJson,
      String iidToken,
      int timeoutSeconds,
      CompletableFuture<ConsumerAdmissionWire.Response> out) {
    byte[] reqBytes;
    try {
      reqBytes = ConsumerAdmissionWire.Request.of(credentialJson, iidToken).toJsonBytes();
    } catch (Throwable t) {
      safeClose(stream);
      safeClose(conn);
      out.completeExceptionally(
          new RpcError(StatusCode.INTERNAL, "admission request encode failed: " + t.getMessage()));
      return;
    }

    stream
        .sendAsync(reqBytes)
        .orTimeout(timeoutSeconds, TimeUnit.SECONDS)
        .thenCompose(v -> stream.finishAsync().orTimeout(timeoutSeconds, TimeUnit.SECONDS))
        .thenCompose(
            v ->
                stream
                    .readToEndAsync(MAX_RESPONSE_BYTES)
                    .orTimeout(timeoutSeconds, TimeUnit.SECONDS))
        .whenComplete(
            (respBytes, err) -> {
              // The QUIC connection stays open — the caller cleans it up or lets GC drop it. We
              // close the stream since our half of the bidi is done.
              safeClose(stream);
              safeClose(conn);
              if (err != null) {
                out.completeExceptionally(
                    new RpcError(StatusCode.UNAVAILABLE, "admission read failed: " + err));
                return;
              }
              try {
                if (respBytes == null || respBytes.length == 0) {
                  out.completeExceptionally(
                      new RpcError(
                          StatusCode.UNAVAILABLE, "admission server returned empty response"));
                  return;
                }
                out.complete(ConsumerAdmissionWire.parseResponse(respBytes));
              } catch (Throwable t) {
                out.completeExceptionally(
                    new RpcError(
                        StatusCode.INTERNAL, "admission response parse failed: " + t.getMessage()));
              }
            });
  }

  private static void safeClose(AutoCloseable c) {
    if (c == null) return;
    try {
      c.close();
    } catch (Exception ignored) {
      // best-effort
    }
  }
}
