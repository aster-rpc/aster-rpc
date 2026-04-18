package site.aster.contract;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Set;
import org.junit.jupiter.api.Test;
import site.aster.annotations.WireType;

/**
 * Tests for the Tarjan-based TypeDef resolver. Requires {@code libaster_transport_ffi} for the hash
 * step via {@link ContractIdentity#computeTypeHash}.
 */
final class ContractIdentityResolverTest {

  @WireType("res/Leaf")
  public record Leaf(String name) {}

  @WireType("res/Outer")
  public record Outer(String topLevel, Leaf inner) {}

  @WireType("res/NodeA")
  public record NodeA(String name, NodeB peer) {}

  @WireType("res/NodeB")
  public record NodeB(String name, NodeA back) {}

  @WireType("res/Color")
  public enum Color {
    RED,
    GREEN,
    BLUE
  }

  @WireType("res/Palette")
  public record Palette(String title, Color primary) {}

  // ── Tarjan / SCC ordering ────────────────────────────────────────────────

  @Test
  void tarjanProducesSingleLeafFirstScc() {
    var g = TypeGraphWalker.walk(List.of(Outer.class));
    var sccs = ContractIdentityResolver.tarjanScc(g.refs());
    // Leaves first: Leaf appears before Outer.
    List<String> flat = sccs.stream().flatMap(List::stream).toList();
    int leafIdx = flat.indexOf("res.Leaf");
    int outerIdx = flat.indexOf("res.Outer");
    assertTrue(leafIdx < outerIdx, "Leaf must precede Outer in SCC order: " + flat);
  }

  @Test
  void spanningTreeDfsFindsBackEdgeForCycle() {
    var g = TypeGraphWalker.walk(List.of(NodeA.class));
    var backEdges =
        ContractIdentityResolver.spanningTreeDfs(
            "res.NodeA", List.of("res.NodeA", "res.NodeB"), g.refs());
    // DFS from A → B. B → A is a back-edge (A already visited).
    assertEquals(Set.of(new ContractIdentityResolver.Edge("res.NodeB", "res.NodeA")), backEdges);
  }

  // ── TypeDef construction ─────────────────────────────────────────────────

  @Test
  void leafOuterResolverEmitsRefToPrehashedLeaf() {
    var g = TypeGraphWalker.walk(List.of(Outer.class));
    var resolved = ContractIdentityResolver.resolve(g);

    assertEquals(2, resolved.typeDefs().size());
    TypeDef leaf = resolved.typeDefs().get("res.Leaf");
    TypeDef outer = resolved.typeDefs().get("res.Outer");

    assertEquals(TypeDefKind.MESSAGE, leaf.kind());
    assertEquals(1, leaf.fields().size());
    assertEquals("name", leaf.fields().get(0).name());
    assertEquals(TypeKind.PRIMITIVE, leaf.fields().get(0).typeKind());
    assertEquals("string", leaf.fields().get(0).typePrimitive());

    assertEquals(TypeDefKind.MESSAGE, outer.kind());
    FieldDef innerField =
        outer.fields().stream().filter(f -> f.name().equals("inner")).findFirst().orElseThrow();
    assertEquals(TypeKind.REF, innerField.typeKind());
    // Outer's inner field type_ref must equal Leaf's computed hash.
    assertEquals(resolved.typeHashes().get("res.Leaf"), innerField.typeRef());
  }

  @Test
  void cycleUsesSelfRefForBackEdge() {
    var g = TypeGraphWalker.walk(List.of(NodeA.class));
    var resolved = ContractIdentityResolver.resolve(g);

    TypeDef nodeA = resolved.typeDefs().get("res.NodeA");
    TypeDef nodeB = resolved.typeDefs().get("res.NodeB");

    // Spanning tree rooted at NodeA: A→B is tree edge, B→A is back-edge.
    // So NodeB.back should be SELF_REF to "res.NodeA".
    FieldDef backField =
        nodeB.fields().stream().filter(f -> f.name().equals("back")).findFirst().orElseThrow();
    assertEquals(TypeKind.SELF_REF, backField.typeKind());
    assertEquals("res.NodeA", backField.selfRefName());
    assertEquals("", backField.typeRef());

    // NodeA.peer references NodeB — but NodeB is in the same SCC. With the literal Python port,
    // if NodeA is hashed first (reversed post-order), NodeB isn't in type_hashes yet, so
    // NodeA.peer gets a zero-placeholder REF. That's the documented fallback behaviour.
    FieldDef peerField =
        nodeA.fields().stream().filter(f -> f.name().equals("peer")).findFirst().orElseThrow();
    assertEquals(TypeKind.REF, peerField.typeKind());
    // Either a real hash or the placeholder — both are 64-char hex; assert shape, not value.
    assertTrue(
        peerField.typeRef().matches("[0-9a-f]{64}"),
        "typeRef must be 64-char hex: " + peerField.typeRef());
  }

  @Test
  void enumsGetEnumKindWithOrdinalValues() {
    var g = TypeGraphWalker.walk(List.of(Palette.class));
    var resolved = ContractIdentityResolver.resolve(g);

    TypeDef color = resolved.typeDefs().get("res.Color");
    assertEquals(TypeDefKind.ENUM, color.kind());
    assertEquals(3, color.enumValues().size());
    assertEquals("RED", color.enumValues().get(0).name());
    assertEquals(0, color.enumValues().get(0).value());
    assertEquals("GREEN", color.enumValues().get(1).name());
    assertEquals(1, color.enumValues().get(1).value());
    assertEquals("BLUE", color.enumValues().get(2).name());
    assertEquals(2, color.enumValues().get(2).value());

    TypeDef palette = resolved.typeDefs().get("res.Palette");
    FieldDef primary =
        palette.fields().stream().filter(f -> f.name().equals("primary")).findFirst().orElseThrow();
    assertEquals(TypeKind.REF, primary.typeKind());
    assertEquals(resolved.typeHashes().get("res.Color"), primary.typeRef());
  }

  @Test
  void everyTypeGetsA64CharHexHash() {
    var g = TypeGraphWalker.walk(List.of(Outer.class, Palette.class, NodeA.class));
    var resolved = ContractIdentityResolver.resolve(g);
    for (var entry : resolved.typeHashes().entrySet()) {
      assertTrue(
          entry.getValue().matches("[0-9a-f]{64}"),
          entry.getKey() + " -> " + entry.getValue() + " (must be 64-char hex)");
    }
  }

  @Test
  void differentWireTypeNamesProduceDifferentHashes() {
    // Same shape (single String field) but different wire identity should hash to different
    // type_hash values — contract_id depends on (package, name).
    var g1 = TypeGraphWalker.walk(List.of(Leaf.class));
    var g2 = TypeGraphWalker.walk(List.of(PaletteMember.class));
    String h1 = ContractIdentityResolver.resolve(g1).typeHashes().get("res.Leaf");
    String h2 = ContractIdentityResolver.resolve(g2).typeHashes().get("res.Member");
    assertNotEquals(h1, h2);
  }

  @WireType("res/Member")
  public record PaletteMember(String name) {}

  @Test
  void noNodeAppearsInMultipleSccsOnAcyclicGraph() {
    var g = TypeGraphWalker.walk(List.of(Outer.class));
    var sccs = ContractIdentityResolver.tarjanScc(g.refs());
    Set<String> seen = new java.util.HashSet<>();
    for (List<String> scc : sccs) {
      for (String fqn : scc) {
        assertFalse(seen.contains(fqn), fqn + " in multiple SCCs");
        seen.add(fqn);
      }
    }
  }
}
