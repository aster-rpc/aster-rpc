package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

public record SubmitLogResult(@ForyField(id = 0) boolean accepted) {
  public static final String FORY_TAG = "mission/SubmitLogResult";

  public SubmitLogResult() {
    this(true);
  }
}
