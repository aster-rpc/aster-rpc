package site.aster.ffi;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** C-header-backed oracle for aggregate arguments and scalar arguments after register spill. */
class IrohLibraryByValueTest {
  @TempDir Path temporary;

  @Test
  void publicLowLevelMethodsPassActualCSpansByValue() throws Exception {
    Path root = Path.of("").toAbsolutePath();
    while (root != null && !Files.exists(root.resolve("ffi/aster_ffi.h"))) root = root.getParent();
    assertTrue(root != null, "cannot find ABI header");
    Path source = temporary.resolve("byvalue.c");
    try (var stream = getClass().getResourceAsStream("/byvalue_abi.c")) {
      assertTrue(stream != null, "missing C oracle");
      Files.copy(stream, source);
    }
    boolean mac = System.getProperty("os.name").toLowerCase().contains("mac");
    Path library = temporary.resolve(System.mapLibraryName("byvalue"));
    List<String> compile = new ArrayList<>(List.of("cc", mac ? "-dynamiclib" : "-shared"));
    if (!mac) compile.add("-fPIC");
    compile.addAll(
        List.of("-I" + root.resolve("ffi"), source.toString(), "-o", library.toString()));
    run(compile);
    run(
        List.of(
            Path.of(System.getProperty("java.home"), "bin", "java").toString(),
            "--enable-native-access=ALL-UNNAMED",
            "-Daster.ffi.lib=" + library,
            "-cp",
            System.getProperty("java.class.path"),
            Probe.class.getName()));
  }

  private void run(List<String> command) throws Exception {
    Path log = Files.createTempFile(temporary, "command-", ".log");
    Process process =
        new ProcessBuilder(command).redirectErrorStream(true).redirectOutput(log.toFile()).start();
    if (!process.waitFor(60, TimeUnit.SECONDS)) {
      process.destroyForcibly();
      throw new AssertionError("ABI oracle timed out: " + command);
    }
    assertEquals(0, process.exitValue(), () -> command + "\n" + read(log));
  }

  private static String read(Path path) {
    try {
      return Files.readString(path);
    } catch (Exception e) {
      return e.toString();
    }
  }

  /** Separate JVM keeps the oracle isolated from the normal SDK singleton/library loader. */
  public static class Probe {
    private static void check(int result) {
      if (result != 0) throw new AssertionError("C aggregate oracle rejected call: " + result);
    }

    public static void main(String[] args) {
      IrohLibrary lib = IrohLibrary.getInstance();
      try (Arena arena = Arena.ofConfined()) {
        MemorySegment data = arena.allocateFrom("payload");
        MemorySegment span = arena.allocate(IrohLibrary.ASTER_BYTES);
        span.set(ValueLayout.ADDRESS, 0, data);
        span.set(ValueLayout.JAVA_LONG, 8, 7);
        MemorySegment out = arena.allocate(64);
        check(lib.connectionSendDatagram(11, 22, span));
        check(lib.blobsAddBytesAsCollection(11, 22, span, span, out));
        check(lib.blobsListCollection(11, 22, span, out));
        check(lib.blobsCreateTicket(11, 22, span, out, 64, out));
        check(lib.blobsCreateCollectionTicket(11, 22, span, out, 64, out));
        check(lib.blobsDownload(11, 22, span, out));
        check(lib.blobsAddPath(11, 22, data, 7, out));
        check(lib.blobsAddPathWithNamedTag(11, 22, data, 7, data, 7, out));
        check(lib.blobsAddCollection(11, 22, data, 7, out));
        check(lib.asterRegistryResolve(11, 22, data, 7, 33, out));
        check(
            lib.asterRegistryPublish(
                11, 22, data, 7, data, 7, data, 7, data, 7, 4, data, 7, 44, 33, out));
        check(
            lib.asterRegistryRenewLease(
                11, 22, data, 7, data, 7, data, 7, data, 7, data, 7, .5f, 55, 44, 33, out));
        check(lib.asterRegistryAclAddWriter(11, 22, data, 7, data, 7, 33, out));
        check(lib.asterRegistryAclRemoveWriter(11, 22, data, 7, data, 7, 33, out));
        System.out.println("14 C aggregate downcalls passed");
      }
    }
  }
}
