package site.aster.ffi;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;

/** Deterministic Java-side contract for native response backpressure. */
final class ReactorBackpressureContractTest {

  @Test
  void queueFullWaitsThenRetriesTheSameSubmission() throws Throwable {
    AtomicInteger attempts = new AtomicInteger();
    AtomicInteger waits = new AtomicInteger();
    Reactor.resetQueueFullStats();

    Reactor.retryQueueFull(
        "test-submit",
        () -> attempts.getAndIncrement() < 2 ? IrohStatus.QUEUE_FULL.code : IrohStatus.OK.code,
        waits::incrementAndGet);

    assertEquals(3, attempts.get(), "two full results followed by the exact retry");
    assertEquals(2, waits.get(), "each full result must wait for one capacity grant");
    assertEquals(2, Reactor.queueFullHits());
  }

  @Test
  void aClosedCallIsTerminalRatherThanRetriedAsFull() {
    AtomicInteger attempts = new AtomicInteger();
    AtomicInteger waits = new AtomicInteger();

    assertThrows(
        IrohException.class,
        () ->
            Reactor.retryQueueFull(
                "test-submit",
                () -> {
                  attempts.incrementAndGet();
                  return IrohStatus.NOT_FOUND.code;
                },
                waits::incrementAndGet));

    assertEquals(1, attempts.get());
    assertEquals(0, waits.get());
  }
}
