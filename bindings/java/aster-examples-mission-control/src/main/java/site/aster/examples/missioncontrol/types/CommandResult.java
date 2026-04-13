package site.aster.examples.missioncontrol.types;

public record CommandResult(String stdout, String stderr, int exitCode) {
  public static final String FORY_TAG = "mission/CommandResult";

  public CommandResult() {
    this("", "", -1);
  }
}
