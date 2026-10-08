package site.aster.client;

import static org.junit.jupiter.api.Assertions.*;

import java.util.concurrent.*;
import org.junit.jupiter.api.Test;
import site.aster.ffi.AsterEnvelope;

class ResponseBufferTest {
  @Test
  void zeroByteItemsAreBoundedAndCancelWakesWriter() throws Exception {
    Object end = new Object();
    ResponseBuffer buffer = new ResponseBuffer(end);
    for (int i = 0; i < 256; i++) {
      buffer.beginRead();
      buffer.awaitCapacity(0);
      buffer.data(i, 0);
    }
    assertEquals(256, buffer.size());
    assertEquals(0, buffer.retainedBytes());
    CountDownLatch started = new CountDownLatch(1);
    CompletableFuture<Boolean> stopped = new CompletableFuture<>();
    Thread worker =
        Thread.ofPlatform()
            .start(
                () -> {
                  started.countDown();
                  try {
                    buffer.beginRead();
                    stopped.complete(false);
                  } catch (InterruptedException expected) {
                    stopped.complete(true);
                  }
                });
    try {
      assertTrue(started.await(1, TimeUnit.SECONDS));
      buffer.cancel();
      assertTrue(stopped.get(2, TimeUnit.SECONDS));
    } finally {
      buffer.cancel();
      worker.interrupt();
      worker.join(2000);
    }
    assertFalse(worker.isAlive());
    buffer.finish();
    assertSame(end, buffer.take());
  }

  @Test
  void legalMaximumFrameWaitsBeforeDecodeAndFitsEmptyBuffer() throws Exception {
    ResponseBuffer buffer = new ResponseBuffer(new Object());
    int small = 7 * 1024 * 1024, large = AsterEnvelope.MAX_DECOMPRESSED_SIZE;
    buffer.beginRead();
    buffer.awaitCapacity(small);
    buffer.data("small", small);
    CompletableFuture<Void> published = new CompletableFuture<>();
    Thread worker =
        Thread.ofPlatform()
            .start(
                () -> {
                  try {
                    buffer.beginRead();
                    buffer.awaitCapacity(large);
                    buffer.data("large", large);
                    published.complete(null);
                  } catch (Throwable error) {
                    published.completeExceptionally(error);
                  }
                });
    try {
      long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
      while (buffer.retainedBytes() != small + (long) large && System.nanoTime() < deadline)
        Thread.sleep(1);
      assertEquals(small + (long) large, buffer.retainedBytes());
      assertTrue(buffer.retainedBytes() <= ResponseBuffer.MAX_RETAINED_BYTES);
      assertFalse(published.isDone(), "capacity is acquired before decode/publication");
      assertEquals("small", buffer.take());
      published.get(2, TimeUnit.SECONDS);
      assertEquals(large, buffer.retainedBytes());
      assertEquals("large", buffer.take());
      assertEquals(0, buffer.retainedBytes());
    } finally {
      buffer.cancel();
      worker.interrupt();
      worker.join(2000);
    }
    assertFalse(worker.isAlive());
  }

  @Test
  void terminalDoesNotNeedDataCapacityAndRemainsTerminal() throws Exception {
    Object end = new Object();
    ResponseBuffer buffer = new ResponseBuffer(end);
    for (int i = 0; i < 256; i++) {
      buffer.beginRead();
      buffer.awaitCapacity(0);
      buffer.data(i, 0);
    }
    buffer.finish();
    for (int i = 0; i < 256; i++) assertEquals(i, buffer.take());
    assertSame(end, buffer.take());
    assertSame(end, buffer.take());
  }
}
