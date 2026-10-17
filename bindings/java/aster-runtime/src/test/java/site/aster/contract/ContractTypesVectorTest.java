package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.JsonNodeFactory;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.IOException;
import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.math.BigDecimal;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Instant;
import java.time.LocalDate;
import java.time.ZoneOffset;
import java.time.format.DateTimeFormatter;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.TreeMap;
import java.util.UUID;
import java.util.stream.Stream;
import org.apache.fory.Fory;
import org.apache.fory.annotation.Nullable;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.Arguments;
import org.junit.jupiter.params.provider.MethodSource;
import site.aster.annotations.Scope;
import site.aster.annotations.WireType;
import site.aster.codec.Codec;
import site.aster.codec.ForyCodec;
import site.aster.codec.Ulid;
import site.aster.interceptors.CallContext;
import site.aster.server.spi.MethodDescriptor;
import site.aster.server.spi.MethodDispatcher;
import site.aster.server.spi.RequestStyle;
import site.aster.server.spi.ServiceDescriptor;
import site.aster.server.spi.ServiceDispatcher;
import site.aster.server.spi.StreamingKind;
import site.aster.server.spi.UnaryDispatcher;

/**
 * {@code conformance/vectors/contract-types.json} (Aster-ContractIdentity.md, "Time, UUID, and
 * Decimal" and §11.3.4 Step 3): Java's {@link Instant} / {@link LocalDate} / {@link BigDecimal} /
 * {@link FixedDecimal} / {@link UUID} / {@link Ulid} and recursive types produce Rust's type
 * hashes, canonical bytes and contract ids; {@link ForyCodec} decodes Rust's payloads, writes
 * Rust's bytes, and refuses the same {@code decode_cases}.
 *
 * <p>Fixture field names are the contract's (snake_case) names: Java contract identity uses the
 * declared field name as is.
 */
@SuppressWarnings({"checkstyle:MemberName", "MemberName"})
final class ContractTypesVectorTest {

  // ── fixture (plain classes: Fory Java records with List fields do not decode) ──

  @WireType("types.v1/Temporal")
  public static final class Temporal {
    public Instant at = Instant.EPOCH;
    public LocalDate day = LocalDate.EPOCH;
    public BigDecimal amount = BigDecimal.ZERO;
    public @Nullable Instant maybe_at;
    public List<Instant> history = new ArrayList<>();
  }

  @WireType("types.v1/TreeNode")
  public static final class TreeNode {
    public String value = "";
    public List<TreeNode> children = new ArrayList<>();
  }

  @WireType("types.v1/Chain")
  public static final class Chain {
    public int value;
    public @Nullable Chain next;
  }

  @WireType("types.v1/Author")
  public static final class Author {
    public String name = "";
    public List<Book> books = new ArrayList<>();
  }

  @WireType("types.v1/Book")
  public static final class Book {
    public String title = "";
    public @Nullable Author written_by;
  }

  @WireType("types.v1/Alpha")
  public static final class Alpha {
    public @Nullable Beta next;
  }

  @WireType("types.v1/Beta")
  public static final class Beta {
    public @Nullable Gamma next;
  }

  @WireType("types.v1/Gamma")
  public static final class Gamma {
    public List<Alpha> next = new ArrayList<>();
  }

  @WireType("types.v1/Forest")
  public static final class Forest {
    public List<TreeNode> trees = new ArrayList<>();
  }

  @WireType("types.v1/Identified")
  public static final class Identified {
    public UUID id = new UUID(0, 0);
    public Ulid trace = Ulid.ZERO;
    public @Nullable UUID parent;
    public List<Ulid> history = new ArrayList<>();
    public Map<UUID, String> names = new LinkedHashMap<>();
  }

  @WireType("types.v1/Priced")
  public static final class Priced {
    @FixedDecimal(precision = 18, scale = 4)
    public BigDecimal price = BigDecimal.ZERO;

    @FixedDecimal(precision = 5, scale = 0)
    public BigDecimal rate = BigDecimal.ZERO;

    @FixedDecimal(precision = 18, scale = 4)
    public @Nullable BigDecimal maybe_price;

    public BigDecimal amount = BigDecimal.ZERO;
  }

  // A cycle whose spanning tree depends on codepoint (not UTF-16) order: Ａ (U+FF21) and 𝔸
  // (U+1D538). Contract identity only, as in Rust.
  @WireType("types.v1/Ord")
  public static final class CpOrd {
    public @Nullable CpWide wide;
    public @Nullable CpMath math;
  }

  @WireType("types.v1/\uFF21")
  public static final class CpWide {
    public @Nullable CpOrd back;
    public @Nullable CpMath math;
  }

  @WireType("types.v1/\uD835\uDD38")
  public static final class CpMath {
    public @Nullable CpWide wide;
  }

  // The spec's B.4 diamond: A -> C is a back-edge though C is hashed first.
  @WireType("types.v1/DiamondA")
  public static final class DiamondA {
    public @Nullable DiamondB b;
    public @Nullable DiamondC c;
  }

  @WireType("types.v1/DiamondB")
  public static final class DiamondB {
    public @Nullable DiamondC c;
  }

  @WireType("types.v1/DiamondC")
  public static final class DiamondC {
    public List<DiamondA> a = new ArrayList<>();
  }

  static final List<Class<?>> TYPES =
      List.of(
          Temporal.class,
          TreeNode.class,
          Chain.class,
          Author.class,
          Book.class,
          Alpha.class,
          Beta.class,
          Gamma.class,
          Forest.class,
          Identified.class,
          Priced.class,
          DiamondA.class,
          DiamondB.class,
          DiamondC.class,
          CpOrd.class,
          CpWide.class,
          CpMath.class);

  // ── vector ───────────────────────────────────────────────────────────────

  static final JsonNode VECTOR = load();

  private static JsonNode load() {
    Path dir = Path.of("").toAbsolutePath();
    for (int i = 0; i < 8 && dir != null; i++, dir = dir.getParent()) {
      Path p = dir.resolve("conformance/vectors/contract-types.json");
      if (Files.exists(p)) {
        try {
          return new ObjectMapper().readTree(p.toFile()).get("vector");
        } catch (IOException e) {
          throw new IllegalStateException(e);
        }
      }
    }
    throw new IllegalStateException("conformance/vectors/contract-types.json not found");
  }

  private static Class<?> classOf(String fqn) {
    for (Class<?> c : TYPES) {
      if (WireIdentity.of(c).fqn().equals(fqn)) return c;
    }
    throw new AssertionError("no fixture for " + fqn);
  }

  static ForyCodec codec() {
    ForyCodec codec = new ForyCodec();
    for (Class<?> c : TYPES) {
      WireIdentity id = WireIdentity.of(c);
      ((org.apache.fory.ThreadSafeFory) codec.fory())
          .registerCallback(f -> f.register(c, id.packageName(), id.name()));
    }
    return codec;
  }

  private static final ForyCodec CODEC = codec();

  private static byte[] hex(String s) {
    return HexFormat.of().parseHex(s);
  }

  // ── identity ─────────────────────────────────────────────────────────────

  static Stream<String> typeNames() {
    List<String> names = new ArrayList<>();
    VECTOR.get("types").fieldNames().forEachRemaining(names::add);
    return names.stream().sorted();
  }

  @ParameterizedTest
  @MethodSource("typeNames")
  void typeHashMatchesRust(String fqn) {
    var resolved = ContractIdentityResolver.resolve(TypeGraphWalker.walk(TYPES));
    TypeDef td = resolved.typeDefs().get(fqn);
    JsonNode exp = VECTOR.get("types").get(fqn);
    Map<String, FieldDef> byName = new LinkedHashMap<>();
    td.fields().forEach(f -> byName.put(f.name(), f));
    for (JsonNode ef : exp.get("fields_in_declaration_order")) {
      String name = ef.get("name").asText();
      FieldDef f = byName.get(name);
      assertEquals(ef.get("type_kind").asInt(), f.typeKind().code(), name);
      assertEquals(ef.get("type_primitive").asText(), f.typePrimitive(), name);
      assertEquals(ef.get("type_ref").asText(), f.typeRef(), name);
      assertEquals(ef.get("self_ref_name").asText(), f.selfRefName(), name);
      assertEquals(ef.get("optional").asBoolean(), f.optional(), name);
      assertEquals(ef.get("container").asInt(), f.container().code(), name);
    }
    String json = ContractJson.toJson(td);
    assertEquals(
        exp.get("expected_canonical_type_def_hex").asText(),
        HexFormat.of().formatHex(ContractIdentity.computeCanonicalBytes("TypeDef", json)));
    assertEquals(exp.get("expected_type_hash").asText(), resolved.typeHashes().get(fqn));
  }

  @Test
  void typesServiceContractIdMatchesRust() {
    var svc =
        new Service(
            "Types",
            Map.of(
                "echo_temporal", new Class<?>[] {Temporal.class, Temporal.class},
                "plant", new Class<?>[] {TreeNode.class, Forest.class},
                "author", new Class<?>[] {Book.class, Author.class},
                "cycle", new Class<?>[] {Alpha.class, Gamma.class}));
    assertEquals(
        VECTOR.get("expected_contract_id").asText(),
        ContractManifestBuilder.computeContractId(svc));
  }

  @Test
  void idsServiceContractIdMatchesRust() {
    var svc = new Service("Ids", Map.of("price", new Class<?>[] {Identified.class, Priced.class}));
    assertEquals(
        VECTOR.get("expected_ids_contract_id").asText(),
        ContractManifestBuilder.computeContractId(svc));
  }

  @WireType("types.v1/Priced")
  public static final class Priced184 {
    @FixedDecimal(precision = 18, scale = 4)
    public BigDecimal price;
  }

  @WireType("types.v1/Priced")
  public static final class Priced102 {
    @FixedDecimal(precision = 10, scale = 2)
    public BigDecimal price;
  }

  @WireType("types.v1/Priced")
  public static final class PricedAny {
    public BigDecimal price;
  }

  @WireType("types.v1/OneId")
  public static final class OneUuid {
    public UUID id;
  }

  @WireType("types.v1/OneId")
  public static final class OneUlid {
    public Ulid id;
  }

  @WireType("types.v1/OneId")
  public static final class OneBinary {
    public byte[] id;
  }

  @WireType("types.v1/Bad")
  public static final class BadDecimal {
    @FixedDecimal(precision = 2, scale = 3)
    public BigDecimal price;
  }

  private static String hashOf(Class<?> c) {
    var resolved = ContractIdentityResolver.resolve(TypeGraphWalker.walk(List.of(c)));
    return resolved.typeHashes().get(WireIdentity.of(c).fqn());
  }

  @Test
  void decimalPrecisionAndScaleAndIdentifierKindsAreIdentity() {
    var resolved = ContractIdentityResolver.resolve(TypeGraphWalker.walk(List.of(Priced102.class)));
    assertEquals(
        "decimal(10,2)",
        resolved.typeDefs().get("types.v1.Priced").fields().getFirst().typePrimitive());
    assertEquals(
        3,
        Stream.of(hashOf(Priced184.class), hashOf(Priced102.class), hashOf(PricedAny.class))
            .distinct()
            .count());
    assertEquals(
        3,
        Stream.of(hashOf(OneUuid.class), hashOf(OneUlid.class), hashOf(OneBinary.class))
            .distinct()
            .count());
    assertThrows(IllegalArgumentException.class, () -> hashOf(BadDecimal.class));
  }

  // ── payloads ─────────────────────────────────────────────────────────────

  static Stream<String> payloadNames() {
    List<String> names = new ArrayList<>();
    VECTOR.get("payloads").fieldNames().forEachRemaining(names::add);
    return names.stream().sorted();
  }

  @ParameterizedTest
  @MethodSource("payloadNames")
  void rustPayloadDecodesAndJavaWritesRustsBytes(String name) {
    JsonNode c = VECTOR.get("payloads").get(name);
    Class<?> type = classOf(c.get("type").asText());
    byte[] rust = hex(c.get("hex").asText());
    Object got = CODEC.decode(rust, type);
    assertEquals(type, got.getClass());
    assertEquals(c.get("value"), render(got), name);
    byte[] ours = CODEC.encode(got);
    if (BYTE_IDENTICAL.contains(name)) {
      assertEquals(c.get("hex").asText(), HexFormat.of().formatHex(ours), name);
    }
    assertEquals(c.get("value"), render(CODEC.decode(ours, type)), name);
  }

  /**
   * Payloads Java writes byte for byte as Rust does. The others hold a list or a string: Fory Java
   * 1.7.6 marks collection elements nullable in the type meta (there is no non-null element
   * annotation) and writes an ASCII string as LATIN1 where Fory Rust writes UTF-8 (pyfory does the
   * same). Both decode everywhere in compatible mode; neither is a scalar difference.
   */
  static final java.util.Set<String> BYTE_IDENTICAL = java.util.Set.of("priced");

  static Stream<Arguments> decodeCases() {
    List<Arguments> out = new ArrayList<>();
    VECTOR.get("decode_cases").fields().forEachRemaining(e -> out.add(Arguments.of(e.getKey())));
    return out.stream();
  }

  @ParameterizedTest
  @MethodSource("decodeCases")
  void rustDecodeCases(String name) {
    JsonNode c = VECTOR.get("decode_cases").get(name);
    Class<?> type = classOf(c.get("type").asText());
    byte[] payload = hex(c.get("hex").asText());
    if (c.get("refused").asBoolean()) {
      RuntimeException e =
          assertThrows(RuntimeException.class, () -> CODEC.decode(payload, type), name);
      // Refused by the contract rule, not by a malformed payload.
      String why = String.valueOf(e.getMessage()) + " / " + e.getCause();
      assertTrue(
          why.contains("16 bytes") || why.contains("rounding") || why.contains("digits"), why);
    } else {
      assertEquals(c.get("value"), render(CODEC.decode(payload, type)), name);
    }
  }

  @Test
  void encodeRescalesToTheDeclaredScaleWithoutTouchingTheCaller() {
    Priced value = new Priced();
    value.price = new BigDecimal("-12345.67");
    value.rate = new BigDecimal("99999");
    value.maybe_price = new BigDecimal("0.0001");
    value.amount = new BigDecimal("1.5");
    assertEquals(
        VECTOR.get("payloads").get("priced").get("hex").asText(),
        HexFormat.of().formatHex(CODEC.encode(value)));
    assertEquals("-12345.67", value.price.toPlainString());
  }

  @Test
  void encodeRefusesADecimalOutsideItsDeclaration() {
    Priced rounding = new Priced();
    rounding.price = new BigDecimal("1.23456");
    var e = assertThrows(IllegalArgumentException.class, () -> CODEC.encode(rounding));
    assertTrue(e.getMessage().contains("rounding"), e.getMessage());
    Priced tooMany = new Priced();
    tooMany.price = new BigDecimal("123456789012345");
    e = assertThrows(IllegalArgumentException.class, () -> CODEC.encode(tooMany));
    assertTrue(e.getMessage().contains("more than 18 digits"), e.getMessage());
  }

  @Test
  void timestampsAreTruncatedToMicroseconds() {
    Temporal t = new Temporal();
    t.at = Instant.parse("2024-03-01T12:34:56.789012345Z");
    t.history = new ArrayList<>(List.of(Instant.parse("1970-01-01T00:00:00.000000999Z")));
    Temporal back = (Temporal) CODEC.decode(CODEC.encode(t), Temporal.class);
    assertEquals(Instant.parse("2024-03-01T12:34:56.789012Z"), back.at);
    assertEquals(List.of(Instant.EPOCH), back.history);
    assertEquals(345, t.at.getNano() % 1000, "the caller's value is not modified");
  }

  @Test
  void ulidAndUuidForms() {
    Ulid u = Ulid.parse("01arz3ndektsv4rrffq69g5fav");
    assertEquals("01ARZ3NDEKTSV4RRFFQ69G5FAV", u.toString());
    assertEquals(u, Ulid.fromBytes(u.toBytes()));
    assertEquals("00000000000000000000000000", Ulid.ZERO.toString());
    assertEquals("7ZZZZZZZZZZZZZZZZZZZZZZZZZ", Ulid.fromBytes(filled((byte) 0xff)).toString());
    assertThrows(IllegalArgumentException.class, () -> Ulid.parse("81ARZ3NDEKTSV4RRFFQ69G5FAV"));
    assertThrows(IllegalArgumentException.class, () -> Ulid.parse("01ARZ3NDEKTSV4RRFFQ69G5FA"));
    assertThrows(IllegalArgumentException.class, () -> Ulid.parse("01ARZ3NDEKTSV4RRFFQ69G5FAU"));
    assertThrows(IllegalArgumentException.class, () -> Ulid.fromBytes(new byte[15]));
    assertNotEquals(u, Ulid.ZERO);
  }

  private static byte[] filled(byte b) {
    byte[] out = new byte[16];
    java.util.Arrays.fill(out, b);
    return out;
  }

  // ── rendering a decoded value as the vector's JSON form ──────────────────

  private static final DateTimeFormatter MICROS =
      DateTimeFormatter.ofPattern("yyyy-MM-dd'T'HH:mm:ss.SSSSSS'Z'").withZone(ZoneOffset.UTC);

  static JsonNode render(Object v) {
    JsonNodeFactory f = JsonNodeFactory.instance;
    if (v == null) return f.nullNode();
    if (v instanceof Optional<?> o) return o.isEmpty() ? f.nullNode() : render(o.get());
    if (v instanceof String s) return f.textNode(s);
    if (v instanceof Integer i) return f.numberNode(i);
    if (v instanceof Instant t) return f.textNode(MICROS.format(t));
    if (v instanceof LocalDate d) return f.textNode(d.toString());
    if (v instanceof BigDecimal d) return f.textNode(d.toPlainString());
    if (v instanceof UUID u) return f.textNode(u.toString());
    if (v instanceof Ulid u) return f.textNode(u.toString());
    if (v instanceof List<?> l) {
      ArrayNode a = f.arrayNode();
      l.forEach(e -> a.add(render(e)));
      return a;
    }
    if (v instanceof Map<?, ?> m) {
      ObjectNode o = f.objectNode();
      new TreeMap<>(
              m.entrySet().stream()
                  .collect(
                      java.util.stream.Collectors.toMap(
                          e -> render(e.getKey()).asText(), Map.Entry::getValue)))
          .forEach((k, val) -> o.set(k, render(val)));
      return o;
    }
    ObjectNode o = f.objectNode();
    for (Field field : v.getClass().getDeclaredFields()) {
      if (Modifier.isStatic(field.getModifiers())) continue;
      try {
        o.set(FieldNames.contractName(field, field.getName()), render(field.get(v)));
      } catch (IllegalAccessException e) {
        throw new AssertionError(e);
      }
    }
    return o;
  }

  // ── a dispatcher for the contract ids ────────────────────────────────────

  static final class Service implements ServiceDispatcher {
    private final ServiceDescriptor descriptor;
    private final Map<String, MethodDispatcher> methods = new LinkedHashMap<>();
    private final Map<String, Class<?>> req = new LinkedHashMap<>();
    private final Map<String, Class<?>> resp = new LinkedHashMap<>();

    Service(String name, Map<String, Class<?>[]> rpcs) {
      this.descriptor = new ServiceDescriptor(name, 1, Scope.SHARED, Object.class);
      rpcs.forEach(
          (method, types) -> {
            req.put(method, types[0]);
            resp.put(method, types[1]);
            MethodDescriptor d =
                new MethodDescriptor(
                    method,
                    StreamingKind.UNARY,
                    RequestStyle.EXPLICIT,
                    types[0].getAnnotation(WireType.class).value(),
                    List.of(),
                    types[1].getAnnotation(WireType.class).value(),
                    false,
                    false);
            methods.put(
                method,
                new UnaryDispatcher() {
                  @Override
                  public MethodDescriptor descriptor() {
                    return d;
                  }

                  @Override
                  public byte[] invoke(Object impl, byte[] r, Codec c, CallContext ctx) {
                    return new byte[0];
                  }
                });
          });
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
      return req;
    }

    @Override
    public Map<String, Class<?>> responseClasses() {
      return resp;
    }
  }
}
