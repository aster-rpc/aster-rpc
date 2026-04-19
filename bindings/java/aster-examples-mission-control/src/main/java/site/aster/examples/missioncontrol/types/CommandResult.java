package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

public record CommandResult(
    @ForyField(id = 0) String stdout,
    @ForyField(id = 1) String stderr,
    @ForyField(id = 2) int exitCode) {
  public static final String FORY_TAG = "mission/CommandResult";

  public CommandResult() {
    this("", "", -1);
  }
}
