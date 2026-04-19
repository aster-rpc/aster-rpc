package site.aster.examples.missioncontrol.types;

import java.util.List;
import java.util.Objects;
import org.apache.fory.annotation.ForyField;

/**
 * Plain class (not a record) because Fory v0.16 fails to round-trip Java records that carry
 * collection fields like {@code List<String>}. See {@code project_fory_java_records.md} and the
 * regression test {@code StreamHeaderRoundTripTest}.
 */
public final class Heartbeat {
  public static final String FORY_TAG = "mission/Heartbeat";

  @ForyField(id = 0)
  public String agentId = "";

  @ForyField(id = 1)
  public List<String> capabilities = List.of();

  @ForyField(id = 2)
  public double loadAvg;

  public Heartbeat() {}

  public Heartbeat(String agentId, List<String> capabilities, double loadAvg) {
    this.agentId = agentId == null ? "" : agentId;
    this.capabilities = capabilities == null ? List.of() : List.copyOf(capabilities);
    this.loadAvg = loadAvg;
  }

  public String agentId() {
    return agentId;
  }

  public List<String> capabilities() {
    return capabilities;
  }

  public double loadAvg() {
    return loadAvg;
  }

  @Override
  public boolean equals(Object o) {
    if (this == o) return true;
    if (!(o instanceof Heartbeat that)) return false;
    return Double.compare(loadAvg, that.loadAvg) == 0
        && Objects.equals(agentId, that.agentId)
        && Objects.equals(capabilities, that.capabilities);
  }

  @Override
  public int hashCode() {
    return Objects.hash(agentId, capabilities, loadAvg);
  }
}
