package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import org.apache.fory.annotation.Nullable;
import org.apache.fory.util.StringUtils;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;
import site.aster.annotations.WireName;
import site.aster.annotations.WireType;
import site.aster.codec.ForyCodec;

/**
 * {@code conformance/vectors/contract-types.json} {@code field_names}
 * (ffi_spec/Aster-FieldNaming.md): the Orders contract as Java defines it (plain camelCase fields:
 * the declared names, verbatim) and as Rust defines it (the same members with {@link WireName}),
 * the shared payload, Fory Java's snake_case conversion, and the refusals.
 */
@SuppressWarnings({"checkstyle:MemberName", "MemberName"})
final class FieldNameVectorTest {

  private static final JsonNode V = ContractTypesVectorTest.VECTOR.get("field_names");

  // ── the contract defined in Java ──

  @WireType("names.v1/Order")
  public static final class JOrder {
    public String orderId = "";
    public List<JLineItem> lineItems = new ArrayList<>();
    public int totalCents;
    public @Nullable String note;
  }

  @WireType("names.v1/LineItem")
  public static final class JLineItem {
    public String skuCode = "";
    public int unitCount;
  }

  // ── the contract defined in Rust, implemented in Java ──

  @WireType("names.v1/Order")
  public static final class RsOrder {
    @WireName("order_id")
    public String orderId = "";

    @WireName("line_items")
    public List<RsLineItem> lineItems = new ArrayList<>();

    @WireName("total_cents")
    public int totalCents;

    public @Nullable String note;
  }

  @WireType("names.v1/LineItem")
  public static final class RsLineItem {
    @WireName("sku_code")
    public String skuCode = "";

    @WireName("unit_count")
    public int unitCount;
  }

  // ── refused ──

  @WireType("names.v1/Bad")
  public static final class Acronym {
    public String userID = "";
  }

  @WireType("names.v1/Bad")
  public static final class Mismatch {
    @WireName("writtenBy")
    public String author = "";
  }

  @WireType("names.v1/Bad")
  public static final class Collide {
    public String userId = "";
    public String user_id = "";
  }

  /** Fory Java writes a leading capital as {@code _value}: a member must not start with one. */
  @WireType("names.v1/Bad")
  public static final class Capital {
    public String Value = "";
  }

  /** The Java way to implement a Go / C# field {@code Value}. */
  @WireType("names.v1/Good")
  public static final class CapitalOverride {
    @WireName("Value")
    public String value = "";
  }

  private static String hashOf(Class<?> c) {
    var resolved = ContractIdentityResolver.resolve(TypeGraphWalker.walk(List.of(c)));
    return resolved.typeHashes().get(WireIdentity.of(c).fqn());
  }

  @Test
  void foryJavaSnakeCaseAndPlainnessMatchTheVector() {
    V.get("snake_case")
        .fields()
        .forEachRemaining(
            e -> {
              String name = e.getKey();
              JsonNode row = e.getValue();
              assertEquals(
                  row.get("java").asText(), StringUtils.lowerCamelToLowerUnderscore(name), name);
              assertEquals(row.get("rust").asText(), FieldNames.referenceSnakeCase(name), name);
              boolean plain = row.get("plain").asBoolean();
              assertEquals(plain, FieldNames.notPlain(name) == null, name);
              if (!plain) {
                assertNull(FieldNames.notPlain(FieldNames.plainSuggestion(name)), name);
              }
            });
  }

  private static List<FieldNames.Entry> entries(JsonNode c) {
    List<FieldNames.Entry> out = new ArrayList<>();
    c.get("fields")
        .forEach(
            f ->
                out.add(
                    new FieldNames.Entry(f.get("member_snake").asText(), f.get("name").asText())));
    return out;
  }

  @Test
  void refusedAndAcceptedCasesMatchTheVector() {
    for (JsonNode c : V.get("refused")) {
      IllegalArgumentException e =
          assertThrows(IllegalArgumentException.class, () -> FieldNames.check(entries(c)));
      assertTrue(e.getMessage().contains(c.get("error").asText()), c + ": " + e.getMessage());
    }
    for (JsonNode c : V.get("accepted")) {
      FieldNames.check(entries(c));
    }
  }

  @Test
  void javaTypesAreRefusedWhenTheContractIsBuilt() {
    Map<Class<?>, String> want =
        Map.of(
            Acronym.class, "not a plain field name",
            Mismatch.class, "does not match",
            Collide.class, "collide",
            Capital.class, "does not match");
    want.forEach(
        (cls, phrase) -> {
          IllegalArgumentException e =
              assertThrows(IllegalArgumentException.class, () -> hashOf(cls), cls.getName());
          assertTrue(e.getMessage().contains(phrase), e.getMessage());
        });
    var td =
        ContractIdentityResolver.resolve(TypeGraphWalker.walk(List.of(CapitalOverride.class)))
            .typeDefs()
            .get("names.v1.Good");
    assertEquals("Value", td.fields().getFirst().name());
  }

  @ParameterizedTest
  @ValueSource(strings = {"java_defined", "rust_defined"})
  void ordersContractHashesIdsAndPayloadMatch(String def) {
    JsonNode d = V.get(def);
    boolean javaDef = def.equals("java_defined");
    Class<?> order = javaDef ? JOrder.class : RsOrder.class;
    Class<?> item = javaDef ? JLineItem.class : RsLineItem.class;
    var resolved = ContractIdentityResolver.resolve(TypeGraphWalker.walk(List.of(order)));
    for (String fqn : List.of("names.v1.Order", "names.v1.LineItem")) {
      JsonNode exp = d.get("types").get(fqn);
      TypeDef td = resolved.typeDefs().get(fqn);
      assertEquals(
          exp.get("expected_canonical_type_def_hex").asText(),
          java.util.HexFormat.of()
              .formatHex(
                  ContractIdentity.computeCanonicalBytes("TypeDef", ContractJson.toJson(td))),
          fqn);
      assertEquals(exp.get("expected_type_hash").asText(), resolved.typeHashes().get(fqn), fqn);
    }
    var svc =
        new ContractTypesVectorTest.Service(
            "Orders", Map.of("place", new Class<?>[] {order, order}));
    assertEquals(
        d.get("expected_contract_id").asText(), ContractManifestBuilder.computeContractId(svc));

    ForyCodec codec = new ForyCodec();
    for (Class<?> c : List.of(item, order)) {
      WireIdentity id = WireIdentity.of(c);
      ((org.apache.fory.ThreadSafeFory) codec.fory())
          .registerCallback(f -> f.register(c, id.packageName(), id.name()));
    }
    JsonNode p = d.get("payload");
    Object got = codec.decode(java.util.HexFormat.of().parseHex(p.get("hex").asText()), order);
    assertEquals(p.get("value"), ContractTypesVectorTest.render(got), def);
    assertEquals(
        p.get("value"),
        ContractTypesVectorTest.render(codec.decode(codec.encode(got), order)),
        def);
  }
}
