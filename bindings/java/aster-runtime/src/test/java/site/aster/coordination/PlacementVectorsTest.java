package site.aster.coordination;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import com.fasterxml.jackson.databind.JsonNode;
import java.util.HexFormat;
import java.util.List;
import org.junit.jupiter.api.Test;
import site.aster.coordination.Coordination.MemberSeed;
import site.aster.coordination.Placement.Candidate;
import site.aster.coordination.Placement.PlacementPolicy;

/** The placement messages (upcall kinds 32 / 33, tags BC-BF) against {@code upcalls.json}. */
final class PlacementVectorsTest {

  private static final HexFormat HEX = HexFormat.of();

  @Test
  void placementVectorsRoundTrip() throws Exception {
    int seen = 0;
    for (JsonNode c : upcallVectors().get("valid")) {
      byte[] b = HEX.parseHex(c.get("hex").asText());
      switch (b[0] & 0xff) {
        case 0xBC -> {
          Placement.decodePolicyRequest(b);
          assertArrayEquals(b, Placement.encodePolicyRequest());
        }
        case 0xBD -> {
          PlacementPolicy p = Placement.decodePolicy(b);
          assertArrayEquals(b, Placement.encodePolicy(p));
          JsonNode v = c.get("value");
          assertEquals(v.get("authority_domain").asText(), p.authorityDomain());
          assertEquals(v.get("eligible").size(), p.eligible().size());
          assertEquals(v.get("replacement_grace_ms").asLong(), p.replacementGraceMs());
        }
        case 0xBE ->
            assertArrayEquals(
                b, Placement.encodeProvisionRequest(Placement.decodeProvisionRequest(b)));
        case 0xBF -> {
          MemberSeed s = CoordinationCodec.decodeMemberSeedMessage(b);
          assertArrayEquals(b, CoordinationCodec.encodeMemberSeedMessage(s));
        }
        default -> {
          continue;
        }
      }
      seen++;
    }
    assertEquals(5, seen);
  }

  @Test
  void setsMustBeStrictlyAscendingAndIdsExact() {
    byte[] one = new byte[32];
    byte[] two = new byte[32];
    two[0] = 1;
    PlacementPolicy p =
        new PlacementPolicy(
            1,
            new byte[32],
            new byte[32],
            "d",
            1,
            0,
            List.of(new Candidate(two, null), new Candidate(one, null)),
            List.of(),
            List.of(),
            0,
            0,
            false);
    // The encoder sorts, so what it writes decodes.
    PlacementPolicy back = Placement.decodePolicy(Placement.encodePolicy(p));
    assertArrayEquals(one, back.eligible().get(0).nodeId());
    // A duplicate is refused by the encoder.
    assertThrows(
        IllegalArgumentException.class,
        () ->
            Placement.encodePolicy(
                new PlacementPolicy(
                    1,
                    new byte[32],
                    new byte[32],
                    "d",
                    1,
                    0,
                    List.of(),
                    List.of(one, one),
                    List.of(),
                    0,
                    0,
                    false)));
    assertThrows(
        IllegalArgumentException.class,
        () ->
            Placement.encodePolicy(
                new PlacementPolicy(
                    1,
                    new byte[31],
                    new byte[32],
                    "d",
                    1,
                    0,
                    List.of(),
                    List.of(),
                    List.of(),
                    0,
                    0,
                    false)));
  }

  private static JsonNode upcallVectors() throws java.io.IOException {
    java.nio.file.Path dir = java.nio.file.Path.of("").toAbsolutePath();
    for (int i = 0; i < 8 && dir != null; i++, dir = dir.getParent()) {
      java.nio.file.Path p = dir.resolve("conformance/vectors/upcalls.json");
      if (java.nio.file.Files.exists(p)) {
        return new com.fasterxml.jackson.databind.ObjectMapper().readTree(p.toFile());
      }
    }
    throw new IllegalStateException("conformance/vectors/upcalls.json not found");
  }
}
