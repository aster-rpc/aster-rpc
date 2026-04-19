package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

public record Assignment(@ForyField(id = 0) String taskId, @ForyField(id = 1) String command) {
  public static final String FORY_TAG = "mission/Assignment";

  public Assignment() {
    this("", "");
  }
}
