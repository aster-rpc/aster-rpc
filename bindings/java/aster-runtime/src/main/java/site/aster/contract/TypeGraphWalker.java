package site.aster.contract;

import java.lang.reflect.Field;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.RecordComponent;
import java.lang.reflect.Type;
import java.lang.reflect.WildcardType;
import java.util.ArrayDeque;
import java.util.Collection;
import java.util.Deque;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;

/**
 * Walk a set of root Java types and collect every reachable @WireType-style user type (record,
 * enum, plain class with declared fields) keyed by its language-neutral {@link WireIdentity#fqn()
 * FQN}. Also returns the outgoing-reference adjacency map that {@link ContractIdentityResolver}
 * feeds to Tarjan's SCC algorithm.
 *
 * <p>Roots come from a service dispatcher's method signatures: each method's request type, response
 * type, and any inline-param types. The walker recurses through generic arguments (Optional, List,
 * Set, Map) and record components / plain class fields, stopping at primitives, boxed primitives,
 * {@link String}, byte[], and {@link java.time.Instant} / similar value types.
 *
 * <p>This deliberately does not depend on {@link site.aster.server.spi.ServiceDispatcher} — the
 * walker is pure type-system logic. The pipeline wiring lives one layer up.
 */
public final class TypeGraphWalker {

  private static final String CALL_CONTEXT_FQN = "site.aster.interceptors.CallContext";

  private TypeGraphWalker() {}

  /** Result of walking a type graph: classes by FQN + outgoing reference edges per FQN. */
  public record TypeGraph(Map<String, Class<?>> types, Map<String, Set<String>> refs) {

    public TypeGraph {
      types = Map.copyOf(types);
      Map<String, Set<String>> copy = new LinkedHashMap<>();
      for (var e : refs.entrySet()) {
        copy.put(e.getKey(), Set.copyOf(e.getValue()));
      }
      refs = Map.copyOf(copy);
    }
  }

  /**
   * Walk from {@code roots} and collect every reachable eligible type. Iteration order is the DFS
   * order the walker sees them — matters for deterministic diffs but not for contract_id (Tarjan
   * reorders by SCC).
   */
  public static TypeGraph walk(Iterable<Class<?>> roots) {
    Map<String, Class<?>> types = new LinkedHashMap<>();
    Map<String, Set<String>> refs = new LinkedHashMap<>();
    Deque<Class<?>> work = new ArrayDeque<>();
    for (Class<?> root : roots) {
      if (isEligibleType(root)) {
        work.addLast(root);
      }
    }

    while (!work.isEmpty()) {
      Class<?> cls = work.pollFirst();
      String fqn = WireIdentity.of(cls).fqn();
      if (types.containsKey(fqn)) {
        continue;
      }
      types.put(fqn, cls);
      Set<String> directRefs = new LinkedHashSet<>();
      for (Type fieldType : fieldTypesOf(cls)) {
        collectRefs(fieldType, directRefs, work);
      }
      refs.put(fqn, directRefs);
    }
    return new TypeGraph(types, refs);
  }

  /**
   * Return the field / record-component types of {@code cls} as Java reflective {@link Type}s. For
   * enums we return nothing — enum values are captured elsewhere in {@code TypeDef.enum_values}.
   */
  static List<Type> fieldTypesOf(Class<?> cls) {
    if (cls.isEnum()) {
      return List.of();
    }
    if (cls.isRecord()) {
      List<Type> out = new java.util.ArrayList<>();
      for (RecordComponent rc : cls.getRecordComponents()) {
        out.add(rc.getGenericType());
      }
      return out;
    }
    // Plain class: declared non-static, non-synthetic fields.
    List<Type> out = new java.util.ArrayList<>();
    for (Field f : cls.getDeclaredFields()) {
      if (java.lang.reflect.Modifier.isStatic(f.getModifiers()) || f.isSynthetic()) {
        continue;
      }
      out.add(f.getGenericType());
    }
    return out;
  }

  /**
   * Visit {@code t} and accumulate every eligible terminal class into {@code directRefs}, pushing
   * each onto {@code work} for the outer DFS. Recurses through Optional, List/Set, Map generic
   * arguments.
   */
  static void collectRefs(Type t, Set<String> directRefs, Deque<Class<?>> work) {
    if (t instanceof ParameterizedType pt) {
      Type raw = pt.getRawType();
      if (raw instanceof Class<?> rawCls) {
        if (Collection.class.isAssignableFrom(rawCls)) {
          Type[] args = pt.getActualTypeArguments();
          if (args.length == 1) {
            collectRefs(args[0], directRefs, work);
          }
          return;
        }
        if (Map.class.isAssignableFrom(rawCls)) {
          Type[] args = pt.getActualTypeArguments();
          // Keys may themselves be ref types in weird cases; recurse on both.
          if (args.length == 2) {
            collectRefs(args[0], directRefs, work);
            collectRefs(args[1], directRefs, work);
          }
          return;
        }
        if (Optional.class.equals(rawCls)) {
          Type[] args = pt.getActualTypeArguments();
          if (args.length == 1) {
            collectRefs(args[0], directRefs, work);
          }
          return;
        }
        // Any other parameterised type — fall through with the raw class. Concrete generic args on
        // user-defined records (e.g. record Pair<A,B>(A a, B b)) are rare in wire types and are
        // effectively ANY until we add concrete-binding resolution.
        addIfEligible(rawCls, directRefs, work);
        return;
      }
    }
    if (t instanceof WildcardType wt) {
      Type[] upper = wt.getUpperBounds();
      if (upper.length == 1) {
        collectRefs(upper[0], directRefs, work);
      }
      return;
    }
    if (t instanceof Class<?> c) {
      addIfEligible(c, directRefs, work);
    }
    // TypeVariable / GenericArrayType: treat as ANY — skip.
  }

  private static void addIfEligible(Class<?> cls, Set<String> directRefs, Deque<Class<?>> work) {
    if (!isEligibleType(cls)) {
      return;
    }
    directRefs.add(WireIdentity.of(cls).fqn());
    work.addLast(cls);
  }

  /**
   * A type is eligible for the graph iff it is a user-defined wire carrier: a record, an enum, or a
   * plain class with at least one declared non-static field. Framework primitives, boxed scalars,
   * {@link String}, byte[], time / temporal value types, and {@link site.aster.interceptors
   * .CallContext} are excluded.
   */
  static boolean isEligibleType(Class<?> cls) {
    if (cls.isPrimitive() || cls.isArray()) {
      return false;
    }
    if (cls == String.class || cls == CharSequence.class || cls == Object.class) {
      return false;
    }
    if (Number.class.isAssignableFrom(cls) || cls == Boolean.class || cls == Character.class) {
      return false;
    }
    if (cls.getName().startsWith("java.time.")) {
      return false;
    }
    if (CALL_CONTEXT_FQN.equals(cls.getName())) {
      return false;
    }
    if (Collection.class.isAssignableFrom(cls) || Map.class.isAssignableFrom(cls)) {
      return false;
    }
    if (cls.isRecord() || cls.isEnum()) {
      return true;
    }
    // Plain class: only count as wire carrier if it has at least one instance field. Framework
    // marker interfaces / empty utility classes are out.
    for (Field f : cls.getDeclaredFields()) {
      if (!java.lang.reflect.Modifier.isStatic(f.getModifiers()) && !f.isSynthetic()) {
        return true;
      }
    }
    return false;
  }
}
