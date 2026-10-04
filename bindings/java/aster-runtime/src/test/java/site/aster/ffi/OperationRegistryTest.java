package site.aster.ffi;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutionException;
import org.junit.jupiter.api.Test;

/** The registration/completion race: the outcome of a fast operation can precede its future. */
final class OperationRegistryTest {

  @Test
  void outcomeThatArrivesBeforeRegistrationIsDeliveredOnRegister() {
    OperationRegistry registry = new OperationRegistry();
    RuntimeException failure = new RuntimeException("early");

    registry.completeExceptionally(7, failure);
    CompletableFuture<?> future = registry.register(7);

    ExecutionException e = assertThrows(ExecutionException.class, future::get);
    assertSame(failure, e.getCause());
    assertEquals(0, registry.size(), "nothing left pending");
  }

  @Test
  void registeredFirstStillCompletesNormally() {
    OperationRegistry registry = new OperationRegistry();
    CompletableFuture<?> future = registry.register(9);
    assertFalse(future.isDone());
    assertEquals(1, registry.size());

    RuntimeException failure = new RuntimeException("late");
    registry.completeExceptionally(9, failure);

    assertSame(failure, assertThrows(ExecutionException.class, future::get).getCause());
    assertEquals(0, registry.size());
  }

  @Test
  void removedOperationDropsItsParkedOutcome() {
    OperationRegistry registry = new OperationRegistry();
    registry.completeExceptionally(11, new RuntimeException("x"));
    registry.remove(11);

    assertFalse(registry.register(11).isDone(), "a cancelled op must not inherit a stale outcome");
  }
}
