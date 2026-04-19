package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

public record LogEntry(
    @ForyField(id = 0) double timestamp,
    @ForyField(id = 1) String level,
    @ForyField(id = 2) String message,
    @ForyField(id = 3) String agentId) {
  public static final String FORY_TAG = "mission/LogEntry";

  public LogEntry() {
    this(0.0d, "info", "", "");
  }
}
