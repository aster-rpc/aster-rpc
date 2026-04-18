package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * Parity tests pinning Java's contract identity records against Python's golden vectors at {@code
 * tests/python/fixtures/canonical_test_vectors.json}. Both sides hand JSON to the same Rust
 * canonicalizer; if the Jackson serde shape diverges from the Rust serde shape, canonical bytes
 * won't match and these golden hashes will fail.
 *
 * <p>Requires {@code libaster_transport_ffi} to be loadable — same gating as other {@link
 * ContractIdentity} tests.
 */
final class ContractModelSerdeTest {

  // ── Appendix A.2: Minimal ServiceContract ─────────────────────────────────

  @Test
  void serviceContractA2MatchesGoldenHash() {
    ServiceContract sc =
        new ServiceContract(
            "EmptyService", 1, List.of(), List.of("xlang"), ScopeKind.SHARED, null, "");
    String json = sc.toJson();
    String hex = ContractIdentity.computeContractId(json);
    assertEquals(
        "d016f1c19d536b69c4fb2af96acce700da5c45bd6c4860b6c9ae408b4ca35438",
        hex,
        "Java ServiceContract JSON must match Python's canonical bytes for A.2. JSON was: " + json);
  }

  // ── Appendix A.3: Minimal TypeDef (enum) ──────────────────────────────────

  @Test
  void typeDefA3EnumMatchesGoldenHash() {
    TypeDef td =
        new TypeDef(
            TypeDefKind.ENUM,
            "test",
            "Color",
            List.of(),
            List.of(
                new EnumValueDef("RED", 0),
                new EnumValueDef("GREEN", 1),
                new EnumValueDef("BLUE", 2)),
            List.of());
    String hex = ContractIdentity.computeTypeHash(ContractJson.toJson(td));
    assertEquals(
        "bac1586aaa144fa0b565268419da29f18e536f18c7290e4bdf3496919cfa29ce",
        hex,
        "enum TypeDef canonical hash must match A.3 golden");
  }

  // ── Appendix A.4: TypeDef with REF field (0xAA*32) ────────────────────────

  @Test
  void typeDefA4MessageWithRefMatchesGoldenHash() {
    String aaHashHex = "aa".repeat(32);
    TypeDef td =
        new TypeDef(
            TypeDefKind.MESSAGE,
            "test",
            "Wrapper",
            List.of(
                new FieldDef(
                    1,
                    "inner",
                    TypeKind.REF,
                    "",
                    aaHashHex,
                    "",
                    false,
                    false,
                    ContainerKind.NONE,
                    TypeKind.PRIMITIVE,
                    "",
                    "",
                    true,
                    "")),
            List.of(),
            List.of());
    String hex = ContractIdentity.computeTypeHash(ContractJson.toJson(td));
    assertEquals(
        "67396f0456ee178135a0adb73adbf884c57ce0358e2698d3b21a7eb5820d7c4f",
        hex,
        "MESSAGE TypeDef with REF field must match A.4 golden");
  }

  // ── Round-trip JSON shape sanity ──────────────────────────────────────────

  @Test
  void typeDefJsonKeysAreSnakeCase() throws JsonProcessingException {
    TypeDef td = new TypeDef(TypeDefKind.MESSAGE, "x", "Y", List.of(), List.of(), List.of());
    String json = ContractJson.toJson(td);
    // Rust serde expects snake_case on all multi-word keys; these assertions fail loudly if
    // Jackson is emitting camelCase or missing @JsonProperty mappings.
    assertTrue(json.contains("\"enum_values\""), json);
    assertTrue(json.contains("\"union_variants\""), json);
    assertTrue(json.contains("\"package\""), json);
  }

  @Test
  void fieldDefJsonKeysAreSnakeCase() throws JsonProcessingException {
    FieldDef f =
        new FieldDef(
            1,
            "x",
            TypeKind.PRIMITIVE,
            "int64",
            "",
            "",
            false,
            false,
            ContainerKind.NONE,
            TypeKind.PRIMITIVE,
            "",
            "",
            true,
            "");
    String json = ContractJson.toJson(f);
    // Spot-check every multi-word key.
    assertTrue(json.contains("\"type_kind\""), json);
    assertTrue(json.contains("\"type_primitive\""), json);
    assertTrue(json.contains("\"type_ref\""), json);
    assertTrue(json.contains("\"self_ref_name\""), json);
    assertTrue(json.contains("\"ref_tracked\""), json);
    assertTrue(json.contains("\"container_key_kind\""), json);
    assertTrue(json.contains("\"container_key_primitive\""), json);
    assertTrue(json.contains("\"container_key_ref\""), json);
    assertTrue(json.contains("\"default_value\""), json);
  }

  @Test
  void enumsUseSnakeCaseLowercaseWire() throws JsonProcessingException {
    ObjectMapper m = ContractJson.mapper();
    assertEquals("\"primitive\"", m.writeValueAsString(TypeKind.PRIMITIVE));
    assertEquals("\"self_ref\"", m.writeValueAsString(TypeKind.SELF_REF));
    assertEquals("\"server_stream\"", m.writeValueAsString(MethodPattern.SERVER_STREAM));
    assertEquals("\"bidi_stream\"", m.writeValueAsString(MethodPattern.BIDI_STREAM));
    assertEquals("\"any_of\"", m.writeValueAsString(CapabilityKind.ANY_OF));
    assertEquals("\"session\"", m.writeValueAsString(ScopeKind.SESSION));
    assertEquals("\"message\"", m.writeValueAsString(TypeDefKind.MESSAGE));
  }

  @Test
  void serviceContractRoundTripsThroughJson() {
    ServiceContract original =
        new ServiceContract(
            "Foo",
            2,
            List.of(),
            List.of("xlang", "native"),
            ScopeKind.SESSION,
            new CapabilityRequirement(CapabilityKind.ROLE, List.of("admin")),
            "java");
    String json = original.toJson();
    ServiceContract round = ContractJson.fromJson(json, ServiceContract.class);
    assertEquals(original, round);
  }

  @Test
  void typeDefRoundTripsThroughJson() {
    TypeDef original =
        new TypeDef(
            TypeDefKind.MESSAGE,
            "pkg",
            "Name",
            List.of(
                new FieldDef(
                    1,
                    "f",
                    TypeKind.PRIMITIVE,
                    "string",
                    "",
                    "",
                    true,
                    false,
                    ContainerKind.NONE,
                    TypeKind.PRIMITIVE,
                    "",
                    "",
                    false,
                    "")),
            List.of(),
            List.of());
    String json = ContractJson.toJson(original);
    TypeDef round = ContractJson.fromJson(json, TypeDef.class);
    assertEquals(original, round);
  }
}
