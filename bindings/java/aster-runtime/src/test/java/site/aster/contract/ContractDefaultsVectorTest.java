package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import org.junit.jupiter.api.Test;

/**
 * {@code conformance/vectors/contract-defaults.json} (Aster-ContractIdentity.md §11.3.2.3): a
 * {@link TypeDef} whose {@link FieldDef}s carry {@code required=false} plus the canonical default
 * bytes canonicalizes (through the Rust FFI) to the same bytes, type hash and contract id as the
 * Rust {@code #[aster(default = ...)]} producer.
 *
 * <p>This covers the contract-identity plumbing (a hand-built {@link TypeDef}); {@link
 * ContractDefaultAnnotationTest} gates the {@code @ContractDefault} annotation layer on the same
 * vector.
 */
final class ContractDefaultsVectorTest {

  private static JsonNode vector() throws IOException {
    Path dir = Path.of("").toAbsolutePath();
    for (int i = 0; i < 8 && dir != null; i++, dir = dir.getParent()) {
      Path p = dir.resolve("conformance/vectors/contract-defaults.json");
      if (Files.exists(p)) {
        return new ObjectMapper().readTree(p.toFile()).get("vector");
      }
    }
    throw new IllegalStateException("conformance/vectors/contract-defaults.json not found");
  }

  private static FieldDef field(JsonNode f, boolean honourDefaults) {
    ContainerKind container =
        switch (f.get("container").asInt()) {
          case 0 -> ContainerKind.NONE;
          case 1 -> ContainerKind.LIST;
          case 2 -> ContainerKind.SET;
          case 3 -> ContainerKind.MAP;
          default -> throw new AssertionError("container " + f.get("container"));
        };
    boolean required = !honourDefaults || f.get("required").asBoolean();
    return new FieldDef(
        0, // ignored: field ids are re-derived from NFC-sorted names
        f.get("name").asText(),
        TypeKind.PRIMITIVE,
        f.get("type_primitive").asText(),
        "",
        "",
        f.get("optional").asBoolean(),
        false,
        container,
        TypeKind.PRIMITIVE,
        // The vector's only map is BTreeMap<String, String>.
        container == ContainerKind.MAP ? "string" : "",
        "",
        required,
        required ? "" : f.get("default_value_hex").asText());
  }

  private static TypeDef typeDef(JsonNode v, boolean honourDefaults) {
    JsonNode td = v.get("type_def");
    List<FieldDef> fields = new ArrayList<>();
    td.get("fields_in_declaration_order").forEach(f -> fields.add(field(f, honourDefaults)));
    return new TypeDef(
        TypeDefKind.MESSAGE,
        td.get("package").asText(),
        td.get("name").asText(),
        fields,
        List.of(),
        List.of());
  }

  @Test
  void defaultsVectorReproducesCanonicalBytesTypeHashAndContractId() throws IOException {
    JsonNode v = vector();
    String json = ContractJson.toJson(typeDef(v, true));

    byte[] canonical = ContractIdentity.computeCanonicalBytes("TypeDef", json);
    assertEquals(
        v.get("expected_canonical_type_def_hex").asText(), HexFormat.of().formatHex(canonical));
    String typeHash = ContractIdentity.computeTypeHash(json);
    assertEquals(v.get("expected_type_hash").asText(), typeHash);

    ServiceContract svc =
        new ServiceContract(
            "Defaults",
            1,
            List.of(
                new MethodDef("put", MethodPattern.UNARY, typeHash, typeHash, false, 0.0, null)),
            List.of("xlang"),
            ScopeKind.SHARED,
            null,
            "");
    assertEquals(
        v.get("expected_contract_id").asText(),
        ContractIdentity.computeContractId(ContractJson.toJson(svc)));
  }

  @Test
  void declaringDefaultsChangesTheTypeHash() throws IOException {
    JsonNode v = vector();
    assertNotEquals(
        ContractIdentity.computeTypeHash(ContractJson.toJson(typeDef(v, false))),
        v.get("expected_type_hash").asText());
  }
}
