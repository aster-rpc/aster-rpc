package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

public record TailRequest(@ForyField(id = 0) String agentId, @ForyField(id = 1) String level) {
  public static final String FORY_TAG = "mission/TailRequest";

  public TailRequest() {
    this("", "info");
  }
}
