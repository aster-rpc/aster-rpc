package site.aster.examples.missioncontrol.types;

public record TailRequest(String agentId, String level) {
  public static final String FORY_TAG = "mission/TailRequest";

  public TailRequest() {
    this("", "info");
  }
}
