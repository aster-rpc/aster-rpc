package site.aster.examples.missioncontrol.types;

import org.apache.fory.annotation.ForyField;

/**
 * {@code @ForyField(id=N)} on every component keeps the Fory struct fingerprint tag-ID based, so
 * Java's snake-case field-name conversion doesn't make its schema hash diverge from Python's (which
 * uses the raw field name). See {@code site.aster.server.wire.RpcStatus} for the same pattern.
 */
public record StatusRequest(@ForyField(id = 0) String agentId) {
  public static final String FORY_TAG = "mission/StatusRequest";

  public StatusRequest() {
    this("");
  }
}
