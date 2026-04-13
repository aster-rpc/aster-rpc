package site.aster.examples.missioncontrol.types;

public record Assignment(String taskId, String command) {
  public static final String FORY_TAG = "mission/Assignment";

  public Assignment() {
    this("", "");
  }
}
