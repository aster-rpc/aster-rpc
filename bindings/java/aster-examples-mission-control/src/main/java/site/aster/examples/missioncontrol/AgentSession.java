package site.aster.examples.missioncontrol;

import java.util.List;
import site.aster.codec.Codec;
import site.aster.examples.missioncontrol.types.Assignment;
import site.aster.examples.missioncontrol.types.Command;
import site.aster.examples.missioncontrol.types.CommandResult;
import site.aster.examples.missioncontrol.types.Heartbeat;
import site.aster.server.spi.RequestStream;
import site.aster.server.spi.ResponseStream;

/**
 * Per-agent session-scoped service. SESSION scope — one instance per (peerId, streamId) pair.
 * Mirrors the Python {@code AgentSession} class.
 *
 * <p>Implements all four method shapes from the Python sample, including the bidi-streaming {@code
 * runCommand}. The Java port fakes the shell execution (returns a deterministic "ran:" stdout) so
 * tests don't depend on a real shell — the wire and dispatcher mechanics are identical to the
 * Python version.
 */
public final class AgentSession {

  /**
   * Test-only observation point: records the reason {@link #runCommand} last exited ({@code "EOF"}
   * = request stream closed cleanly, {@code "CANCELLED"} = {@code out.isCancelled()} flipped to
   * true, {@code "EXCEPTION"} = unexpected error). Mutated only by the dispatcher thread; volatile
   * so a test thread observes the latest value. Static because the SESSION-scoped instance is owned
   * by the runtime and not directly accessible to the test fixture.
   */
  public static volatile String lastRunCommandExitReason = "";

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

  /**
   * Test-only method that always throws. Used by the tier-2 chaos suite to prove a handler
   * exception on one session does not poison that session's instance or leak across sessions.
   */
  public Assignment chaosFail(Heartbeat hb) {
    throw new RuntimeException("chaos/expected-throw agentId=" + hb.agentId());
  }

  /**
   * Bidi-streaming: drain commands from {@code in}, "execute" each one (a deterministic fake that
   * returns {@code "ran: <command>"} as stdout), and emit a {@link CommandResult} per command via
   * {@code out}. Returns when {@code in.receive()} reports end-of-stream.
   */
  public void runCommand(RequestStream in, ResponseStream out, Codec codec) throws Exception {
    try {
      while (true) {
        if (out.isCancelled()) {
          lastRunCommandExitReason = "CANCELLED";
          return;
        }
        byte[] payload = in.receive();
        if (payload == null) {
          // Distinguish EOF from "EOF caused by cancellation" — the cancellation flag
          // is set BEFORE the per-call request channel closes, so checking again here
          // catches the "client cancelled while we were blocked in receive()" case.
          if (out.isCancelled()) {
            lastRunCommandExitReason = "CANCELLED";
          } else {
            lastRunCommandExitReason = "EOF";
          }
          return;
        }
        Command cmd = (Command) codec.decode(payload, Command.class);
        CommandResult result = new CommandResult("ran: " + cmd.command(), "", 0);
        out.send(codec.encode(result));
      }
    } catch (Throwable t) {
      lastRunCommandExitReason = "EXCEPTION";
      throw t;
    }
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
