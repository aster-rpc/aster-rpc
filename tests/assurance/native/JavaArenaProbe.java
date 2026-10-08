import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.regex.Pattern;
import site.aster.handle.IrohConnection;
import site.aster.handle.IrohEndpoint;
import site.aster.handle.IrohStream;
import site.aster.node.IrohNode;

/** Standalone NMT control: 8 MiB of completed sends must not retain FFM allocation arenas. */
public final class JavaArenaProbe {
  private static String nmt(String... args) throws Exception {
    var command = new java.util.ArrayList<String>();
    command.add(System.getProperty("java.home") + "/bin/jcmd");
    command.add(Long.toString(ProcessHandle.current().pid()));
    command.add("VM.native_memory");
    command.addAll(List.of(args));
    var process = new ProcessBuilder(command).redirectErrorStream(true).start();
    if (!process.waitFor(10, TimeUnit.SECONDS)) {
      process.destroyForcibly();
      throw new AssertionError("owned jcmd timed out");
    }
    var output = new String(process.getInputStream().readAllBytes(), StandardCharsets.UTF_8);
    if (process.exitValue() != 0 || output.contains("not enabled")) {
      throw new AssertionError(output);
    }
    return output;
  }

  public static void main(String[] args) throws Exception {
    final int chunkBytes = 64 * 1024;
    final int chunks = 128;
    final int totalBytes = chunkBytes * chunks;
    var alpn = "aster-test/arena";
    var protocols = List.of(alpn.getBytes(StandardCharsets.UTF_8));
    var a = IrohNode.memoryWithAlpns(protocols).get(20, TimeUnit.SECONDS);
    var b = IrohNode.memoryWithAlpns(protocols).get(20, TimeUnit.SECONDS);
    var epA = IrohEndpoint.ofNode(a.runtime(), a.nodeHandle());
    var epB = IrohEndpoint.ofNode(b.runtime(), b.nodeHandle());
    IrohConnection out = null, in = null;
    IrohStream send = null, receive = null;
    try {
      var accepted = epA.acceptAsync();
      out = epB.connectNodeAddrAsync(epA.addrInfo(), alpn).get(20, TimeUnit.SECONDS);
      in = accepted.get(20, TimeUnit.SECONDS);
      var streamAccepted = in.acceptBiAsync();
      send = out.openBiAsync().get(5, TimeUnit.SECONDS);
      byte[] payload = new byte[chunkBytes];
      for (int i = 0; i < payload.length; i++) payload[i] = (byte) (i * 31);
      // Publish the QUIC stream and warm the native/FFM path before the NMT baseline.
      send.sendAsync(payload).get(5, TimeUnit.SECONDS);
      receive = streamAccepted.get(5, TimeUnit.SECONDS);
      var bytes = receive.readToEndAsync(totalBytes + 1L);
      System.out.println(nmt("baseline"));
      for (int i = 1; i < chunks; i++) send.sendAsync(payload).get(5, TimeUnit.SECONDS);
      send.finishAsync().get(5, TimeUnit.SECONDS);
      byte[] actual = bytes.get(5, TimeUnit.SECONDS);
      if (actual.length != totalBytes) throw new AssertionError("received " + actual.length);
      for (int i = 0; i < actual.length; i++) {
        if (actual[i] != payload[i % chunkBytes]) throw new AssertionError("byte mismatch " + i);
      }
      String diff = nmt("summary.diff", "scale=KB");
      System.out.println(diff);
      var other = Pattern.compile("(?m)^-\\s+Other \\(([^\\n]+)\\)").matcher(diff);
      if (!other.find()) throw new AssertionError("NMT Other allocation category missing");
      var delta = Pattern.compile("committed=\\d+KB(?: ([+-]\\d+)KB)?").matcher(other.group(1));
      if (!delta.find()) throw new AssertionError("NMT Other committed delta missing");
      long retainedKiB = delta.group(1) == null ? 0 : Long.parseLong(delta.group(1));
      System.out.println("arena_probe bytes=" + totalBytes + " retainedOtherKiB=" + retainedKiB);
      // Fixed scoped arenas retain no payload-sized allocation. Allow 1 MiB bookkeeping noise.
      if (retainedKiB > 1024) throw new AssertionError("retained FFM payload allocations " + retainedKiB + " KiB");
    } finally {
      if (send != null) send.close();
      if (receive != null) receive.close();
      if (out != null) out.close();
      if (in != null) in.close();
      epB.closeAsync().get(20, TimeUnit.SECONDS);
      epA.closeAsync().get(20, TimeUnit.SECONDS);
      a.close();
      b.close();
    }
  }
}
