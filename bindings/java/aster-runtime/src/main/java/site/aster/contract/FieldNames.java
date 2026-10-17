package site.aster.contract;

import java.lang.reflect.AnnotatedElement;
import java.util.List;
import org.apache.fory.util.StringUtils;
import site.aster.annotations.WireName;

/**
 * Field names ({@code ffi_spec/Aster-FieldNaming.md}). A field's contract name is the Java member
 * name, verbatim, or its {@link WireName} when the contract is defined in another language. Fory
 * Java matches fields by its snake_case form of the member name, so contract definition refuses a
 * name that is not plain, a member whose Fory form differs from the reference snake_case form of
 * its name (a member starting with a capital: Fory Java writes {@code Value} as {@code _value}),
 * and two fields with the same snake_case form.
 */
public final class FieldNames {
  private FieldNames() {}

  /** One field: Fory's snake_case form of the member, and the contract name. */
  public record Entry(String memberSnake, String name) {}

  /** A member's contract name: its {@link WireName}, else its declared name. */
  public static String contractName(AnnotatedElement member, String declared) {
    WireName w = member.getAnnotation(WireName.class);
    return w != null ? w.value() : declared;
  }

  /**
   * Fory Java's snake_case form of a member name ({@code StringUtils.lowerCamelToLowerUnderscore}).
   */
  public static String forySnakeCase(String name) {
    return StringUtils.lowerCamelToLowerUnderscore(name);
  }

  private static boolean asciiUpper(int c) {
    return c >= 'A' && c <= 'Z';
  }

  private static boolean asciiLowerOrDigit(int c) {
    return (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9');
  }

  /** The reference snake_case form (Rust fory-core / Fory JavaScript). */
  public static String referenceSnakeCase(String name) {
    int[] cs = name.codePoints().toArray();
    StringBuilder b = new StringBuilder(name.length() + 4);
    for (int i = 0; i < cs.length; i++) {
      int c = cs[i];
      if (asciiUpper(c)) {
        if (i > 0) {
          boolean prevUpper = asciiUpper(cs[i - 1]);
          boolean nextUpperOrEnd = i + 1 >= cs.length || asciiUpper(cs[i + 1]);
          if (!prevUpper || !nextUpperOrEnd) {
            b.append('_');
          }
        }
        b.append((char) (c + 32));
      } else {
        b.appendCodePoint(c);
      }
    }
    return b.toString();
  }

  /**
   * Why {@code name} is not plain, or null when it is: every ASCII capital starts the name or
   * follows an ASCII lowercase letter or a digit, no non-ASCII letter is uppercase, and the name
   * does not end in {@code _}.
   */
  public static String notPlain(String name) {
    int[] cs = name.codePoints().toArray();
    for (int i = 0; i < cs.length; i++) {
      if (asciiUpper(cs[i]) && i > 0 && !asciiLowerOrDigit(cs[i - 1])) {
        return "an uppercase letter follows another uppercase letter, `_` or a non-ASCII character";
      }
      if (cs[i] > 127 && Character.isUpperCase(cs[i])) {
        return "it has an uppercase letter outside ASCII";
      }
    }
    return name.endsWith("_") ? "it ends in `_`" : null;
  }

  /** A plain spelling of {@code name} ({@code userID} → {@code userId}). */
  public static String plainSuggestion(String name) {
    int[] rs = name.codePoints().toArray();
    int i = 0;
    while (i < rs.length) {
      if (!asciiUpper(rs[i])) {
        if (rs[i] > 127 && Character.isUpperCase(rs[i])) {
          rs[i] = Character.toLowerCase(rs[i]);
        }
        i++;
        continue;
      }
      int end = i;
      while (end < rs.length && asciiUpper(rs[end])) {
        end++;
      }
      boolean nextLower = end < rs.length && rs[end] >= 'a' && rs[end] <= 'z';
      boolean keepLast = nextLower && end - i >= 3;
      boolean firstOk = i == 0 || asciiLowerOrDigit(rs[i - 1]);
      for (int k = i; k < end; k++) {
        if (!((k == i && firstOk) || (k == end - 1 && keepLast))) {
          rs[k] = Character.toLowerCase(rs[k]);
        }
      }
      i = end;
    }
    String out = new String(rs, 0, rs.length);
    return out.replaceAll("_+$", "");
  }

  /**
   * Check one type's fields. Throws {@link IllegalArgumentException} naming the first offending
   * field.
   */
  public static void check(List<Entry> fields) {
    for (Entry f : fields) {
      String why = notPlain(f.name());
      if (why != null) {
        throw new IllegalArgumentException(
            "\""
                + f.name()
                + "\" is not a plain field name: "
                + why
                + ", so the Fory implementations disagree on its snake_case form. Use a plain"
                + " spelling such as \""
                + plainSuggestion(f.name())
                + "\" (ffi_spec/Aster-FieldNaming.md §5)");
      }
      String snake = referenceSnakeCase(f.name());
      if (!snake.equals(f.memberSnake())) {
        throw new IllegalArgumentException(
            "the field name \""
                + f.name()
                + "\" does not match its member: Fory writes the member as \""
                + f.memberSnake()
                + "\" but \""
                + f.name()
                + "\" is \""
                + snake
                + "\". Name the member so its snake_case form is \""
                + snake
                + "\", with @WireName(\""
                + f.name()
                + "\") (ffi_spec/Aster-FieldNaming.md §4)");
      }
    }
    for (int i = 0; i < fields.size(); i++) {
      for (int j = 0; j < i; j++) {
        String a = fields.get(j).name();
        String b = fields.get(i).name();
        if (referenceSnakeCase(a).equals(referenceSnakeCase(b))) {
          throw new IllegalArgumentException(
              "the fields \""
                  + a
                  + "\" and \""
                  + b
                  + "\" collide: Fory writes both as \""
                  + referenceSnakeCase(b)
                  + "\" and could not tell them apart (ffi_spec/Aster-FieldNaming.md §4)");
        }
      }
    }
  }

  /** {@link #check(List)} for a type, prefixing the message with the class name. */
  static void check(Class<?> cls, List<Entry> fields) {
    try {
      check(fields);
    } catch (IllegalArgumentException e) {
      throw new IllegalArgumentException(cls.getName() + ": " + e.getMessage(), e);
    }
  }
}
