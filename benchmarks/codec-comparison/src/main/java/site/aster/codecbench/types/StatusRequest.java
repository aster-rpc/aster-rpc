package site.aster.codecbench.types;

public record StatusRequest(String agentId) {
  public static final String FORY_TAG = "mission/StatusRequest";

  public StatusRequest() {
    this("");
  }
}
