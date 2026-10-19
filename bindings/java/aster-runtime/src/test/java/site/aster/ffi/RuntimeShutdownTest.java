package site.aster.ffi;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandles;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import site.aster.config.AsterConfig;
import site.aster.docs.Doc;
import site.aster.docs.DocEvents;
import site.aster.event.IrohEvent;
import site.aster.node.IrohNode;
import site.aster.testing.WaitBudget;

final class RuntimeShutdownTest {
  private static Throwable failure(CompletableFuture<?> future) {
    return assertThrows(
            ExecutionException.class, () -> future.get(WaitBudget.SECONDS, TimeUnit.SECONDS))
        .getCause();
  }

  private static IrohEvent event(long operation, long buffer) {
    return new IrohEvent(
        IrohEventKind.DOC_EVENT, operation, 0, 0, 0, 0, 0, 0, buffer, 0, MemorySegment.NULL);
  }

  @Test
  void closeFailsOutstandingAndFutureRegistrationsIncludingSecondary() {
    var registry = new OperationRegistry();
    var single = registry.register(1);
    var primary = new CompletableFuture<>();
    var secondary = new CompletableFuture<>();
    registry.registerWithSecondary(2, primary, secondary);
    var closed = IrohException.forStatus(IrohStatus.ALREADY_CLOSED, "closed");

    registry.close(closed);
    registry.close(new IllegalStateException("second close"));

    for (var future : List.of(single, primary, secondary, registry.register(3)))
      assertSame(closed, failure(future));
    var laterPrimary = new CompletableFuture<>();
    var laterSecondary = new CompletableFuture<>();
    registry.registerWithSecondary(4, laterPrimary, laterSecondary);
    assertSame(closed, failure(laterPrimary));
    assertSame(closed, failure(laterSecondary));
    assertEquals(0, registry.size());
  }

  @Test
  void closeDiscardsEarlyAndLateOwnedCompletionsOnce() {
    var discarded = new ArrayList<Long>();
    var registry = new OperationRegistry(event -> discarded.add(event.buffer()));
    registry.complete(1, event(1, 101));
    registry.complete(2, event(2, 102));
    registry.close(new IllegalStateException("closed"));
    registry.close(new IllegalStateException("closed again"));
    registry.complete(3, event(3, 103));
    assertEquals(List.of(101L, 102L, 103L), discarded);
    assertInstanceOf(IllegalStateException.class, failure(registry.register(1)));
    assertEquals(0, registry.size());
  }

  @Test
  void discardFailureStillSettlesWaitersAndAttemptsOtherReleases() {
    var discarded = new ArrayList<Long>();
    var releaseFailure = new IllegalStateException("release failed");
    var registry =
        new OperationRegistry(
            event -> {
              discarded.add(event.buffer());
              if (event.buffer() == 101) throw releaseFailure;
            });
    registry.complete(1, event(1, 101));
    registry.complete(2, event(2, 102));
    var waiting = registry.register(3);
    var closed = IrohException.forStatus(IrohStatus.ALREADY_CLOSED, "closed");

    assertSame(
        releaseFailure, assertThrows(IllegalStateException.class, () -> registry.close(closed)));
    assertEquals(List.of(101L, 102L), discarded);
    assertSame(closed, failure(waiting));
    assertSame(closed, failure(registry.register(4)));
  }

  @Test
  void runtimeCloseSettlesIdleAndTimedDocumentReceives() throws Exception {
    IrohNode node =
        IrohNode.start(
                AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build(),
                List.of(),
                null)
            .get(30, TimeUnit.SECONDS);
    try (node;
        Doc doc = node.docs().createAsync().get(30, TimeUnit.SECONDS);
        DocEvents idle = doc.subscribeAsync().get(30, TimeUnit.SECONDS);
        DocEvents timed = doc.subscribeAsync().get(30, TimeUnit.SECONDS)) {
      var idleReceive = idle.nextAsync();
      var timedReceive = timed.nextAsync(Duration.ofSeconds(30));
      node.runtime().close();
      for (var future :
          List.of(idleReceive, timedReceive, node.runtime().registry().register(999))) {
        IrohException closed = assertInstanceOf(IrohException.class, failure(future));
        assertEquals(IrohStatus.ALREADY_CLOSED, closed.status());
      }
      assertEquals(0, node.runtime().registry().size());
      node.runtime().close();
    } finally {
      node.runtime().close();
    }
  }

  @Test
  void selfStopSkipsRemainingBatchAndInboundHandlers() throws Exception {
    for (boolean inbound : List.of(false, true)) {
      try (Arena arena = Arena.ofShared()) {
        var registry = new OperationRegistry();
        long eventSize = IrohLibrary.ASTER_EVENT.byteSize();
        MemorySegment batch = arena.allocate(eventSize * 2);
        batch.set(
            ValueLayout.JAVA_INT,
            4,
            (inbound ? IrohEventKind.FRAME_RECEIVED : IrohEventKind.DOC_EVENT).code);
        batch.set(ValueLayout.JAVA_LONG, 16, 1);
        batch.set(ValueLayout.JAVA_INT, eventSize + 4, IrohEventKind.DOC_EVENT.code);
        batch.set(ValueLayout.JAVA_LONG, eventSize + 16, 2);
        var poll =
            MethodHandles.dropArguments(
                MethodHandles.constant(long.class, 2L),
                0,
                long.class,
                MemorySegment.class,
                long.class,
                int.class);
        var release =
            MethodHandles.dropArguments(
                MethodHandles.constant(int.class, 0), 0, long.class, long.class);
        var poller = new IrohPollThread(registry, 0, 2, Duration.ZERO, batch, poll, release);
        var first = registry.register(1);
        var second = registry.register(2);
        var stopped = new CompletableFuture<Void>();
        var laterHandlers = new AtomicInteger();
        java.util.function.Consumer<IrohEvent> stop =
            event -> {
              poller.stop();
              assertFalse(Thread.currentThread().isInterrupted());
              stopped.complete(null);
            };
        if (inbound) {
          poller.addInboundHandler(stop);
          poller.addInboundHandler(event -> laterHandlers.incrementAndGet());
        } else first.thenAccept(stop);
        try {
          poller.start();
          stopped.get(WaitBudget.SECONDS, TimeUnit.SECONDS);
          poller.stop(); // Wait for the worker to leave the batch before inspecting outcomes.
          assertFalse(second.isDone(), "the rest of a stopped batch must not be accessed");
          assertEquals(0, laterHandlers.get(), "a stopped handler chain must not reuse the event");
        } finally {
          poller.stop();
          registry.close(new IllegalStateException("test closed"));
        }
      }
    }
  }

  @Test
  void receiveContinuationCanCloseItsOwnRuntime() throws Exception {
    IrohNode node =
        IrohNode.start(
                AsterConfig.builder().relayMode("disabled").bindAddr("127.0.0.1:0").build(),
                List.of(),
                null)
            .get(30, TimeUnit.SECONDS);
    try (node;
        Doc doc = node.docs().createAsync().get(30, TimeUnit.SECONDS);
        Doc other = node.docs().createAsync().get(30, TimeUnit.SECONDS);
        DocEvents receiving = doc.subscribeAsync().get(30, TimeUnit.SECONDS);
        DocEvents idle = other.subscribeAsync().get(30, TimeUnit.SECONDS)) {
      var author = node.docs().createAuthorAsync().get(30, TimeUnit.SECONDS);
      var idleReceive = idle.nextAsync();
      var closed =
          receiving
              .nextAsync()
              .thenAccept(
                  event -> {
                    assertEquals("self/close", event.entry().key());
                    assertEquals("iroh-poll", Thread.currentThread().getName());
                    node.runtime().close();
                    assertFalse(Thread.currentThread().isInterrupted());
                  });
      // This write's completion can share a poll batch with the receive that closes the runtime.
      doc.setBytesAsync(author, "self/close", new byte[] {1});
      closed.get(WaitBudget.SECONDS, TimeUnit.SECONDS);
      IrohException error = assertInstanceOf(IrohException.class, failure(idleReceive));
      assertEquals(IrohStatus.ALREADY_CLOSED, error.status());
      assertEquals(0, node.runtime().registry().size());
    } finally {
      node.runtime().close();
    }
  }
}
