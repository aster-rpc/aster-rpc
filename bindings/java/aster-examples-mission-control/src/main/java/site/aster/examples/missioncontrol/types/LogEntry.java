package site.aster.examples.missioncontrol.types;

public record LogEntry(double timestamp, String level, String message, String agentId) {
  public static final String FORY_TAG = "mission/LogEntry";

  public LogEntry() {
    this(0.0d, "info", "", "");
  }
}
