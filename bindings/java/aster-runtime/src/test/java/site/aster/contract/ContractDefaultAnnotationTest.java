package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import org.apache.fory.Fory;
import org.junit.jupiter.api.Test;
import site.aster.annotations.ContractDefault;
import site.aster.annotations.Scope;
import site.aster.annotations.WireType;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.interceptors.CallContext;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * {@link ContractDefault} on Java records / classes lowers (§11.3.2.3) to the same {@code
 * type_hash} and {@code contract_id} as the Rust {@code #[aster(default = ...)]} producer pinned in
 * {@code conformance/vectors/contract-defaults.json} (which Python's {@code contract_default} and
 * TypeScript's {@code @ContractDefault} also reproduce). Needs {@code libaster_transport_ffi}.
 */
final class ContractDefaultAnnotationTest {

  /** The vector's {@code defaults.v1/Settings}, declared with Java annotations. */
  @WireType("defaults.v1/Settings")
  public record Settings(
      String name,
      @ContractDefault("idle") String state,
      @ContractDefault("") String note,
      @ContractDefault("true") boolean enabled,
      @ContractDefault("-3") int retries,
      @ContractDefault("1700000000000") long since_ms,
      @ContractDefault("0.5") double ratio,
      @ContractDefault("x") Optional<String> label,
      @ContractDefault List<String> tags,
      @ContractDefault Map<String, String> attrs,
      byte[] blob) {}

  /** Same shape, no annotations: every field required. */
  @WireType("defaults.v1/Settings")
  public record PlainSettings(
      String name,
      String state,
      String note,
      boolean enabled,
      int retries,
      long since_ms,
      double ratio,
      Optional<String> label,
      List<String> tags,
      Map<String, String> attrs,
      byte[] blob) {}

  /** A plain class: the annotation goes on the field. */
  @WireType("defaults.v1/Pojo")
  public static final class Pojo {
    @ContractDefault("a")
    String s = "a";

    int required = 0;
  }

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

  private static ContractIdentityResolver.ResolvedTypes resolve(Class<?> cls) {
    return ContractIdentityResolver.resolve(TypeGraphWalker.walk(List.of(cls)));
  }

  private static FieldDef field(Class<?> cls, String name) {
    TypeDef td = resolve(cls).typeDefs().get(WireIdentity.of(cls).fqn());
    return td.fields().stream().filter(f -> f.name().equals(name)).findFirst().orElseThrow();
  }

  @Test
  void annotatedRecordReproducesTheVectorTypeHashAndContractId() throws IOException {
    JsonNode v = vector();
    var resolved = resolve(Settings.class);
    assertEquals(
        v.get("expected_type_hash").asText(),
        resolved.typeHashes().get(WireIdentity.of(Settings.class).fqn()));
    assertEquals(
        v.get("expected_contract_id").asText(),
        ContractManifestBuilder.computeContractId(new DefaultsDispatcher(Settings.class)));
  }

  @Test
  void everyFieldLowersToTheVectorsRequiredFlagAndDefaultBytes() throws IOException {
    JsonNode fields = vector().get("type_def").get("fields_in_declaration_order");
    for (JsonNode f : fields) {
      FieldDef fd = field(Settings.class, f.get("name").asText());
      assertEquals(f.get("required").asBoolean(), fd.required(), f.get("name").asText());
      assertEquals(f.get("default_value_hex").asText(), fd.defaultValue(), f.get("name").asText());
    }
  }

  @Test
  void aPlainInitializerIsNotAContractDefault() throws IOException {
    var plain = resolve(PlainSettings.class);
    String plainHash = plain.typeHashes().get(WireIdentity.of(PlainSettings.class).fqn());
    assertNotEquals(vector().get("expected_type_hash").asText(), plainHash);
    for (FieldDef fd : plain.typeDefs().get("defaults.v1.Settings").fields()) {
      assertTrue(fd.required(), fd.name());
      assertEquals("", fd.defaultValue(), fd.name());
    }
  }

  @Test
  void classFieldAnnotationIsHonoured() {
    FieldDef s = field(Pojo.class, "s");
    assertEquals(false, s.required());
    assertEquals("0661", s.defaultValue());
    assertTrue(field(Pojo.class, "required").required());
  }

  @Test
  void manifestFieldsReportTheContractDefault() {
    ContractManifest m = ContractManifestBuilder.build(new DefaultsDispatcher(Settings.class));
    @SuppressWarnings("unchecked")
    List<Map<String, Object>> fields = (List<Map<String, Object>>) m.methods().get(0).get("fields");
    Map<String, Map<String, Object>> byName = new HashMap<>();
    fields.forEach(f -> byName.put((String) f.get("name"), f));
    assertEquals(true, byName.get("name").get("required"));
    assertEquals(false, byName.get("state").get("required"));
    assertEquals("idle", byName.get("state").get("default_value"));
    assertEquals(-3L, byName.get("retries").get("default_value"));
    assertEquals(0.5, byName.get("ratio").get("default_value"));
    assertEquals(true, byName.get("enabled").get("default_value"));
    assertEquals("empty_list", byName.get("tags").get("default_kind"));
    assertEquals("empty_map", byName.get("attrs").get("default_kind"));
  }

  @WireType("defaults.v1/Wire")
  public record WireWithDefaults(
      @ContractDefault("idle") String state,
      @ContractDefault("-3") int retries,
      @ContractDefault List<String> tags) {}

  @WireType("defaults.v1/Wire")
  public record WirePlain(String state, int retries, List<String> tags) {}

  @Test
  void defaultsNeverChangeThePayloadBytes() {
    ForyCodec codec = new ForyCodec();
    codec.fory().register(WireWithDefaults.class, "defaults.v1/Wire");
    ForyCodec plainCodec = new ForyCodec();
    plainCodec.fory().register(WirePlain.class, "defaults.v1/Wire");
    assertArrayEquals(
        codec.encode(new WireWithDefaults("busy", 7, List.of("a"))),
        plainCodec.encode(new WirePlain("busy", 7, List.of("a"))));
  }

  // ── fail-fast on anything §11.3.2.3 does not define ───────────────────────

  @WireType("bad.v1/Byte")
  public record BadByte(@ContractDefault("1") byte b) {}

  @WireType("bad.v1/Float32")
  public record BadFloat32(@ContractDefault("1.5") float f) {}

  @WireType("bad.v1/Bytes")
  public record BadBinary(@ContractDefault("") byte[] b) {}

  @WireType("bad.v1/Inner")
  public record Inner(String x) {}

  @WireType("bad.v1/Nested")
  public record BadNested(@ContractDefault("x") Inner inner) {}

  @WireType("bad.v1/ScalarUnset")
  public record BadScalarUnset(@ContractDefault String s) {}

  @WireType("bad.v1/ContainerValue")
  public record BadContainerValue(@ContractDefault("[a]") List<String> l) {}

  @WireType("bad.v1/IntLiteral")
  public record BadInt(@ContractDefault("abc") int i) {}

  @WireType("bad.v1/IntRange")
  public record BadIntRange(@ContractDefault("2147483648") int i) {}

  @WireType("bad.v1/Bool")
  public record BadBool(@ContractDefault("yes") boolean b) {}

  @WireType("bad.v1/Subnormal")
  public record BadSubnormal(@ContractDefault("1e-310") double d) {}

  @WireType("bad.v1/NaN")
  public record BadNan(@ContractDefault("NaN") double d) {}

  @Test
  void unsupportedDefaultsAreRefused() {
    for (Class<?> c :
        List.of(
            BadByte.class,
            BadFloat32.class,
            BadBinary.class,
            BadNested.class,
            BadScalarUnset.class,
            BadContainerValue.class,
            BadInt.class,
            BadIntRange.class,
            BadBool.class,
            BadSubnormal.class,
            BadNan.class)) {
      IllegalArgumentException e =
          assertThrows(IllegalArgumentException.class, () -> resolve(c), c.getSimpleName());
      assertTrue(e.getMessage().contains("field "), e.getMessage());
    }
  }

  @WireType("ok.v1/Edge")
  public record Edge(
      @ContractDefault("-0.0") double negZero,
      @ContractDefault("0.0") double zero,
      @ContractDefault("-9223372036854775808") long minLong,
      @ContractDefault("2147483647") int maxInt) {}

  @Test
  void edgeValuesLowerCanonically() {
    assertEquals("0000000000000000", field(Edge.class, "negZero").defaultValue());
    assertEquals(
        field(Edge.class, "zero").defaultValue(), field(Edge.class, "negZero").defaultValue());
    assertEquals("ffffffffffffffffff01", field(Edge.class, "minLong").defaultValue());
    assertEquals("feffffff0f", field(Edge.class, "maxInt").defaultValue());
  }

  // ── fixture: a Defaults v1 service, one unary `put` (the vector's contract) ──

  static final class DefaultsDispatcher implements ServiceDispatcher {
    private final Class<?> type;
    private final ServiceDescriptor descriptor =
        new ServiceDescriptor("Defaults", 1, Scope.SHARED, Object.class);
    private final Map<String, MethodDispatcher> methods = Map.of("put", new PutMethod());

    DefaultsDispatcher(Class<?> type) {
      this.type = type;
    }

    @Override
    public ServiceDescriptor descriptor() {
      return descriptor;
    }

    @Override
    public Map<String, MethodDispatcher> methods() {
      return methods;
    }

    @Override
    public void registerTypes(Fory fory) {}

    @Override
    public Map<String, Class<?>> requestClasses() {
      return Map.of("put", type);
    }

    @Override
    public Map<String, Class<?>> responseClasses() {
      return Map.of("put", type);
    }
  }

  static final class PutMethod implements UnaryDispatcher {
    static final MethodDescriptor DESCRIPTOR =
        new MethodDescriptor(
            "put",
            StreamingKind.UNARY,
            RequestStyle.EXPLICIT,
            "defaults.v1/Settings",
            List.of(),
            "defaults.v1/Settings",
            false,
            false);

    @Override
    public MethodDescriptor descriptor() {
      return DESCRIPTOR;
    }

    @Override
    public byte[] invoke(Object impl, byte[] requestBytes, Codec codec, CallContext ctx) {
      return new byte[0];
    }
  }
}
