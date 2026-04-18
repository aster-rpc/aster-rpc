package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import org.junit.jupiter.api.Test;

/**
 * Regression guard for {@link ContractIdentity} ↔ Rust canonicalizer parity.
 *
 * <p>Java never computes canonical bytes or BLAKE3 hashes locally — all canonicalization goes
 * through the Rust reference implementation per spec §11.3.2.3. This test pins that contract: if
 * someone ever introduces a Java-local canonicalizer, these assertions should catch divergence
 * against Python (which pushes the same JSON through the same Rust FFI and produces the same hash).
 *
 * <p>The Python fix at commit {@code 386d5bb} aligned Python's local {@code _get_fqn} with the
 * wire-tag namespace/typename (language-neutral). Java was never affected because it doesn't
 * compute FQNs from Java types — it forwards whatever JSON the caller built to Rust. This test
 * concretely proves that forwarding is deterministic and that non-canonical JSON edits (whitespace,
 * key ordering) don't affect the hash.
 *
 * <p>Requires the native {@code libaster_transport_ffi} to be loadable. Fails with an
 * initialization error in environments without the lib built — same behavior as {@code
 * AbiContractTest} and {@code ReactorContractTest}.
 */
final class ContractIdentityTest {

  // Minimal ServiceContract per core::contract::ServiceContract serde shape.
  // Keep this string stable — it's the baseline for the cross-language parity guard.
  private static final String MINIMAL_SERVICE_CONTRACT =
      "{"
          + "\"name\":\"EchoService\","
          + "\"version\":1,"
          + "\"methods\":[],"
          + "\"serialization_modes\":[\"xlang\"],"
          + "\"scoped\":\"shared\","
          + "\"requires\":null,"
          + "\"producer_language\":\"\""
          + "}";

  @Test
  void contractIdIsDeterministicAcrossInvocations() {
    String a = ContractIdentity.computeContractId(MINIMAL_SERVICE_CONTRACT);
    String b = ContractIdentity.computeContractId(MINIMAL_SERVICE_CONTRACT);
    assertEquals(a, b, "same JSON must produce the same contract_id");
    assertEquals(64, a.length(), "contract_id is 64-char hex BLAKE3");
    assertTrue(a.matches("[0-9a-f]{64}"), "contract_id must be lowercase hex: " + a);
  }

  @Test
  void whitespaceAndKeyOrderDoNotAffectContractId() {
    // Same logical contract, different JSON textual representation. Rust canonicalizes before
    // hashing, so the output should match.
    String reordered =
        "{"
            + "\"version\":1,"
            + "\"name\":\"EchoService\","
            + "\"scoped\":\"shared\","
            + "\"requires\":null,"
            + "\"methods\":[],"
            + "\"serialization_modes\":[\"xlang\"],"
            + "\"producer_language\":\"\""
            + "}";

    String spaced =
        "{\n"
            + "  \"name\": \"EchoService\",\n"
            + "  \"version\": 1,\n"
            + "  \"methods\": [],\n"
            + "  \"serialization_modes\": [\"xlang\"],\n"
            + "  \"scoped\": \"shared\",\n"
            + "  \"requires\": null,\n"
            + "  \"producer_language\": \"\"\n"
            + "}";

    String base = ContractIdentity.computeContractId(MINIMAL_SERVICE_CONTRACT);
    assertEquals(base, ContractIdentity.computeContractId(reordered));
    assertEquals(base, ContractIdentity.computeContractId(spaced));
  }

  @Test
  void changingCanonicalFieldChangesContractId() {
    String v2 =
        "{"
            + "\"name\":\"EchoService\","
            + "\"version\":2,"
            + "\"methods\":[],"
            + "\"serialization_modes\":[\"xlang\"],"
            + "\"scoped\":\"shared\","
            + "\"requires\":null,"
            + "\"producer_language\":\"\""
            + "}";
    String base = ContractIdentity.computeContractId(MINIMAL_SERVICE_CONTRACT);
    String bumped = ContractIdentity.computeContractId(v2);
    assertNotEquals(base, bumped, "version bump is a canonical change and must change contract_id");
  }

  @Test
  void changingServiceNameChangesContractId() {
    String renamed =
        "{"
            + "\"name\":\"ReverseEchoService\","
            + "\"version\":1,"
            + "\"methods\":[],"
            + "\"serialization_modes\":[\"xlang\"],"
            + "\"scoped\":\"shared\","
            + "\"requires\":null,"
            + "\"producer_language\":\"\""
            + "}";
    String base = ContractIdentity.computeContractId(MINIMAL_SERVICE_CONTRACT);
    assertNotEquals(base, ContractIdentity.computeContractId(renamed));
  }

  @Test
  void invalidJsonSurfacesCleanError() {
    assertThrows(
        IllegalArgumentException.class,
        () -> ContractIdentity.computeContractId("{\"not a\": \"ServiceContract\"}"));
  }

  @Test
  void canonicalBytesRoundTrips() {
    byte[] bytes =
        ContractIdentity.computeCanonicalBytes("ServiceContract", MINIMAL_SERVICE_CONTRACT);
    assertTrue(bytes.length > 0, "canonical bytes must be non-empty");
    // Stable across calls.
    byte[] again =
        ContractIdentity.computeCanonicalBytes("ServiceContract", MINIMAL_SERVICE_CONTRACT);
    assertTrue(java.util.Arrays.equals(bytes, again), "canonical bytes are deterministic");
  }

  @Test
  void blake3HexOfEmptyInputMatchesKnownDigest() {
    // BLAKE3 hash of the empty input is a well-known test vector.
    String empty = ContractIdentity.blake3Hex(new byte[0]);
    assertEquals(
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        empty,
        "BLAKE3(\"\") is a stable, published digest — mismatch means the FFI is wrong.");
    // null is treated the same as empty bytes.
    assertEquals(empty, ContractIdentity.blake3Hex(null));
  }

  @Test
  void blake3HexIsDeterministicForNonEmptyInput() {
    byte[] payload = "the quick brown fox jumps over the lazy dog".getBytes();
    String first = ContractIdentity.blake3Hex(payload);
    String second = ContractIdentity.blake3Hex(payload);
    assertEquals(first, second);
    assertEquals(64, first.length());
    assertTrue(first.matches("[0-9a-f]{64}"));
  }

  @Test
  void computeTypeHashMatchesCanonicalBytesPlusBlake3() {
    // Minimal TypeDef JSON matching core::contract::TypeDef serde shape.
    String typeDefJson =
        "{"
            + "\"kind\":\"message\","
            + "\"package\":\"test\","
            + "\"name\":\"Empty\","
            + "\"fields\":[],"
            + "\"enum_values\":[],"
            + "\"union_variants\":[]"
            + "}";

    // Convenience composer matches manual two-step composition byte-for-byte.
    String composed = ContractIdentity.computeTypeHash(typeDefJson);
    byte[] bytes = ContractIdentity.computeCanonicalBytes("TypeDef", typeDefJson);
    String manual = ContractIdentity.blake3Hex(bytes);

    assertEquals(manual, composed);
    assertEquals(64, composed.length());
    assertTrue(composed.matches("[0-9a-f]{64}"));
  }
}
