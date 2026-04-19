package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

public record StatusResponse(
    @ForyField(id = 0) String agentId,
    @ForyField(id = 1) String status,
    @ForyField(id = 2) long uptimeSecs) {
  public static final String FORY_TAG = "mission/StatusResponse";

  public StatusResponse() {
    this("", "idle", 0L);
  }
}
