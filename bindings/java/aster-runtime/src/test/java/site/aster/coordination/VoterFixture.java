package site.aster.coordination;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;
import site.aster.config.AsterConfig;
import site.aster.node.IrohNode;
import site.aster.node.NodeAddr;

/**
 * A real single-voter coordination group in its own process: the {@code coordination-voter} test
 * binary of the {@code aster} crate (feature {@code coordination-testing}). Build it with
 *
 * <pre>
 *   /Users/emrul/dev/ai/rust-dev/scripts/with-kache cargo build -p aster \
 *       --features coordination-testing --bin coordination-voter
 * </pre>
 *
 * and the live tests find {@code target/debug/coordination-voter} above the working directory (or
 * the {@code aster.coordination.voter} system property). Without the binary they are skipped.
 *
 * <p>The group is deterministic from {@code SEED}: group id {@code [SEED; 32]}, ROOT authority
 * secret {@code [SEED ^ 0xFF; 32]} (the administrator: start a node with it to grant others
 * rights), voter secret {@code [SEED ^ 0x10; 32]}, authority domain {@code process-loss.test}.
 */
final class VoterFixture {

  static final int SEED = 0xF4;
  static final String DOMAIN = "process-loss.test";

  private static VoterFixture instance;

  private final Process process;
  final Path control;
  final String ticket;
  final byte[] groupId = new byte[32];

  private VoterFixture(Process process, Path control, String ticket) {
    this.process = process;
    this.control = control;
    this.ticket = ticket;
    Arrays.fill(groupId, (byte) SEED);
  }

  static byte[] filled(int v) {
    byte[] b = new byte[32];
    Arrays.fill(b, (byte) v);
    return b;
  }

  static byte[] rootSecret() {
    return filled(SEED ^ 0xFF);
  }

  /** The running group, or null when the voter binary is not built. */
  static synchronized VoterFixture get() throws Exception {
    if (instance == null) {
      Path bin = findBinary();
      if (bin == null) {
        return null;
      }
      instance = start(bin);
      Runtime.getRuntime().addShutdownHook(new Thread(() -> instance.process.destroyForcibly()));
    }
    return instance;
  }

  private static Path findBinary() {
    String prop = System.getProperty("aster.coordination.voter");
    if (prop != null && Files.isExecutable(Path.of(prop))) {
      return Path.of(prop);
    }
    Path dir = Path.of("").toAbsolutePath();
    for (int i = 0; i < 8 && dir != null; i++, dir = dir.getParent()) {
      Path p = dir.resolve("target").resolve("debug").resolve("coordination-voter");
      if (Files.isExecutable(p)) {
        return p;
      }
    }
    return null;
  }

  private static VoterFixture start(Path bin) throws Exception {
    Path control = Files.createTempDirectory("aster-coordination-voter");
    Process p =
        new ProcessBuilder(bin.toString(), control.toString(), "0", "1", Integer.toString(SEED))
            .redirectErrorStream(true)
            .redirectOutput(control.resolve("voter.log").toFile())
            .start();
    long end = System.nanoTime() + TimeUnit.SECONDS.toNanos(90);
    String ticket = null;
    while (System.nanoTime() < end) {
      if (!p.isAlive()) {
        throw new IllegalStateException(
            "coordination-voter exited: " + Files.readString(control.resolve("voter.log")));
      }
      Path addr = control.resolve("addr-0");
      Path status = control.resolve("status-0");
      if (ticket == null && Files.exists(addr)) {
        String[] parts = Files.readString(addr).trim().split("\\s+");
        if (parts.length > 1) {
          ticket = new NodeAddr(parts[0], null, List.of(parts).subList(1, parts.length)).toTicket();
        }
      }
      if (ticket != null && Files.exists(status)) {
        String[] s = Files.readString(status).trim().split("\\s+");
        // "<lifecycle> <quorum> <leader-index> <applied>": ready once a quorum is available
        if (s.length >= 2 && s[1].equals("yes")) {
          return new VoterFixture(p, control, ticket);
        }
      }
      Thread.sleep(100);
    }
    p.destroyForcibly();
    throw new IllegalStateException("the voter never became ready");
  }

  /** Start a node with {@code secret} as its identity (loopback, relays off). */
  static IrohNode node(byte[] secret) throws Exception {
    AsterConfig cfg =
        AsterConfig.builder()
            .relayMode("disabled")
            .bindAddr("127.0.0.1:0")
            .secretKey(secret)
            .build();
    return IrohNode.start(cfg).get(30, TimeUnit.SECONDS);
  }

  /** A fresh actor identity per call. */
  static byte[] actorSecret() {
    byte[] s = filled(0x5A);
    long n = NEXT.incrementAndGet();
    for (int i = 0; i < 8; i++) {
      s[i] = (byte) (n >>> (8 * i));
    }
    s[8] = (byte) (ProcessHandle.current().pid() & 0xff);
    s[9] = (byte) (System.nanoTime() & 0xff);
    return s;
  }

  private static final java.util.concurrent.atomic.AtomicLong NEXT =
      new java.util.concurrent.atomic.AtomicLong(System.nanoTime());

  CoordinationClient.Options durable(Path journal) {
    return CoordinationClient.Options.durable(groupId, journal, List.of(ticket));
  }

  CoordinationClient.Options readOnly() {
    return CoordinationClient.Options.readOnly(groupId, List.of(ticket));
  }
}
