package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

public record Command(@ForyField(id = 0) String command) {
  public static final String FORY_TAG = "mission/Command";

  public Command() {
    this("");
  }
}
