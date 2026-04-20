package site.aster.examples.missioncontrol.types;

public record IngestResult(int accepted, int dropped) {
  public static final String FORY_TAG = "mission/IngestResult";

  public IngestResult() {
    this(0, 0);
  }
}
