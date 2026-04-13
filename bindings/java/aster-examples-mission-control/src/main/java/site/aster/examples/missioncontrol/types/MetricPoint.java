package site.aster.examples.missioncontrol.types;

import java.util.Map;
import java.util.Objects;

/**
 * Plain class (not a record) because Fory v0.16 fails to round-trip Java records that carry
 * collection fields. Mirrors the Python {@code mission/MetricPoint} type.
 */
public final class MetricPoint {
  public static final String FORY_TAG = "mission/MetricPoint";

  public String name = "";
  public double value;
  public double timestamp;
  public Map<String, String> tags = Map.of();

  public MetricPoint() {}

  public MetricPoint(String name, double value, double timestamp, Map<String, String> tags) {
    this.name = name == null ? "" : name;
    this.value = value;
    this.timestamp = timestamp;
    this.tags = tags == null ? Map.of() : Map.copyOf(tags);
  }

  public String name() {
    return name;
  }

  public double value() {
    return value;
  }

  public double timestamp() {
    return timestamp;
  }

  public Map<String, String> tags() {
    return tags;
  }

  @Override
  public boolean equals(Object o) {
    if (this == o) return true;
    if (!(o instanceof MetricPoint that)) return false;
    return Double.compare(value, that.value) == 0
        && Double.compare(timestamp, that.timestamp) == 0
        && Objects.equals(name, that.name)
        && Objects.equals(tags, that.tags);
  }

  @Override
  public int hashCode() {
    return Objects.hash(name, value, timestamp, tags);
  }
}
