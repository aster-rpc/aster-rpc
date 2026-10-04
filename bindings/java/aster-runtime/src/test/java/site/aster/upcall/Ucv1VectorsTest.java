package site.aster.upcall;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.function.Function;
import org.junit.jupiter.api.Test;
import site.aster.lease.Fence;
import site.aster.lease.LeaseOp;
import site.aster.lease.LeaseSnapshot;
import site.aster.lease.MutateResult;
import site.aster.lease.TransitionOutcome;

/**
 * {@link Ucv1} against the shared vectors {@code conformance/vectors/upcalls.json}: every valid
 * case of a supported message type decodes to the vector's JSON rendering and re-encodes to the
 * same bytes; every invalid input is refused; the kind / status / cancel-reason tables match.
 */
final class Ucv1VectorsTest {

  private static final HexFormat HEX = HexFormat.of();
  private static final ObjectMapper JSON = new ObjectMapper();

  /** Placement messages are not used by the C ABI yet (no coordination client). */
  private static final Set<String> UNSUPPORTED =
      Set.of(
          "PlacementPolicyRequest", "PlacementPolicy", "PlacementProvisionRequest", "MemberSeed");

  static JsonNode vectors() throws IOException {
    Path dir = Path.of("").toAbsolutePath();
    for (int i = 0; i < 8 && dir != null; i++, dir = dir.getParent()) {
      Path p = dir.resolve("conformance").resolve("vectors").resolve("upcalls.json");
      if (Files.exists(p)) {
        return JSON.readTree(p.toFile());
      }
    }
    throw new IllegalStateException("conformance/vectors/upcalls.json not found above cwd");
  }

  /** decode → JSON-shaped render, and decode → encode. */
  private record Codec(Function<byte[], Object> render, Function<byte[], byte[]> roundTrip) {}

  private static Codec codec(String type) {
    return switch (type) {
      case "LeaseTransitionRequest" ->
          new Codec(
              b -> {
                var r = Ucv1.decodeLeaseTransitionRequest(b);
                return map("op", op(r.op()), "resource", r.resource());
              },
              b -> {
                var r = Ucv1.decodeLeaseTransitionRequest(b);
                return Ucv1.encodeLeaseTransitionRequest(r.resource(), r.op());
              });
      case "TransitionOutcome" ->
          new Codec(
              b -> outcome(Ucv1.decodeTransitionOutcome(b)),
              b -> Ucv1.encodeTransitionOutcome(Ucv1.decodeTransitionOutcome(b)));
      case "LeaseSnapshotRequest" ->
          new Codec(
              b -> map("resource", Ucv1.decodeLeaseSnapshotRequest(b)),
              b -> Ucv1.encodeLeaseSnapshotRequest(Ucv1.decodeLeaseSnapshotRequest(b)));
      case "LeaseSnapshot" ->
          new Codec(
              b -> snapshot(Ucv1.decodeLeaseSnapshot(b)),
              b -> Ucv1.encodeLeaseSnapshot(Ucv1.decodeLeaseSnapshot(b)));
      case "LeaseAuthorizeRequest" ->
          new Codec(
              b -> map("op", op(Ucv1.decodeLeaseAuthorizeRequest(b))),
              b -> Ucv1.encodeLeaseAuthorizeRequest(Ucv1.decodeLeaseAuthorizeRequest(b)));
      case "FencedMutateRequest" ->
          new Codec(
              b -> {
                var r = Ucv1.decodeFencedMutateRequest(b);
                return map(
                    "fence",
                    fence(r.fence()),
                    "op_hex",
                    HEX.formatHex(r.op()),
                    "writer",
                    r.writer());
              },
              b -> {
                var r = Ucv1.decodeFencedMutateRequest(b);
                return Ucv1.encodeFencedMutateRequest(r.fence(), r.writer(), r.op());
              });
      case "FencedMutateResponse" ->
          new Codec(
              b ->
                  switch (Ucv1.decodeFencedMutateResponse(b)) {
                    case MutateResult.Applied a ->
                        map("applied", map("output_hex", HEX.formatHex(a.output())));
                    case MutateResult.Rejected r ->
                        map(
                            "rejected",
                            map(
                                "carried",
                                r.rejection().carried(),
                                "current",
                                r.rejection().current(),
                                "reason",
                                r.rejection().reason().name().toLowerCase(Locale.ROOT)));
                  },
              b -> Ucv1.encodeFencedMutateResponse(Ucv1.decodeFencedMutateResponse(b)));
      case "Decision" ->
          new Codec(Ucv1::decodeDecision, b -> Ucv1.encodeDecision(Ucv1.decodeDecision(b)));
      case "TopologyAdmittedRequest" ->
          new Codec(
              b -> map("node_ids", Ucv1.decodeTopologyAdmittedRequest(b)),
              b -> Ucv1.encodeTopologyAdmittedRequest(Ucv1.decodeTopologyAdmittedRequest(b)));
      case "TopologyAdmittedResponse" ->
          new Codec(
              b -> map("admitted", Ucv1.decodeTopologyAdmittedResponse(b)),
              b -> Ucv1.encodeTopologyAdmittedResponse(Ucv1.decodeTopologyAdmittedResponse(b)));
      case "TopologySnapshotRequest" ->
          new Codec(
              b -> {
                Ucv1.decodeTopologySnapshotRequest(b);
                return map();
              },
              b -> {
                Ucv1.decodeTopologySnapshotRequest(b);
                return Ucv1.encodeTopologySnapshotRequest();
              });
      case "TopologySnapshot" ->
          new Codec(
              b -> {
                var s = Ucv1.decodeTopologySnapshot(b);
                List<Object> clusters = new ArrayList<>();
                for (var c : s.clusters()) {
                  clusters.add(
                      map(
                          "bridge",
                          c.bridge(),
                          "members",
                          c.members(),
                          "witnesses",
                          c.witnesses()));
                }
                List<Object> pairs = new ArrayList<>();
                for (var p : s.farPairs()) {
                  pairs.add(List.of(p.a(), p.b()));
                }
                return map(
                    "clusters",
                    clusters,
                    "far_pairs",
                    pairs,
                    "separation_coverage",
                    s.separationCoverage());
              },
              b -> Ucv1.encodeTopologySnapshot(Ucv1.decodeTopologySnapshot(b)));
      case "ExposeAuthorizeRequest" ->
          new Codec(
              b -> {
                var r = Ucv1.decodeExposeAuthorizeRequest(b);
                return map(
                    "metadata_hex",
                    HEX.formatHex(r.metadata()),
                    "peer_id",
                    r.peerId(),
                    "service_id",
                    r.serviceId());
              },
              b -> Ucv1.encodeExposeAuthorizeRequest(Ucv1.decodeExposeAuthorizeRequest(b)));
      case "ExposeHttpRequest" ->
          new Codec(
              b -> {
                var r = Ucv1.decodeExposeHttpRequest(b);
                return map(
                    "body",
                    body(r.body()),
                    "headers",
                    headers(r.headers()),
                    "method",
                    r.method(),
                    "peer_id",
                    r.peerId(),
                    "service_id",
                    r.serviceId(),
                    "stream_id",
                    r.streamId(),
                    "uri",
                    r.uri(),
                    "version",
                    r.version());
              },
              b -> Ucv1.encodeExposeHttpRequest(Ucv1.decodeExposeHttpRequest(b)));
      case "ExposeHttpResponse" ->
          new Codec(
              b -> {
                var r = Ucv1.decodeExposeHttpResponse(b);
                return map(
                    "body", body(r.body()), "headers", headers(r.headers()), "status", r.status());
              },
              b -> Ucv1.encodeExposeHttpResponse(Ucv1.decodeExposeHttpResponse(b)));
      case "ExposeHttpTrailers" ->
          new Codec(
              b -> headers(Ucv1.decodeExposeHttpTrailers(b)),
              b -> Ucv1.encodeExposeHttpTrailers(Ucv1.decodeExposeHttpTrailers(b)));
      case "ExposeRoutePolicyRequest" ->
          new Codec(
              b -> {
                var r = Ucv1.decodeExposeRoutePolicyRequest(b);
                List<Object> specs = new ArrayList<>();
                for (var sp : r.specs()) {
                  specs.add(
                      map(
                          "host",
                          sp.host(),
                          "port",
                          sp.port(),
                          "protocol",
                          sp.protocol() == Ucv1.ROUTE_PROTOCOL_HTTP ? "http" : "tcp",
                          "service_id",
                          sp.serviceId()));
                }
                return map(
                    "metadata_hex",
                    HEX.formatHex(r.metadata()),
                    "peer_id",
                    r.peerId(),
                    "specs",
                    specs);
              },
              b -> Ucv1.encodeExposeRoutePolicyRequest(Ucv1.decodeExposeRoutePolicyRequest(b)));
      case "ExposeRouteGrant" ->
          new Codec(
              b -> map("granted", Ucv1.decodeExposeRouteGrant(b)),
              b -> Ucv1.encodeExposeRouteGrant(Ucv1.decodeExposeRouteGrant(b)));
      case "AdmissionConnectRequest" ->
          new Codec(
              b -> {
                var r = Ucv1.decodeAdmissionConnectRequest(b);
                return map("alpn", utf8(r.alpn()), "peer", r.peer());
              },
              b -> Ucv1.encodeAdmissionConnectRequest(Ucv1.decodeAdmissionConnectRequest(b)));
      case "AdmissionHandshakeRequest" ->
          new Codec(
              b -> {
                var r = Ucv1.decodeAdmissionHandshakeRequest(b);
                return map("alpn", utf8(r.alpn()), "is_alive", r.isAlive(), "peer", r.peer());
              },
              b -> Ucv1.encodeAdmissionHandshakeRequest(Ucv1.decodeAdmissionHandshakeRequest(b)));
      case "AdmissionHandshakeResponse" ->
          new Codec(
              b -> {
                var d = Ucv1.decodeAdmissionHandshakeResponse(b);
                return map("accept", d.accept(), "code", d.code(), "reason", utf8(d.reason()));
              },
              b -> Ucv1.encodeAdmissionHandshakeResponse(Ucv1.decodeAdmissionHandshakeResponse(b)));
      default -> null;
    };
  }

  private static String utf8(byte[] b) {
    return new String(b, StandardCharsets.UTF_8);
  }

  /** A header value renders as text when it is UTF-8, else as {@code value_hex}. */
  private static List<Object> headers(List<Ucv1.HttpHeader> hs) {
    List<Object> out = new ArrayList<>();
    for (var h : hs) {
      String text = strictUtf8(h.value());
      out.add(
          text != null
              ? map("name", h.name(), "value", text)
              : map("name", h.name(), "value_hex", HEX.formatHex(h.value())));
    }
    return out;
  }

  private static Object body(Ucv1.HttpBody b) {
    return switch (b) {
      case Ucv1.HttpBody.Inline i -> map("inline", utf8(i.body()));
      case Ucv1.HttpBody.Stream x -> "stream";
    };
  }

  private static String strictUtf8(byte[] b) {
    try {
      return StandardCharsets.UTF_8.newDecoder().decode(java.nio.ByteBuffer.wrap(b)).toString();
    } catch (java.nio.charset.CharacterCodingException e) {
      return null;
    }
  }

  private static Map<String, Object> map(Object... kv) {
    Map<String, Object> m = new LinkedHashMap<>();
    for (int i = 0; i < kv.length; i += 2) {
      m.put((String) kv[i], kv[i + 1]);
    }
    return m;
  }

  private static Object op(LeaseOp op) {
    return switch (op) {
      case LeaseOp.Acquire a ->
          map("acquire", map("candidate", a.candidate(), "ttl_ms", a.ttlMs()));
      case LeaseOp.Renew r -> map("renew", map("epoch", r.epoch(), "holder", r.holder()));
      case LeaseOp.Release r -> map("release", map("epoch", r.epoch(), "holder", r.holder()));
      case LeaseOp.Revoke r -> map("revoke", map("authority", r.authority()));
    };
  }

  private static Object fence(Fence f) {
    return map("epoch", f.epoch(), "holder", f.holder(), "resource", f.resource());
  }

  private static Object snapshot(LeaseSnapshot s) {
    return map(
        "epoch",
        s.epoch(),
        "held",
        s.held(),
        "holder",
        s.holder(),
        "remaining_ms",
        s.remainingMs(),
        "resource",
        s.resource());
  }

  private static Object outcome(TransitionOutcome o) {
    return switch (o) {
      case TransitionOutcome.Granted g -> map("granted", fence(g.fence()));
      case TransitionOutcome.Renewed x -> "renewed";
      case TransitionOutcome.Released x -> "released";
      case TransitionOutcome.Revoked x -> "revoked";
      case TransitionOutcome.Denied d ->
          map(
              "denied",
              map(
                  "reason",
                  d.reason().name().toLowerCase(Locale.ROOT),
                  "snapshot",
                  snapshot(d.snapshot())));
    };
  }

  @Test
  void validCasesDecodeRenderAndReencode() throws IOException {
    int checked = 0;
    for (JsonNode v : vectors().get("valid")) {
      String type = v.get("type").asText();
      Codec c = codec(type);
      if (c == null) {
        assertTrue(UNSUPPORTED.contains(type), "no codec for " + type);
        continue;
      }
      byte[] bytes = HEX.parseHex(v.get("hex").asText());
      String name = v.get("name").asText();
      JsonNode rendered = JSON.readTree(JSON.writeValueAsString(c.render().apply(bytes)));
      assertEquals(v.get("value"), rendered, name);
      assertArrayEquals(bytes, c.roundTrip().apply(bytes), name);
      checked++;
    }
    assertEquals(41, checked);
  }

  @Test
  void invalidInputsAreRefused() throws IOException {
    int checked = 0;
    for (JsonNode v : vectors().get("invalid")) {
      Codec c = codec(v.get("type").asText());
      assertNotNull(c, v.get("type").asText());
      byte[] bytes = HEX.parseHex(v.get("hex").asText());
      assertThrows(
          Ucv1.DecodeException.class, () -> c.render().apply(bytes), v.get("name").asText());
      checked++;
    }
    assertEquals(21, checked);
  }

  @Test
  void tablesMatchTheAbi() throws IOException {
    JsonNode d = vectors();
    for (JsonNode k : d.get("kinds")) {
      UpcallKind kind = UpcallKind.fromCode(k.get("code").asInt());
      assertNotNull(kind, k.toString());
      assertEquals(k.get("name").asText(), kind.name().toLowerCase(Locale.ROOT));
      assertEquals(k.get("decision").asBoolean(), kind.decision);
    }
    assertEquals(d.get("kinds").size(), UpcallKind.values().length);
    Map<String, Integer> statuses =
        Map.of(
            "ok", Upcalls.STATUS_OK,
            "error", Upcalls.STATUS_ERROR,
            "unimplemented", Upcalls.STATUS_UNIMPLEMENTED);
    for (JsonNode s : d.get("statuses")) {
      assertEquals(statuses.get(s.get("name").asText()), s.get("code").asInt());
    }
    Map<String, Integer> reasons =
        Map.of(
            "timeout", Upcalls.CANCEL_TIMEOUT,
            "abandoned", Upcalls.CANCEL_ABANDONED,
            "shutdown", Upcalls.CANCEL_SHUTDOWN);
    for (JsonNode r : d.get("cancel_reasons")) {
      assertEquals(reasons.get(r.get("name").asText()), r.get("code").asInt());
    }
  }

  @Test
  void unsignedAndBoundaryValues() {
    // Epochs are u64: the full range round-trips.
    byte[] b = Ucv1.encodeLeaseAuthorizeRequest(new LeaseOp.Renew("h", -1L));
    assertEquals(new LeaseOp.Renew("h", -1L), Ucv1.decodeLeaseAuthorizeRequest(b));
    // A truncated payload is refused, not read past.
    byte[] trunc = java.util.Arrays.copyOf(b, b.length - 1);
    assertThrows(Ucv1.DecodeException.class, () -> Ucv1.decodeLeaseAuthorizeRequest(trunc));
    // Invalid UTF-8 in a string is refused.
    byte[] bad = {(byte) 0xB3, 0x01, 0x06, (byte) 0xFF};
    assertThrows(Ucv1.DecodeException.class, () -> Ucv1.decodeLeaseSnapshotRequest(bad));
  }
}
