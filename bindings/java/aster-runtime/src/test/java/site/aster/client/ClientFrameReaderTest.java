package site.aster.client;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.ArrayDeque;
import java.util.Arrays;
import java.util.Deque;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.function.LongFunction;
import org.junit.jupiter.api.Test;
import site.aster.server.AsterFraming;

/** Pure-Java unit tests for {@link ClientFrameReader} — no FFI required. */
final class ClientFrameReaderTest {

  /** Queues a pre-scripted sequence of chunks, returning them one readAsync call at a time. */
  private static LongFunction<CompletableFuture<byte[]>> scriptedReader(byte[]... chunks) {
    Deque<byte[]> queue = new ArrayDeque<>(Arrays.asList(chunks));
    return maxLen -> {
      byte[] next = queue.pollFirst();
      if (next == null) {
        return CompletableFuture.completedFuture(new byte[0]);
      }
      return CompletableFuture.completedFuture(next);
    };
  }

  @Test
  void readsFrameDeliveredAsSingleChunk() throws Exception {
    byte[] payload = "hello".getBytes();
    byte[] frame = AsterFraming.encodeFrame(payload, AsterFraming.FLAG_HEADER);
    ClientFrameReader reader = new ClientFrameReader(scriptedReader(frame));

    ClientFrameReader.Frame result = reader.readFrame().get();

    assertArrayEquals(payload, result.payload());
    assertEquals(AsterFraming.FLAG_HEADER, result.flags());
  }

  @Test
  void readsFrameSplitAcrossMultipleChunks() throws Exception {
    byte[] payload = new byte[1024];
    for (int i = 0; i < payload.length; i++) {
      payload[i] = (byte) (i & 0xFF);
    }
    byte[] frame = AsterFraming.encodeFrame(payload, (byte) 0);

    // Split mid-header (byte 2 of 4) and mid-payload (byte 40 of 1025).
    byte[] chunkA = Arrays.copyOfRange(frame, 0, 2);
    byte[] chunkB = Arrays.copyOfRange(frame, 2, 40);
    byte[] chunkC = Arrays.copyOfRange(frame, 40, frame.length);

    ClientFrameReader reader = new ClientFrameReader(scriptedReader(chunkA, chunkB, chunkC));

    ClientFrameReader.Frame result = reader.readFrame().get();
    assertArrayEquals(payload, result.payload());
    assertEquals((byte) 0, result.flags());
  }

  @Test
  void readsTwoFramesFromOneBufferedChunk() throws Exception {
    byte[] p1 = "first".getBytes();
    byte[] p2 = "second".getBytes();
    byte[] f1 = AsterFraming.encodeFrame(p1, (byte) 0);
    byte[] f2 = AsterFraming.encodeFrame(p2, AsterFraming.FLAG_TRAILER);

    byte[] combined = new byte[f1.length + f2.length];
    System.arraycopy(f1, 0, combined, 0, f1.length);
    System.arraycopy(f2, 0, combined, f1.length, f2.length);

    ClientFrameReader reader = new ClientFrameReader(scriptedReader(combined));

    ClientFrameReader.Frame a = reader.readFrame().get();
    ClientFrameReader.Frame b = reader.readFrame().get();
    assertArrayEquals(p1, a.payload());
    assertEquals((byte) 0, a.flags());
    assertArrayEquals(p2, b.payload());
    assertEquals(AsterFraming.FLAG_TRAILER, b.flags());
  }

  @Test
  void eofBeforeHeaderCompletes() {
    ClientFrameReader reader = new ClientFrameReader(scriptedReader());

    ExecutionException ex = assertThrows(ExecutionException.class, () -> reader.readFrame().get());
    assertTrue(ex.getCause() instanceof ClientFrameReader.FramingException);
  }

  @Test
  void rejectsOversizedFrame() {
    byte[] badLen = new byte[5];
    // 17 MiB — beyond the 16 MiB max.
    int oversized = 17 * 1024 * 1024;
    badLen[0] = (byte) (oversized & 0xFF);
    badLen[1] = (byte) ((oversized >>> 8) & 0xFF);
    badLen[2] = (byte) ((oversized >>> 16) & 0xFF);
    badLen[3] = (byte) ((oversized >>> 24) & 0xFF);
    badLen[4] = 0;

    ClientFrameReader reader = new ClientFrameReader(scriptedReader(badLen));
    ExecutionException ex = assertThrows(ExecutionException.class, () -> reader.readFrame().get());
    assertTrue(ex.getCause() instanceof ClientFrameReader.FramingException);
    assertTrue(ex.getCause().getMessage().contains("exceeds max"));
  }

  @Test
  void rejectsZeroLengthFrame() {
    byte[] zero = new byte[] {0, 0, 0, 0};
    ClientFrameReader reader = new ClientFrameReader(scriptedReader(zero));
    ExecutionException ex = assertThrows(ExecutionException.class, () -> reader.readFrame().get());
    assertTrue(ex.getCause() instanceof ClientFrameReader.FramingException);
  }
}
