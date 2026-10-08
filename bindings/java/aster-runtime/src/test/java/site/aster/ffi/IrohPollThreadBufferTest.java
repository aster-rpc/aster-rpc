package site.aster.ffi;

import static org.junit.jupiter.api.Assertions.*;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletionException;
import java.util.concurrent.atomic.AtomicBoolean;
import org.junit.jupiter.api.Test;
import site.aster.event.IrohEvent;

/** Error completion copies text and consumes its native buffer exactly once. */
final class IrohPollThreadBufferTest {
  @Test
  void errorsAndCanceledOperationsReleaseBorrowedPayloadBeforeCompleting() {
    for (IrohEventKind kind : List.of(IrohEventKind.ERROR, IrohEventKind.OPERATION_CANCELLED)) {
      OperationRegistry registry = new OperationRegistry();
      var pending = registry.register(42);
      List<Long> released = new ArrayList<>();
      byte[] text = "native audit error".getBytes(StandardCharsets.UTF_8);
      try (Arena arena = Arena.ofConfined()) {
        var payload = arena.allocate(text.length);
        payload.copyFrom(MemorySegment.ofArray(text));
        var event = new IrohEvent(kind, 42, 0, 0, 0, -1, 4, 0, 123, text.length, payload);
        AtomicBoolean releasedBeforeCompletion = new AtomicBoolean();
        pending.whenComplete(
            (value, failure) -> releasedBeforeCompletion.set(released.equals(List.of(123L))));
        IrohPollThread.dispatchFailure(registry, event, released::add);
        assertEquals(List.of(123L), released);
        assertTrue(releasedBeforeCompletion.get());
        CompletionException failure = assertThrows(CompletionException.class, pending::join);
        assertInstanceOf(IrohException.class, failure.getCause());
        assertTrue(failure.getCause().getMessage().contains("native audit error"));
        assertEquals(4, ((IrohException) failure.getCause()).errorCode());
      }
    }
  }

  @Test
  void unreadablePayloadStillReleasesAndNoBufferNeedsNoRelease() {
    for (long buffer : List.of(0L, 123L)) {
      OperationRegistry registry = new OperationRegistry();
      var pending = registry.register(42);
      List<Long> released = new ArrayList<>();
      var event =
          new IrohEvent(
              IrohEventKind.ERROR, 42, 0, 0, 0, -1, 0, 0, buffer, 100, MemorySegment.NULL);
      IrohPollThread.dispatchFailure(registry, event, released::add);
      assertEquals(buffer == 0 ? List.of() : List.of(buffer), released);
      assertThrows(CompletionException.class, pending::join);
    }
  }
}
