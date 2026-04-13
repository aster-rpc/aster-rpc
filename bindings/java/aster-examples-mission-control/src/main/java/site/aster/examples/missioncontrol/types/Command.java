package site.aster.examples.missioncontrol.types;

public record Command(String command) {
  public static final String FORY_TAG = "mission/Command";

  public Command() {
    this("");
  }
}
