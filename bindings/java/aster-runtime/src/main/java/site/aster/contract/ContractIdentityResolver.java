package site.aster.contract;

import java.lang.reflect.ParameterizedType;
import java.lang.reflect.RecordComponent;
import java.lang.reflect.Type;
import java.text.Normalizer;
import java.util.ArrayList;
import java.util.Collection;
import java.util.Collections;
import java.util.Comparator;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import java.util.TreeSet;

/**
 * Turn a {@link TypeGraphWalker.TypeGraph} into a map of {@link TypeDef}s with resolved {@code
 * type_ref} hashes, using Tarjan's SCC algorithm to break cycles via {@link TypeKind#SELF_REF}.
 *
 * <p>The algorithm is a direct port of {@code resolve_with_cycles} in {@code
 * bindings/python/aster/contract/identity.py}: SCCs are processed in reverse topological order
 * (leaves first); within a multi-node SCC, a spanning tree rooted at the NFC-smallest member is
 * computed via DFS, and edges not in the spanning tree become back-edges that the field emitter
 * converts to {@code SELF_REF}.
 *
 * <p>Each resolved {@link TypeDef} is canonicalized via {@link ContractIdentity#computeTypeHash}
 * and its 64-char hex digest is stashed in a parallel map so later TypeDefs can embed it in a
 * {@code type_ref}.
 */
public final class ContractIdentityResolver {

  private ContractIdentityResolver() {}

  /** Output of resolution: TypeDef per FQN + its 64-char hex BLAKE3 hash. */
  public record ResolvedTypes(Map<String, TypeDef> typeDefs, Map<String, String> typeHashes) {

    public ResolvedTypes {
      typeDefs = Map.copyOf(typeDefs);
      typeHashes = Map.copyOf(typeHashes);
    }
  }

  public static ResolvedTypes resolve(TypeGraphWalker.TypeGraph graph) {
    Map<String, Set<String>> refs = ensureAllNodesPresent(graph);
    List<List<String>> sccs = tarjanScc(refs);

    Map<String, TypeDef> typeDefs = new LinkedHashMap<>();
    Map<String, String> typeHashes = new LinkedHashMap<>();

    for (List<String> scc : sccs) {
      Set<String> sccSet = Set.copyOf(scc);
      Map<String, Set<String>> backEdgeTargets = new HashMap<>();
      Set<Edge> backEdges = new HashSet<>();

      if (scc.size() == 1) {
        String fqn = scc.get(0);
        if (refs.getOrDefault(fqn, Set.of()).contains(fqn)) {
          backEdges.add(new Edge(fqn, fqn));
          backEdgeTargets.computeIfAbsent(fqn, k -> new HashSet<>()).add(fqn);
        }
      } else {
        List<String> sortedMembers = sortByNfc(scc);
        String start = sortedMembers.get(0);
        backEdges = spanningTreeDfs(start, sortedMembers, refs);
        for (Edge e : backEdges) {
          backEdgeTargets.computeIfAbsent(e.src(), k -> new HashSet<>()).add(e.tgt());
        }
      }

      List<String> order;
      if (scc.size() == 1) {
        order = scc;
      } else {
        List<String> sortedMembers = sortByNfc(scc);
        order = sccProcessingOrder(sortedMembers.get(0), sortedMembers, refs, backEdges);
      }

      for (String fqn : order) {
        Class<?> cls = graph.types().get(fqn);
        Set<String> fqnBackEdges = backEdgeTargets.getOrDefault(fqn, Set.of());
        TypeDef td = buildTypeDef(cls, fqnBackEdges, typeHashes, sccSet);
        String hash = ContractIdentity.computeTypeHash(ContractJson.toJson(td));
        typeDefs.put(fqn, td);
        typeHashes.put(fqn, hash);
      }
    }
    return new ResolvedTypes(typeDefs, typeHashes);
  }

  // ── Graph normalization ────────────────────────────────────────────────────

  private static Map<String, Set<String>> ensureAllNodesPresent(TypeGraphWalker.TypeGraph graph) {
    Map<String, Set<String>> out = new LinkedHashMap<>(graph.refs());
    for (String fqn : graph.types().keySet()) {
      out.computeIfAbsent(fqn, k -> Set.of());
    }
    return out;
  }

  // ── Tarjan's SCC ──────────────────────────────────────────────────────────

  /** Reverse-topological-order SCC list. Matches Python's {@code _tarjan_scc}. */
  static List<List<String>> tarjanScc(Map<String, Set<String>> graph) {
    var state = new TarjanState();
    List<String> keys = new ArrayList<>(graph.keySet());
    Collections.sort(keys);
    for (String v : keys) {
      if (!state.index.containsKey(v)) {
        strongConnect(v, graph, state);
      }
    }
    return state.sccs;
  }

  private static void strongConnect(String v, Map<String, Set<String>> graph, TarjanState state) {
    state.index.put(v, state.counter);
    state.lowlink.put(v, state.counter);
    state.counter++;
    state.stack.push(v);
    state.onStack.put(v, true);

    Set<String> successors = graph.getOrDefault(v, Set.of());
    List<String> sorted = new ArrayList<>(successors);
    Collections.sort(sorted);
    for (String w : sorted) {
      if (!state.index.containsKey(w)) {
        strongConnect(w, graph, state);
        state.lowlink.put(v, Math.min(state.lowlink.get(v), state.lowlink.get(w)));
      } else if (Boolean.TRUE.equals(state.onStack.get(w))) {
        state.lowlink.put(v, Math.min(state.lowlink.get(v), state.index.get(w)));
      }
    }

    if (state.lowlink.get(v).equals(state.index.get(v))) {
      List<String> scc = new ArrayList<>();
      while (true) {
        String w = state.stack.pop();
        state.onStack.put(w, false);
        scc.add(w);
        if (w.equals(v)) {
          break;
        }
      }
      state.sccs.add(scc);
    }
  }

  private static final class TarjanState {
    int counter = 0;
    final Map<String, Integer> index = new HashMap<>();
    final Map<String, Integer> lowlink = new HashMap<>();
    final Map<String, Boolean> onStack = new HashMap<>();
    final java.util.Deque<String> stack = new java.util.ArrayDeque<>();
    final List<List<String>> sccs = new ArrayList<>();
  }

  // ── Back-edge detection via spanning tree ─────────────────────────────────

  /** DFS from {@code start} within {@code members}; returns edges NOT in the spanning tree. */
  static Set<Edge> spanningTreeDfs(
      String start, List<String> members, Map<String, Set<String>> graph) {
    Set<String> memberSet = Set.copyOf(members);
    Set<String> visited = new HashSet<>();
    Set<Edge> backEdges = new LinkedHashSet<>();
    dfsCollectBackEdges(start, memberSet, graph, visited, backEdges);
    return backEdges;
  }

  private static void dfsCollectBackEdges(
      String v,
      Set<String> members,
      Map<String, Set<String>> graph,
      Set<String> visited,
      Set<Edge> backEdges) {
    visited.add(v);
    Set<String> targets = new TreeSet<>();
    for (String t : graph.getOrDefault(v, Set.of())) {
      if (members.contains(t)) {
        targets.add(t);
      }
    }
    for (String w : targets) {
      if (!visited.contains(w)) {
        dfsCollectBackEdges(w, members, graph, visited, backEdges);
      } else {
        backEdges.add(new Edge(v, w));
      }
    }
  }

  // ── Processing order inside an SCC ────────────────────────────────────────

  static List<String> sccProcessingOrder(
      String start, List<String> members, Map<String, Set<String>> graph, Set<Edge> backEdges) {
    Set<String> memberSet = Set.copyOf(members);
    Set<Edge> spanningTreeEdges = new LinkedHashSet<>();
    for (String fqn : members) {
      Set<String> successors = graph.getOrDefault(fqn, Set.of());
      for (String tgt : successors) {
        if (!memberSet.contains(tgt)) {
          continue;
        }
        Edge e = new Edge(fqn, tgt);
        if (!backEdges.contains(e)) {
          spanningTreeEdges.add(e);
        }
      }
    }

    Set<String> visited = new HashSet<>();
    List<String> postOrder = new ArrayList<>();
    dfsPost(start, spanningTreeEdges, visited, postOrder);

    // DFS post-order visits children before parents, i.e. leaves first — exactly the order we
    // need for bottom-up hashing (a parent's REF to a child embeds the child's already-computed
    // hash). Returning the reverse would put SCC roots first and produce zero-placeholder REFs
    // for every forward edge in the root's TypeDef.
    return postOrder;
  }

  private static void dfsPost(
      String v, Set<Edge> spanningTreeEdges, Set<String> visited, List<String> postOrder) {
    visited.add(v);
    Set<String> successors = new TreeSet<>();
    for (Edge e : spanningTreeEdges) {
      if (e.src().equals(v) && !visited.contains(e.tgt())) {
        successors.add(e.tgt());
      }
    }
    for (String w : successors) {
      if (!visited.contains(w)) {
        dfsPost(w, spanningTreeEdges, visited, postOrder);
      }
    }
    postOrder.add(v);
  }

  // ── TypeDef + FieldDef construction ───────────────────────────────────────

  static TypeDef buildTypeDef(
      Class<?> cls, Set<String> backEdges, Map<String, String> typeHashes, Set<String> sccMembers) {
    WireIdentity id = WireIdentity.of(cls);
    if (cls.isEnum()) {
      List<EnumValueDef> values = new ArrayList<>();
      for (Object c : cls.getEnumConstants()) {
        Enum<?> e = (Enum<?>) c;
        values.add(new EnumValueDef(e.name(), e.ordinal()));
      }
      return new TypeDef(
          TypeDefKind.ENUM, id.packageName(), id.name(), List.of(), values, List.of());
    }

    List<FieldDef> fields = new ArrayList<>();
    int idx = 1;
    for (RecordOrClassField f : fieldsOf(cls)) {
      fields.add(buildFieldDef(f.name(), idx, f.type(), backEdges, typeHashes, sccMembers));
      idx++;
    }
    return new TypeDef(
        TypeDefKind.MESSAGE, id.packageName(), id.name(), fields, List.of(), List.of());
  }

  private record RecordOrClassField(String name, Type type) {}

  private static List<RecordOrClassField> fieldsOf(Class<?> cls) {
    List<RecordOrClassField> out = new ArrayList<>();
    if (cls.isRecord()) {
      for (RecordComponent rc : cls.getRecordComponents()) {
        out.add(new RecordOrClassField(rc.getName(), rc.getGenericType()));
      }
      return out;
    }
    for (java.lang.reflect.Field f : cls.getDeclaredFields()) {
      if (java.lang.reflect.Modifier.isStatic(f.getModifiers()) || f.isSynthetic()) {
        continue;
      }
      out.add(new RecordOrClassField(f.getName(), f.getGenericType()));
    }
    return out;
  }

  static FieldDef buildFieldDef(
      String name,
      int id,
      Type t,
      Set<String> backEdges,
      Map<String, String> typeHashes,
      Set<String> sccMembers) {
    // Unwrap Optional<X>.
    boolean optional = false;
    Type inner = t;
    if (t instanceof ParameterizedType pt
        && pt.getRawType() instanceof Class<?> rawCls
        && Optional.class.equals(rawCls)) {
      optional = true;
      Type[] args = pt.getActualTypeArguments();
      if (args.length == 1) {
        inner = args[0];
      }
    }

    ContainerKind container = ContainerKind.NONE;
    TypeKind containerKeyKind = TypeKind.PRIMITIVE;
    String containerKeyPrimitive = "";
    String containerKeyRef = "";

    Type valueType = inner;
    if (inner instanceof ParameterizedType pt && pt.getRawType() instanceof Class<?> rawCls) {
      if (Collection.class.isAssignableFrom(rawCls)) {
        container = Set.class.isAssignableFrom(rawCls) ? ContainerKind.SET : ContainerKind.LIST;
        Type[] args = pt.getActualTypeArguments();
        valueType = args.length == 1 ? args[0] : Object.class;
      } else if (Map.class.isAssignableFrom(rawCls)) {
        container = ContainerKind.MAP;
        Type[] args = pt.getActualTypeArguments();
        Type keyType = args.length >= 2 ? args[0] : Object.class;
        valueType = args.length >= 2 ? args[1] : Object.class;
        var keyResolved = resolveType(keyType, backEdges, typeHashes, sccMembers);
        containerKeyKind = keyResolved.kind;
        containerKeyPrimitive = keyResolved.primitive;
        containerKeyRef = keyResolved.typeRefHex;
      }
    }

    ResolvedType resolved = resolveType(valueType, backEdges, typeHashes, sccMembers);

    return new FieldDef(
        id,
        name,
        resolved.kind,
        resolved.primitive,
        resolved.typeRefHex,
        resolved.selfRefName,
        optional,
        false,
        container,
        containerKeyKind,
        containerKeyPrimitive,
        containerKeyRef,
        true,
        "");
  }

  private record ResolvedType(
      TypeKind kind, String primitive, String typeRefHex, String selfRefName) {}

  private static final Map<Class<?>, String> PRIMITIVE_WIRE_NAMES = primitiveMap();

  private static Map<Class<?>, String> primitiveMap() {
    Map<Class<?>, String> m = new HashMap<>();
    m.put(String.class, "string");
    m.put(boolean.class, "bool");
    m.put(Boolean.class, "bool");
    m.put(byte.class, "int8");
    m.put(Byte.class, "int8");
    m.put(short.class, "int16");
    m.put(Short.class, "int16");
    m.put(int.class, "int32");
    m.put(Integer.class, "int32");
    m.put(long.class, "int64");
    m.put(Long.class, "int64");
    m.put(float.class, "float32");
    m.put(Float.class, "float32");
    m.put(double.class, "float64");
    m.put(Double.class, "float64");
    return Map.copyOf(m);
  }

  private static ResolvedType resolveType(
      Type t, Set<String> backEdges, Map<String, String> typeHashes, Set<String> sccMembers) {
    Class<?> cls = terminalClass(t);
    if (cls == null) {
      return new ResolvedType(TypeKind.ANY, "", "", "");
    }
    if (cls == byte[].class || cls == Byte[].class) {
      return new ResolvedType(TypeKind.PRIMITIVE, "binary", "", "");
    }
    String primitive = PRIMITIVE_WIRE_NAMES.get(cls);
    if (primitive != null) {
      return new ResolvedType(TypeKind.PRIMITIVE, primitive, "", "");
    }
    if (cls.isRecord() || cls.isEnum() || TypeGraphWalker.isEligibleType(cls)) {
      String fqn = WireIdentity.of(cls).fqn();
      if (backEdges.contains(fqn)) {
        return new ResolvedType(TypeKind.SELF_REF, "", "", fqn);
      }
      String hash = typeHashes.get(fqn);
      if (hash != null) {
        return new ResolvedType(TypeKind.REF, "", hash, "");
      }
      // With sccProcessingOrder visiting leaves first, every forward edge MUST already be in
      // typeHashes. Hitting this branch means either the back-edge classifier missed something
      // or the walker failed to discover this type — both are bugs, raise loudly.
      throw new IllegalStateException(
          "Could not resolve type_ref for "
              + fqn
              + " (Java class: "
              + cls.getName()
              + "). Type is reachable but was not hashed before the parent TypeDef. "
              + "Known hashes: "
              + new java.util.TreeSet<>(typeHashes.keySet()));
    }
    return new ResolvedType(TypeKind.ANY, "", "", "");
  }

  private static Class<?> terminalClass(Type t) {
    if (t instanceof Class<?> c) {
      return c;
    }
    if (t instanceof ParameterizedType pt && pt.getRawType() instanceof Class<?> raw) {
      return raw;
    }
    return null;
  }

  // ── Misc helpers ──────────────────────────────────────────────────────────

  /** NFC codepoint ordering for FQN strings — matches Python's sort key. */
  static List<String> sortByNfc(List<String> items) {
    List<String> copy = new ArrayList<>(items);
    copy.sort(Comparator.comparing(s -> codepointsNfc(s), ContractIdentityResolver::compareInts));
    return copy;
  }

  private static int[] codepointsNfc(String s) {
    return Normalizer.normalize(s, Normalizer.Form.NFC).codePoints().toArray();
  }

  private static int compareInts(int[] a, int[] b) {
    int n = Math.min(a.length, b.length);
    for (int i = 0; i < n; i++) {
      int cmp = Integer.compare(a[i], b[i]);
      if (cmp != 0) {
        return cmp;
      }
    }
    return Integer.compare(a.length, b.length);
  }

  /** (source, target) pair for graph edges. */
  public record Edge(String src, String tgt) {}
}
