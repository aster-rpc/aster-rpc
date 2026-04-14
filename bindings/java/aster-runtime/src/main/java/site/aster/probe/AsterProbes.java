package site.aster.probe;

import java.io.IOException;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import site.aster.ffi.IrohLibrary;

/**
 * Env-gated timing probes for the Stage-1 sequential-unary profiling plan.
 *
 * <p>Enable by setting {@code ASTER_PROBES=1}. When disabled, every recorder is a cheap
 * branch-and-return — no allocation, no synchronisation — so probes can live on the hot path in
 * release builds. When enabled, records are accumulated in two in-memory lists (client / server)
 * and a third Rust-side ring (dumped via FFI).
 */
public final class AsterProbes {

  public static final boolean ENABLED = "1".equals(System.getenv("ASTER_PROBES"));

  private static final List<long[]> CLIENT = new ArrayList<>(4096);
  private static final List<long[]> SERVER = new ArrayList<>(4096);

  private AsterProbes() {}

  public static synchronized void recordClient(long t0, long t1) {
    if (!ENABLED) return;
    CLIENT.add(new long[] {t0, t1});
  }

  public static synchronized void recordServer(long t2, long t3, long t4, long t5, long t6) {
    if (!ENABLED) return;
    SERVER.add(new long[] {t2, t3, t4, t5, t6});
  }

  public static synchronized void reset() {
    CLIENT.clear();
    SERVER.clear();
    if (ENABLED) {
      try {
        RUST_RESET.invoke();
      } catch (Throwable ignored) {
      }
    }
  }

  /** Dump the three CSVs (client, server, rust-unary) into {@code dir}. Creates dir if missing. */
  public static synchronized void dump(Path dir, String stage) throws IOException {
    if (!ENABLED) return;
    Files.createDirectories(dir);
    Path clientCsv = dir.resolve("probes_client_" + stage + ".csv");
    Path serverCsv = dir.resolve("probes_server_" + stage + ".csv");
    Path rustCsv = dir.resolve("probes_rust_" + stage + ".csv");

    StringBuilder c = new StringBuilder("i,t0,t1\n");
    for (int i = 0; i < CLIENT.size(); i++) {
      long[] r = CLIENT.get(i);
      c.append(i).append(',').append(r[0]).append(',').append(r[1]).append('\n');
    }
    Files.writeString(clientCsv, c.toString());

    StringBuilder s = new StringBuilder("i,t2,t3,t4,t5,t6\n");
    for (int i = 0; i < SERVER.size(); i++) {
      long[] r = SERVER.get(i);
      s.append(i)
          .append(',')
          .append(r[0])
          .append(',')
          .append(r[1])
          .append(',')
          .append(r[2])
          .append(',')
          .append(r[3])
          .append(',')
          .append(r[4])
          .append('\n');
    }
    Files.writeString(serverCsv, s.toString());

    try (Arena arena = Arena.ofConfined()) {
      byte[] pathBytes = rustCsv.toString().getBytes(StandardCharsets.UTF_8);
      MemorySegment seg = arena.allocate(pathBytes.length);
      MemorySegment.copy(pathBytes, 0, seg, ValueLayout.JAVA_BYTE, 0, pathBytes.length);
      try {
        RUST_DUMP.invoke(seg, pathBytes.length);
      } catch (Throwable t) {
        throw new IOException("aster_probe_dump_unary_csv failed: " + t.getMessage(), t);
      }
    }
  }

  private static final MethodHandle RUST_RESET =
      IrohLibrary.getInstance().getHandle("aster_probe_reset", FunctionDescriptor.ofVoid());

  private static final MethodHandle RUST_DUMP =
      IrohLibrary.getInstance()
          .getHandle(
              "aster_probe_dump_unary_csv",
              FunctionDescriptor.of(
                  ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_INT));
}
