package site.aster.coordination;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HexFormat;
import java.util.function.Function;
import org.junit.jupiter.api.Test;
import site.aster.coordination.Coordination.*;
import site.aster.upcall.Ucv1.DecodeException;

/**
 * {@link CoordinationCodec} against {@code conformance/vectors/coordination.json}: every valid case
 * decodes and re-encodes to the same bytes (and its decoded fields spot-check against the vector's
 * JSON rendering); every invalid input is refused.
 */
final class CoordinationVectorsTest {

  private static final HexFormat HEX = HexFormat.of();
  private static final ObjectMapper JSON = new ObjectMapper();

  static JsonNode vectors() throws IOException {
    Path dir = Path.of("").toAbsolutePath();
    for (int i = 0; i < 8 && dir != null; i++, dir = dir.getParent()) {
      Path p = dir.resolve("conformance").resolve("vectors").resolve("coordination.json");
      if (Files.exists(p)) {
        return JSON.readTree(p.toFile());
      }
    }
    throw new IllegalStateException("conformance/vectors/coordination.json not found above cwd");
  }

  private record Codec(Function<byte[], Object> decode, Function<Object, byte[]> encode) {}

  @SuppressWarnings("unchecked")
  private static <T> Codec codec(Function<byte[], T> dec, Function<T, byte[]> enc) {
    return new Codec(dec::apply, o -> enc.apply((T) o));
  }

  private static Codec forType(String type) {
    return switch (type) {
      case "MutationRequest" ->
          codec(CoordinationCodec::decodeMutationRequest, CoordinationCodec::encodeMutationRequest);
      case "ReadRequest" ->
          codec(CoordinationCodec::decodeReadRequest, CoordinationCodec::encodeReadRequest);
      case "MutationReply" ->
          codec(CoordinationCodec::decodeMutationReply, CoordinationCodec::encodeMutationReply);
      case "ReadReply" ->
          codec(CoordinationCodec::decodeReadReply, CoordinationCodec::encodeReadReply);
      case "AclStatus" ->
          codec(CoordinationCodec::decodeAclStatus, CoordinationCodec::encodeAclStatus);
      case "AdminStatus" ->
          codec(CoordinationCodec::decodeAdminStatus, CoordinationCodec::encodeAdminStatus);
      case "MembershipStatus" ->
          codec(
              CoordinationCodec::decodeMembershipStatus, CoordinationCodec::encodeMembershipStatus);
      case "GroupStatus" ->
          codec(CoordinationCodec::decodeGroupStatus, CoordinationCodec::encodeGroupStatus);
      case "ClientProbe" ->
          codec(CoordinationCodec::decodeClientProbe, CoordinationCodec::encodeClientProbe);
      case "AuditRecords" ->
          codec(CoordinationCodec::decodeAuditRecords, CoordinationCodec::encodeAuditRecords);
      case "RecoveryExport" ->
          codec(CoordinationCodec::decodeRecoveryExport, CoordinationCodec::encodeRecoveryExport);
      case "RecoveryExportManifest" ->
          codec(
              CoordinationCodec::decodeRecoveryManifest, CoordinationCodec::encodeRecoveryManifest);
      case "RetireReply" ->
          codec(CoordinationCodec::decodeRetireReply, CoordinationCodec::encodeRetireReply);
      case "CoordinationFence" ->
          codec(CoordinationCodec::decodeFence, CoordinationCodec::encodeFence);
      case "PlacementReport" ->
          codec(CoordinationCodec::decodePlacementReport, CoordinationCodec::encodePlacementReport);
      default -> throw new IllegalArgumentException("unknown vector type " + type);
    };
  }

  @Test
  void everyValidVectorRoundTripsExactly() throws Exception {
    JsonNode valid = vectors().get("valid");
    assertEquals(40, valid.size());
    for (JsonNode c : valid) {
      byte[] bytes = HEX.parseHex(c.get("hex").asText());
      Codec codec = forType(c.get("type").asText());
      Object decoded = codec.decode().apply(bytes);
      assertArrayEquals(bytes, codec.encode().apply(decoded), c.get("name").asText());
    }
  }

  @Test
  void everyInvalidVectorIsRefused() throws Exception {
    JsonNode invalid = vectors().get("invalid");
    assertEquals(22, invalid.size());
    for (JsonNode c : invalid) {
      byte[] bytes = HEX.parseHex(c.get("hex").asText());
      Codec codec = forType(c.get("type").asText());
      assertThrows(
          DecodeException.class,
          () -> codec.decode().apply(bytes),
          "must refuse: " + c.get("name").asText());
    }
  }

  @Test
  void decodedFieldsMatchTheVectorRendering() throws Exception {
    for (JsonNode c : vectors().get("valid")) {
      byte[] bytes = HEX.parseHex(c.get("hex").asText());
      JsonNode v = c.get("value");
      switch (c.get("type").asText()) {
        case "MutationRequest" -> {
          var m = CoordinationCodec.decodeMutationRequest(bytes);
          String op = v.get("op").asText();
          assertEquals(
              op,
              switch (m) {
                case MutationRequest.Acquire a -> "acquire";
                case MutationRequest.Renew a -> "renew";
                case MutationRequest.CompareAndSet a -> "compare_and_set";
                case MutationRequest.Release a -> "release";
                case MutationRequest.ReleaseWithValue a -> "release_with_value";
                case MutationRequest.Revoke a -> "revoke";
                case MutationRequest.SetGrant a -> "set_grant";
                case MutationRequest.SetAdmin a -> "set_admin";
                case MutationRequest.ChangeMembership a -> "change_membership";
                case MutationRequest.CancelMembership a -> "cancel_membership";
              });
          if (m instanceof MutationRequest.Acquire a) {
            assertEquals(v.get("ttl_ms").asLong(), a.ttlMs());
            assertEquals(v.get("resource").get("namespace").asText(), a.resource().namespace());
            assertEquals(v.get("resource").get("key").asText(), HEX.formatHex(a.resource().key()));
          }
          if (m instanceof MutationRequest.Revoke a) {
            assertEquals(
                v.get("expected_epoch").isNull() ? null : v.get("expected_epoch").asLong(),
                a.expectedEpoch());
          }
          if (m instanceof MutationRequest.SetGrant a) {
            assertEquals(v.get("rights").asLong(), a.rights());
            assertEquals(v.get("target_node_id").asText(), HEX.formatHex(a.targetNodeId()));
          }
          if (m instanceof MutationRequest.ChangeMembership a) {
            assertEquals(v.get("desired_voters").size(), a.desiredVoters().size());
            assertEquals(v.get("member_seeds").size(), a.memberSeeds().size());
          }
        }
        case "ReadRequest" -> {
          var q = CoordinationCodec.decodeReadRequest(bytes);
          if (q instanceof ReadRequest.AuditStream a) {
            assertEquals(v.get("limit").asLong(), a.limit());
            assertEquals(v.get("after").isNull(), a.after() == null);
          }
        }
        case "MutationReply" -> {
          var m = CoordinationCodec.decodeMutationReply(bytes);
          assertEquals(v.get("outcome_code").asInt(), m.outcomeCode());
          assertEquals(v.get("reason_code").asInt(), m.reasonCode());
          assertEquals(v.get("disposition_code").asInt(), m.dispositionCode());
          assertEquals(v.get("client_id").asText(), HEX.formatHex(m.clientId()));
          assertEquals(v.get("snapshot").isNull(), m.snapshot() == null);
        }
        case "ReadReply" -> {
          var s = CoordinationCodec.decodeReadReply(bytes);
          assertEquals(v.get("held").asBoolean(), s.held());
          assertEquals(v.get("epoch").asLong(), s.epoch());
          assertEquals(v.get("value_redacted").asBoolean(), s.valueRedacted());
          assertEquals(v.get("holder").isNull(), s.holder() == null);
        }
        case "GroupStatus" -> {
          var g = CoordinationCodec.decodeGroupStatus(bytes);
          assertEquals(v.get("authority_domain").asText(), g.authorityDomain());
          assertEquals(v.get("recovered_from").isNull(), g.recoveredFrom() == null);
          assertEquals(v.get("voters").size(), g.voters().size());
        }
        case "AuditRecords" ->
            assertEquals(v.size(), CoordinationCodec.decodeAuditRecords(bytes).size());
        case "RetireReply" -> {
          var rr = CoordinationCodec.decodeRetireReply(bytes);
          assertEquals(v.get("outcome_code").asInt(), rr.outcomeCode());
        }
        case "PlacementReport" -> {
          var p = CoordinationCodec.decodePlacementReport(bytes);
          assertEquals(v.get("reason").asText(), p.reason());
          assertEquals(v.get("policy_revision").asLong(), p.policyRevision());
          assertEquals(v.get("separation").isNull(), p.separation() == null);
        }
        default -> {
          // round trip covers the rest
        }
      }
    }
    assertTrue(true);
  }

  @Test
  void requestEncodersEnforceIdLengths() {
    assertThrows(
        IllegalArgumentException.class,
        () ->
            CoordinationCodec.encodeReadRequest(
                new ReadRequest.ProbeClient(new byte[31], new byte[16])));
    assertThrows(
        IllegalArgumentException.class,
        () -> CoordinationCodec.encodeReadRequest(new ReadRequest.AuditStream(null, 0)));
    assertThrows(
        IllegalArgumentException.class,
        () ->
            CoordinationCodec.encodeMutationRequest(
                new MutationRequest.CancelMembership(new byte[15], 1)));
  }

  @Test
  void errorEventsMapToTypedExceptions() {
    var rpcDenied =
        site.aster.ffi.IrohException.forStatus(
                site.aster.ffi.IrohStatus.PERMISSION_DENIED, "refused")
            .withEvent(5, 7);
    CoordinationException e = CoordinationException.from(rpcDenied);
    assertEquals(CoordinationException.Kind.RPC, e.kind());
    assertEquals(7, e.rpcCode());
    assertTrue(e.isPermissionDenied());
    var unauth =
        site.aster.ffi.IrohException.forStatus(site.aster.ffi.IrohStatus.UNAUTHENTICATED, "who")
            .withEvent(5, 16);
    assertTrue(CoordinationException.from(unauth).isUnauthenticated());
    assertEquals(
        CoordinationException.Kind.JOURNAL_LOCKED,
        CoordinationException.from(
                site.aster.ffi.IrohException.forStatus(site.aster.ffi.IrohStatus.INTERNAL, "l")
                    .withEvent(3, 0))
            .kind());
    assertEquals(
        CoordinationException.Kind.AWAITING_ACK,
        CoordinationException.from(
                site.aster.ffi.IrohException.forStatus(
                        site.aster.ffi.IrohStatus.INVALID_ARGUMENT, "a")
                    .withEvent(1, 0))
            .kind());
    // A synchronous status (no event code) maps by status.
    assertEquals(
        CoordinationException.Kind.CLOSED,
        CoordinationException.from(
                new site.aster.ffi.IrohException(site.aster.ffi.IrohStatus.ALREADY_CLOSED, "c"))
            .kind());
    assertEquals(110, site.aster.ffi.IrohEventKind.COORDINATION_CLIENT_OPENED.code);
    assertEquals(113, site.aster.ffi.IrohEventKind.COORDINATION_LEASE_ACQUIRED.code);
    assertEquals(48, CoordNative.OPTIONS_LAYOUT.byteSize());
  }
}
