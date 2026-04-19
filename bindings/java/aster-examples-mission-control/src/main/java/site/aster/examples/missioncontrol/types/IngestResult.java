package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

public record IngestResult(@ForyField(id = 0) int accepted, @ForyField(id = 1) int dropped) {
  public static final String FORY_TAG = "mission/IngestResult";

  public IngestResult() {
    this(0, 0);
  }
}
