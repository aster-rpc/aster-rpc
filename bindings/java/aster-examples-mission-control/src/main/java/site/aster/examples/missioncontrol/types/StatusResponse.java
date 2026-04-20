package site.aster.examples.missioncontrol.types;

public record StatusResponse(String agentId, String status, long uptimeSecs) {
  public static final String FORY_TAG = "mission/StatusResponse";

  public StatusResponse() {
    this("", "idle", 0L);
  }
}
