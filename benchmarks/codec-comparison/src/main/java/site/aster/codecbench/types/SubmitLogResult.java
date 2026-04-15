package site.aster.codecbench.types;

public record SubmitLogResult(boolean accepted) {
  public static final String FORY_TAG = "mission/SubmitLogResult";

  public SubmitLogResult() {
    this(true);
  }
}
