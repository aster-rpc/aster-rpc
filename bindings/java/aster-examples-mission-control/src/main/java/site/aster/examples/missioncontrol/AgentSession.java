package site.aster.examples.missioncontrol;

import java.util.List;
import site.aster.examples.missioncontrol.types.Assignment;
import site.aster.examples.missioncontrol.types.Heartbeat;

/**
 * Per-agent session-scoped service. SESSION scope — one instance per (peerId, streamId) pair.
 * Mirrors the Python {@code AgentSession} class.
 *
 * <p>Implements the unary methods only ({@code register}, {@code heartbeat}); the bidi-streaming
 * {@code runCommand} method is omitted until reactor read-side multi-frame support lands.
 */
public final class AgentSession {

  private final String peerId;
  private volatile String agentId = "";
  private volatile List<String> capabilities = List.of();

  public AgentSession(String peerId) {
    this.peerId = peerId == null ? "" : peerId;
  }

  public Assignment register(Heartbeat hb) {
    this.agentId = hb.agentId();
    this.capabilities = List.copyOf(hb.capabilities());
    if (capabilities.contains("gpu")) {
      return new Assignment("train-42", "python train.py");
    }
    return new Assignment("idle", "sleep 60");
  }

  public Assignment heartbeat(Heartbeat hb) {
    this.capabilities = List.copyOf(hb.capabilities());
    return new Assignment("continue", "");
  }

  public String peerId() {
    return peerId;
  }

  public String agentId() {
    return agentId;
  }

  public List<String> capabilities() {
    return capabilities;
  }
}
