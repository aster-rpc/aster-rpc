package site.aster.enrollment;

/**
 * A typed claim value: {@code type} is {@code "bool"}, {@code "int"}, {@code "float"} or {@code
 * "string"} (JSON {@code {"type": …, "value": …}}).
 */
public record Attribute(String type, Object value) {

  public static Attribute string(String v) {
    return new Attribute("string", v);
  }

  public static Attribute integer(long v) {
    return new Attribute("int", v);
  }

  public static Attribute bool(boolean v) {
    return new Attribute("bool", v);
  }

  public static Attribute floating(double v) {
    return new Attribute("float", v);
  }

  /** The value in its display form. */
  @Override
  public String toString() {
    return String.valueOf(value);
  }
}
