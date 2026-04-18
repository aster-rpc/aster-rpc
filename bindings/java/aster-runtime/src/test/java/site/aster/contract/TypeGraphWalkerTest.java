package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import org.junit.jupiter.api.Test;
import site.aster.annotations.WireType;

/**
 * Type graph walker behaviour. Covers (a) primitive stops, (b) Optional / List / Map recursion, (c)
 * record cycles (A → B → A), (d) enum as leaf, (e) @WireType FQN override, (f) plain-class fields.
 * Every case pinned against Python's walker semantics.
 */
final class TypeGraphWalkerTest {

  @WireType("fixture/Primitive")
  public record PrimitiveOnly(String name, long count, boolean flag) {}

  @WireType("fixture/Inner")
  public record Inner(String value) {}

  @WireType("fixture/Outer")
  public record Outer(String topLevel, Inner inner, List<Inner> innerList) {}

  @WireType("fixture/Container")
  public record Container(List<Inner> items, Map<String, Inner> indexed, Optional<Inner> maybe) {}

  // Two records that refer to each other — classic cycle.
  @WireType("fixture/NodeA")
  public record NodeA(String name, NodeB peer) {}

  @WireType("fixture/NodeB")
  public record NodeB(String name, NodeA back) {}

  @WireType("fixture/ColorEnum")
  public enum Color {
    RED,
    GREEN,
    BLUE
  }

  @WireType("fixture/Palette")
  public record Palette(String title, Color primary, List<Color> palette) {}

  public static final class PlainWithFields {
    public String s;
    public int n;
    public static final String STATIC_CONST = "ignored";
  }

  // ── Basic cases ───────────────────────────────────────────────────────────

  @Test
  void primitiveFieldsProduceNoRefs() {
    var g = TypeGraphWalker.walk(List.of(PrimitiveOnly.class));
    assertEquals(Set.of("fixture.Primitive"), g.types().keySet());
    assertEquals(Set.of(), g.refs().get("fixture.Primitive"));
  }

  @Test
  void directAndNestedRefsCollected() {
    var g = TypeGraphWalker.walk(List.of(Outer.class));
    assertEquals(Set.of("fixture.Outer", "fixture.Inner"), g.types().keySet());
    assertEquals(Set.of("fixture.Inner"), g.refs().get("fixture.Outer"));
    assertEquals(Set.of(), g.refs().get("fixture.Inner"));
  }

  @Test
  void containerAndOptionalUnwrapping() {
    var g = TypeGraphWalker.walk(List.of(Container.class));
    assertEquals(Set.of("fixture.Container", "fixture.Inner"), g.types().keySet());
    assertEquals(Set.of("fixture.Inner"), g.refs().get("fixture.Container"));
  }

  @Test
  void cycleIsCollectedWithoutInfiniteRecursion() {
    var g = TypeGraphWalker.walk(List.of(NodeA.class));
    assertEquals(Set.of("fixture.NodeA", "fixture.NodeB"), g.types().keySet());
    assertEquals(Set.of("fixture.NodeB"), g.refs().get("fixture.NodeA"));
    assertEquals(Set.of("fixture.NodeA"), g.refs().get("fixture.NodeB"));
  }

  @Test
  void enumsAreLeafTypes() {
    var g = TypeGraphWalker.walk(List.of(Palette.class));
    assertEquals(Set.of("fixture.Palette", "fixture.ColorEnum"), g.types().keySet());
    // Enums contribute no outgoing ref edges — they're captured via enum_values in TypeDef.
    assertEquals(Set.of(), g.refs().get("fixture.ColorEnum"));
    assertEquals(Set.of("fixture.ColorEnum"), g.refs().get("fixture.Palette"));
  }

  @Test
  void plainClassWithFieldsIsEligible() {
    var g = TypeGraphWalker.walk(List.of(PlainWithFields.class));
    // No @WireType → default FQN = package + simple name.
    String expected =
        PlainWithFields.class.getPackageName() + "." + PlainWithFields.class.getSimpleName();
    assertEquals(Set.of(expected), g.types().keySet());
    assertEquals(Set.of(), g.refs().get(expected));
  }

  // ── Non-eligibility filters ───────────────────────────────────────────────

  @Test
  void callContextIsSkipped() {
    assertFalse(TypeGraphWalker.isEligibleType(site.aster.interceptors.CallContext.class));
  }

  @Test
  void javaLangAndCollectionsAreSkipped() {
    assertFalse(TypeGraphWalker.isEligibleType(String.class));
    assertFalse(TypeGraphWalker.isEligibleType(Integer.class));
    assertFalse(TypeGraphWalker.isEligibleType(Long.class));
    assertFalse(TypeGraphWalker.isEligibleType(Boolean.class));
    assertFalse(TypeGraphWalker.isEligibleType(List.class));
    assertFalse(TypeGraphWalker.isEligibleType(Map.class));
    assertFalse(TypeGraphWalker.isEligibleType(Object.class));
  }

  @Test
  void enumIsEligibleEvenWithoutFields() {
    assertTrue(TypeGraphWalker.isEligibleType(Color.class));
  }

  @Test
  void recordIsEligible() {
    assertTrue(TypeGraphWalker.isEligibleType(Inner.class));
  }
}
