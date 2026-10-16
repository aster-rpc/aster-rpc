package site.aster.codec;

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.RecordComponent;
import java.lang.reflect.Type;
import java.math.BigDecimal;
import java.math.RoundingMode;
import java.util.ArrayList;
import java.util.Collection;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import site.aster.contract.FixedDecimal;

/**
 * Contract-value rules {@link ForyCodec} applies to every value it encodes and decodes.
 *
 * <p><b>Identifiers in collections.</b> Fory reads a collection element or map key by its wire type
 * id, so a {@code uuid} / {@code ulid} element (BINARY) arrives as {@code byte[]}; it is turned
 * back into the declared {@link java.util.UUID} / {@link Ulid}, refusing any length but 16.
 *
 * <p><b>Timestamps.</b> A {@code timestamp} is pinned to microsecond UTC precision: an {@link
 * java.time.Instant} is truncated to microseconds (its nanoseconds dropped) both ways.
 *
 * <p><b>Fixed decimals.</b> The {@code decimal(p,s)} rule (Aster-ContractIdentity.md, "Time, UUID,
 * and Decimal") for {@link FixedDecimal} fields, applied by {@link ForyCodec} to every value it
 * encodes and decodes: a value is rescaled to scale exactly {@code s} (zero-padded; digits dropped
 * only when they are all zero), and one that would need rounding, or whose rescaled unscaled value
 * has more than {@code p} digits, is refused with an {@link IllegalArgumentException} — never
 * rounded or truncated.
 *
 * <p>The caller's objects are never modified: a value that needs rescaling is rebuilt (records
 * through their canonical constructor, plain classes as a shallow copy). Classes without a {@code
 * FixedDecimal} field anywhere in their graph cost one cached lookup.
 */
public final class ContractValues {
  private ContractValues() {}

  /**
   * Check and rescale {@code value} to its declaration.
   *
   * @throws IllegalArgumentException if a value does not fit its {@code decimal(p,s)}
   */
  public static BigDecimal rescale(BigDecimal value, int precision, int scale, String where) {
    BigDecimal r;
    try {
      r = value.setScale(scale, RoundingMode.UNNECESSARY);
    } catch (ArithmeticException e) {
      throw new IllegalArgumentException(
          where
              + ": "
              + value.toPlainString()
              + " needs rounding to fit decimal("
              + precision
              + ","
              + scale
              + ")");
    }
    int digits = r.unscaledValue().abs().toString().length();
    if (digits > precision) {
      throw new IllegalArgumentException(
          where
              + ": "
              + value.toPlainString()
              + " has more than "
              + precision
              + " digits at scale "
              + scale
              + " (decimal("
              + precision
              + ","
              + scale
              + "))");
    }
    return r;
  }

  /** {@code value} with every {@link FixedDecimal} field checked and rescaled. */
  public static Object normalize(Object value) {
    if (value == null || !Plan.of(value.getClass()).needsWork) {
      return value;
    }
    return normalizeObject(value);
  }

  // ── plan ─────────────────────────────────────────────────────────────────

  private record Slot(Field field, int component, Type type, FixedDecimal decimal, String where) {}

  private static final class Plan {
    private static final ClassValue<Plan> PLANS =
        new ClassValue<>() {
          @Override
          protected Plan computeValue(Class<?> type) {
            return new Plan(type);
          }
        };

    final Class<?> type;
    final List<Slot> slots = new ArrayList<>();
    final boolean needsWork;
    Constructor<?> canonical;

    static Plan of(Class<?> type) {
      return PLANS.get(type);
    }

    private Plan(Class<?> type) {
      this.type = type;
      if (!candidate(type)) {
        needsWork = false;
        return;
      }
      if (type.isRecord()) {
        RecordComponent[] rcs = type.getRecordComponents();
        for (int i = 0; i < rcs.length; i++) {
          RecordComponent rc = rcs[i];
          slots.add(
              new Slot(
                  null,
                  i,
                  rc.getGenericType(),
                  rc.getAnnotation(FixedDecimal.class),
                  type.getSimpleName() + "." + rc.getName()));
        }
      } else {
        for (Class<?> c = type; c != null && c != Object.class; c = c.getSuperclass()) {
          for (Field f : c.getDeclaredFields()) {
            if (Modifier.isStatic(f.getModifiers()) || f.isSynthetic()) continue;
            slots.add(
                new Slot(
                    f,
                    -1,
                    f.getGenericType(),
                    f.getAnnotation(FixedDecimal.class),
                    type.getSimpleName() + "." + f.getName()));
          }
        }
      }
      needsWork = needs(type, new HashSet<>());
    }

    private static boolean candidate(Class<?> type) {
      if (type.isPrimitive() || type.isArray() || type.isEnum() || type.isInterface()) return false;
      String n = type.getName();
      return !(n.startsWith("java.") || n.startsWith("javax.") || n.startsWith("jdk."));
    }

    private static boolean needs(Class<?> type, Set<Class<?>> visiting) {
      if (!candidate(type) || !visiting.add(type)) {
        return false;
      }
      for (Slot s : slotsOf(type)) {
        if (s.decimal() != null || typeNeeds(s.type(), visiting)) {
          return true;
        }
      }
      return false;
    }

    /** Slots without building (and caching) a plan, so recursive graphs terminate. */
    private static List<Slot> slotsOf(Class<?> type) {
      List<Slot> out = new ArrayList<>();
      if (type.isRecord()) {
        for (RecordComponent rc : type.getRecordComponents()) {
          out.add(new Slot(null, 0, rc.getGenericType(), rc.getAnnotation(FixedDecimal.class), ""));
        }
        return out;
      }
      for (Class<?> c = type; c != null && c != Object.class; c = c.getSuperclass()) {
        for (Field f : c.getDeclaredFields()) {
          if (Modifier.isStatic(f.getModifiers()) || f.isSynthetic()) continue;
          out.add(new Slot(f, -1, f.getGenericType(), f.getAnnotation(FixedDecimal.class), ""));
        }
      }
      return out;
    }

    private static boolean typeNeeds(Type t, Set<Class<?>> visiting) {
      if (t instanceof ParameterizedType pt) {
        for (Type arg : pt.getActualTypeArguments()) {
          // A uuid / ulid element or key arrives as byte[] (see the class docs).
          if (arg == java.util.UUID.class || arg == Ulid.class) return true;
          if (typeNeeds(arg, visiting)) return true;
        }
        return false;
      }
      if (t == java.time.Instant.class) return true; // truncated to microseconds
      return t instanceof Class<?> c && needs(c, visiting);
    }

    Constructor<?> canonical() throws NoSuchMethodException {
      if (canonical == null) {
        Class<?>[] types =
            java.util.Arrays.stream(type.getRecordComponents())
                .map(RecordComponent::getType)
                .toArray(Class<?>[]::new);
        Constructor<?> c = type.getDeclaredConstructor(types);
        c.setAccessible(true);
        canonical = c;
      }
      return canonical;
    }
  }

  // ── rewrite ──────────────────────────────────────────────────────────────

  private static Object normalizeObject(Object obj) {
    Plan plan = Plan.of(obj.getClass());
    if (!plan.needsWork) {
      return obj;
    }
    try {
      Object[] values = new Object[plan.slots.size()];
      boolean changed = false;
      for (int i = 0; i < values.length; i++) {
        Slot s = plan.slots.get(i);
        Object v = read(obj, s);
        Object nv = normalizeValue(v, s.type(), s.decimal(), s.where());
        values[i] = nv;
        changed |= nv != v;
      }
      if (!changed) {
        return obj;
      }
      if (plan.type.isRecord()) {
        return plan.canonical().newInstance(values);
      }
      Constructor<?> ctor = plan.type.getDeclaredConstructor();
      ctor.setAccessible(true);
      Object copy = ctor.newInstance();
      for (int i = 0; i < values.length; i++) {
        Field f = plan.slots.get(i).field();
        f.setAccessible(true);
        f.set(copy, values[i]);
      }
      return copy;
    } catch (ReflectiveOperationException e) {
      throw new IllegalStateException(
          "cannot apply decimal(p,s) to " + obj.getClass().getName() + ": " + e, e);
    }
  }

  private static Object read(Object obj, Slot s) throws ReflectiveOperationException {
    if (s.field() == null) {
      return obj.getClass().getRecordComponents()[s.component()].getAccessor().invoke(obj);
    }
    s.field().setAccessible(true);
    return s.field().get(obj);
  }

  private static Object normalizeValue(Object v, Type type, FixedDecimal d, String where) {
    if (v == null) {
      return null;
    }
    if (v instanceof BigDecimal bd) {
      return d == null ? bd : rescale(bd, d.precision(), d.scale(), where);
    }
    if (v instanceof java.time.Instant t) {
      java.time.Instant micros = t.truncatedTo(java.time.temporal.ChronoUnit.MICROS);
      return micros.equals(t) ? t : micros;
    }
    if (v instanceof byte[] raw && (type == java.util.UUID.class || type == Ulid.class)) {
      if (raw.length != 16) {
        throw new IllegalArgumentException(
            where
                + ": a "
                + (type == Ulid.class ? "ulid" : "uuid")
                + " must be 16 bytes, got "
                + raw.length);
      }
      return type == Ulid.class ? Ulid.fromBytes(raw) : ContractScalars.uuidOf(raw);
    }
    Type elem = typeArg(type, 0);
    if (v instanceof Optional<?> o) {
      if (o.isEmpty()) return o;
      Object inner = o.get();
      Object ni = normalizeValue(inner, elem, d, where);
      return ni == inner ? o : Optional.of(ni);
    }
    if (v instanceof List<?> list) {
      List<Object> out = null;
      for (int i = 0; i < list.size(); i++) {
        Object e = list.get(i);
        Object ne = normalizeValue(e, elem, d, where);
        if (ne != e && out == null) {
          out = new ArrayList<>(list.subList(0, i));
        }
        if (out != null) out.add(ne);
      }
      return out == null ? list : out;
    }
    if (v instanceof Set<?> set) {
      Set<Object> out = new LinkedHashSet<>();
      boolean changed = false;
      for (Object e : set) {
        Object ne = normalizeValue(e, elem, d, where);
        changed |= ne != e;
        out.add(ne);
      }
      return changed ? out : set;
    }
    if (v instanceof Collection<?>) {
      return v;
    }
    if (v instanceof Map<?, ?> map) {
      Type valueType = typeArg(type, 1);
      Type keyType = typeArg(type, 0);
      Map<Object, Object> out = new LinkedHashMap<>();
      boolean changed = false;
      for (Map.Entry<?, ?> e : map.entrySet()) {
        Object nk = normalizeValue(e.getKey(), keyType, null, where);
        Object nv = normalizeValue(e.getValue(), valueType, d, where);
        changed |= nv != e.getValue() || nk != e.getKey();
        out.put(nk, nv);
      }
      return changed ? out : map;
    }
    return normalizeObject(v);
  }

  private static Type typeArg(Type t, int i) {
    if (t instanceof ParameterizedType pt && pt.getActualTypeArguments().length > i) {
      return pt.getActualTypeArguments()[i];
    }
    return Object.class;
  }
}
