package site.aster.identity;

import java.util.List;

/**
 * The answer to {@code aster.ops.NodeConfig} set, clear and ack (Rust {@code NodeConfigAck}):
 * {@code complete} only once the generation asked about is stored and in effect.
 */
public record NodeConfigAck(
    boolean complete, long generation, long appliedGeneration, List<Outstanding> outstanding) {

  public NodeConfigAck {
    outstanding = outstanding == null ? List.of() : List.copyOf(outstanding);
  }

  /** A part of a record not (yet) in effect. */
  public record Outstanding(String part, String state, String detail) {}
}
